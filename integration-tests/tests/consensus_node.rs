use std::time::Duration;

use alloy::primitives::U256;
use alloy::providers::WalletProvider;
use alloy::rpc::types::TransactionRequest;
use tokio::time::{Instant, sleep};
use zksync_os_integration_tests::assert_traits::ReceiptAssert;
use zksync_os_integration_tests::multi_node::MultiNodeTester;

fn consensus_1_nodes_test_keys() -> Vec<zksync_os_network::SecretKey> {
    vec![zksync_os_network::rng_secret_key()]
}

fn consensus_3_nodes_test_keys() -> Vec<zksync_os_network::SecretKey> {
    (0..3)
        .map(|_| zksync_os_network::rng_secret_key())
        .collect()
}

async fn raft_status(
    cluster: &MultiNodeTester,
    index: usize,
) -> anyhow::Result<zksync_os_status_server::StatusResponse> {
    cluster.node(index).status().await.map_err(Into::into)
}

fn raft_node_id(
    status: &zksync_os_status_server::StatusResponse,
    index: usize,
) -> anyhow::Result<String> {
    status
        .consensus
        .raft
        .as_ref()
        .map(|raft| raft.node_id.clone())
        .ok_or_else(|| anyhow::anyhow!("node {index} did not expose raft status"))
}

fn raft_last_applied(
    status: &zksync_os_status_server::StatusResponse,
    index: usize,
) -> anyhow::Result<Option<u64>> {
    status
        .consensus
        .raft
        .as_ref()
        .map(|raft| raft.last_applied_index)
        .ok_or_else(|| anyhow::anyhow!("node {index} did not expose raft status"))
}

/// Send a simple self-transfer on the given node and wait for the receipt.
async fn send_transfer(
    cluster: &MultiNodeTester,
    index: usize,
) -> anyhow::Result<alloy::rpc::types::TransactionReceipt> {
    cluster.node(index).wait_for_initial_deposit().await?;
    let self_addr = cluster.node(index).l2_provider.default_signer_address();
    cluster
        .node(index)
        .l2_provider
        .send_transaction(
            TransactionRequest::default()
                .with_to(self_addr)
                .with_value(U256::from(1u64)),
        )
        .await?
        .expect_successful_receipt()
        .await
        .map_err(Into::into)
}

async fn wait_for_node_last_applied_index_at_or_above(
    cluster: &MultiNodeTester,
    index: usize,
    min_index: u64,
    timeout: Duration,
) -> anyhow::Result<u64> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(status) = raft_status(cluster, index).await {
            if let Some(last_applied) = raft_last_applied(&status, index)? {
                if last_applied >= min_index {
                    return Ok(last_applied);
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    anyhow::bail!("timed out waiting for node {index} to reach last_applied_index >= {min_index}")
}

async fn active_max_last_applied_index(cluster: &MultiNodeTester) -> anyhow::Result<u64> {
    let mut max_last_applied = 0;
    for (idx, node) in cluster.nodes.iter().enumerate() {
        if node.is_suspended() {
            continue;
        }
        max_last_applied = max_last_applied
            .max(raft_last_applied(&raft_status(cluster, idx).await?, idx)?.unwrap_or(0));
    }
    Ok(max_last_applied)
}

async fn send_transfer_and_wait_for_active_replication(
    cluster: &MultiNodeTester,
    leader_index: usize,
) -> anyhow::Result<u64> {
    let initial_applied = active_max_last_applied_index(cluster).await?;
    send_transfer(cluster, leader_index).await?;
    cluster
        .wait_for_active_last_applied_index_convergence(
            initial_applied + 1,
            Duration::from_secs(20),
        )
        .await
}

/// Send a transaction, wait for replication, and verify all nodes see the same block.
async fn send_transfer_and_verify_consistent_view(
    cluster: &MultiNodeTester,
    leader_index: usize,
) -> anyhow::Result<u64> {
    let replicated = send_transfer_and_wait_for_active_replication(cluster, leader_index).await?;
    cluster
        .wait_for_consistent_cluster_view(Duration::from_secs(20))
        .await?;
    Ok(replicated)
}

#[test_log::test(tokio::test)]
async fn consensus_cluster_includes_simple_transaction_with_wait() -> anyhow::Result<()> {
    let cluster = MultiNodeTester::builder()
        .with_consensus_secret_keys(consensus_1_nodes_test_keys())
        .build()
        .await?;
    let result = async {
        let leader_index = cluster
            .wait_for_raft_cluster_formation(Duration::from_secs(15))
            .await?;

        send_transfer(&cluster, leader_index).await?;

        Ok(())
    }
    .await;
    let shutdown_result = cluster.shutdown_all().await;
    result.and(shutdown_result)
}

#[test_log::test(tokio::test)]
async fn consensus_cluster_forms_with_three_nodes_and_replicates_blocks() -> anyhow::Result<()> {
    let cluster = MultiNodeTester::builder()
        .with_consensus_secret_keys(consensus_3_nodes_test_keys())
        .build()
        .await?;
    let result = async {
        let leader_index = cluster
            .wait_for_raft_cluster_formation(Duration::from_secs(20))
            .await?;

        // Send a transaction and verify all nodes converge on the same block view.
        let initial_applied = active_max_last_applied_index(&cluster).await?;
        let replicated_applied =
            send_transfer_and_verify_consistent_view(&cluster, leader_index).await?;
        assert!(replicated_applied >= initial_applied + 1);

        Ok(())
    }
    .await;
    let shutdown_result = cluster.shutdown_all().await;
    result.and(shutdown_result)
}

#[test_log::test(tokio::test)]
async fn consensus_cluster_rotates_leader_after_failure() -> anyhow::Result<()> {
    let mut cluster = MultiNodeTester::builder()
        .with_consensus_secret_keys(consensus_3_nodes_test_keys())
        .build()
        .await?;
    let result = async {
        let initial_leader_idx = cluster
            .wait_for_raft_cluster_formation(Duration::from_secs(20))
            .await?;
        let initial_leader_node_id = raft_node_id(
            &raft_status(&cluster, initial_leader_idx).await?,
            initial_leader_idx,
        )?;

        // Verify transaction works before suspending.
        send_transfer_and_verify_consistent_view(&cluster, initial_leader_idx).await?;

        // Suspend the leader.
        cluster.suspend_node(initial_leader_idx).await;

        // New leader should be elected.
        let new_leader_idx = cluster
            .wait_for_active_raft_cluster_formation(Duration::from_secs(20))
            .await?;
        let new_leader_id = raft_node_id(
            &raft_status(&cluster, new_leader_idx).await?,
            new_leader_idx,
        )?;
        assert_ne!(initial_leader_node_id, new_leader_id);

        // Verify transaction works under new leader and nodes agree on state.
        send_transfer_and_verify_consistent_view(&cluster, new_leader_idx).await?;

        Ok(())
    }
    .await;
    let shutdown_result = cluster.shutdown_all().await;
    result.and(shutdown_result)
}

#[test_log::test(tokio::test)]
async fn consensus_cluster_stops_making_progress_without_quorum() -> anyhow::Result<()> {
    let mut cluster = MultiNodeTester::builder()
        .with_consensus_secret_keys(consensus_3_nodes_test_keys())
        .build()
        .await?;
    let result = async {
        let leader_idx = cluster
            .wait_for_raft_cluster_formation(Duration::from_secs(20))
            .await?;

        // Verify transaction works and all nodes agree before quorum loss.
        let committed_applied =
            send_transfer_and_verify_consistent_view(&cluster, leader_idx).await?;

        let follower_indices: Vec<_> = (0..cluster.nodes.len())
            .filter(|idx| *idx != leader_idx)
            .collect();
        let survivor_idx = follower_indices[1];
        cluster
            .node(survivor_idx)
            .wait_for_initial_deposit()
            .await?;

        // Suspend leader + one follower → lose quorum.
        cluster.suspend_node(leader_idx).await;
        cluster.suspend_node(follower_indices[0]).await;

        let survivor_applied =
            raft_last_applied(&raft_status(&cluster, survivor_idx).await?, survivor_idx)?
                .unwrap_or(0);
        sleep(Duration::from_secs(2)).await;
        let survivor_applied_after_wait =
            raft_last_applied(&raft_status(&cluster, survivor_idx).await?, survivor_idx)?
                .unwrap_or(0);
        assert!(
            survivor_applied <= committed_applied,
            "last_applied unexpectedly advanced before quorum-loss check: committed={committed_applied} survivor={survivor_applied}"
        );
        assert!(
            survivor_applied_after_wait <= survivor_applied,
            "last_applied unexpectedly advanced after quorum loss: before={survivor_applied} after={survivor_applied_after_wait}"
        );

        Ok(())
    }
    .await;
    let shutdown_result = cluster.shutdown_all().await;
    result.and(shutdown_result)
}

#[test_log::test(tokio::test)]
async fn consensus_follower_restarts_and_catches_up() -> anyhow::Result<()> {
    let mut cluster = MultiNodeTester::builder()
        .with_consensus_secret_keys(consensus_3_nodes_test_keys())
        .build()
        .await?;
    let result = async {
        let leader_idx = cluster
            .wait_for_raft_cluster_formation(Duration::from_secs(20))
            .await?;

        // Verify transaction works before suspending the follower.
        send_transfer_and_verify_consistent_view(&cluster, leader_idx).await?;

        let follower_idx = (0..cluster.nodes.len())
            .find(|idx| *idx != leader_idx)
            .expect("3-node cluster must have a follower");

        // Suspend one follower.
        cluster.suspend_node(follower_idx).await;
        let active_leader_idx = cluster
            .wait_for_active_raft_cluster_formation(Duration::from_secs(20))
            .await?;

        // Do transactions while the follower is down.
        send_transfer(&cluster, active_leader_idx).await?;
        send_transfer(&cluster, active_leader_idx).await?;
        let target_applied = cluster
            .wait_for_active_last_applied_index_convergence(1, Duration::from_secs(20))
            .await?;

        // Restart the follower and wait for it to catch up.
        cluster.start_node(follower_idx).await?;
        wait_for_node_last_applied_index_at_or_above(
            &cluster,
            follower_idx,
            target_applied,
            Duration::from_secs(20),
        )
        .await?;

        // Verify all nodes (including restarted follower) agree on the same block view.
        cluster
            .wait_for_consistent_cluster_view(Duration::from_secs(20))
            .await?;

        Ok(())
    }
    .await;
    let shutdown_result = cluster.shutdown_all().await;
    result.and(shutdown_result)
}
