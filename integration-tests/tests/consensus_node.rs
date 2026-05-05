use alloy::eips::BlockId;
use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use anyhow::Context as _;
use std::time::Duration;
use tokio::time::{Instant, sleep};
use zksync_os_integration_tests::Tester;
use zksync_os_integration_tests::assert_traits::ReceiptAssert;
use zksync_os_integration_tests::multi_node::MultiNodeTester;
use zksync_os_integration_tests::provider::ZksyncTestingProvider;
use zksync_os_integration_tests::rpc_recorder::{HttpRpcRecorder, HttpRpcReport, RpcRecordConfig};

const CLUSTER_FORMATION_TIMEOUT: Duration = Duration::from_secs(20);
const REPLICATION_TIMEOUT: Duration = Duration::from_secs(20);
const L1_FINALIZATION_TIMEOUT: Duration = Duration::from_secs(60);
const CONSENSUS_LONG_GAP_LOAD_DURATION: Duration = Duration::from_secs(60);
const CONSENSUS_CONTINUED_LOAD_AFTER_RESTART_DURATION: Duration = Duration::from_secs(45);
const CONSENSUS_LONG_GAP_CATCH_UP_TIMEOUT: Duration = Duration::from_secs(180);
const MIN_LONG_GAP_LOAD_BLOCKS: usize = 10;
const MIN_CONTINUED_LOAD_BLOCKS: usize = 5;

fn consensus_test_keys(n: usize) -> Vec<zksync_os_network::SecretKey> {
    (0..n)
        .map(|_| zksync_os_network::rng_secret_key())
        .collect()
}

async fn raft_status(
    cluster: &MultiNodeTester,
    index: usize,
) -> anyhow::Result<zksync_os_status_server::StatusResponse> {
    cluster.node(index).status().await
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

async fn send_transfer(
    cluster: &MultiNodeTester,
    index: usize,
) -> anyhow::Result<alloy::rpc::types::TransactionReceipt> {
    let node = cluster.node(index);
    send_transfer_to_node(node).await
}

async fn send_transfer_to_node(
    node: &Tester,
) -> anyhow::Result<alloy::rpc::types::TransactionReceipt> {
    let gas_price = node.l2_provider.get_gas_price().await?;
    let tx = TransactionRequest::default()
        .with_to(Address::random())
        .with_value(U256::from(1))
        .with_gas_price(gas_price);
    node.l2_provider
        .send_transaction(tx)
        .await?
        .expect_successful_receipt()
        .await
}

async fn latest_l2_block(node: &Tester) -> anyhow::Result<u64> {
    node.l2_zk_provider
        .get_block_number_by_id(BlockId::latest())
        .await?
        .context("latest block number is missing")
}

async fn wait_for_l2_block(
    node: &Tester,
    block_number: u64,
    timeout: Duration,
) -> anyhow::Result<()> {
    tokio::time::timeout(timeout, node.l2_zk_provider.wait_for_block(block_number))
        .await
        .with_context(|| format!("timed out waiting for L2 block {block_number}"))??;
    Ok(())
}

async fn wait_for_raft_leader_among(
    cluster: &MultiNodeTester,
    node_indices: &[usize],
    timeout: Duration,
) -> anyhow::Result<usize> {
    let deadline = Instant::now() + timeout;
    let mut last_summary = String::new();

    while Instant::now() < deadline {
        let mut statuses = Vec::with_capacity(node_indices.len());
        let mut errors = Vec::new();

        for &index in node_indices {
            match raft_status(cluster, index).await {
                Ok(status) => statuses.push((index, status)),
                Err(error) => errors.push(format!("node_{index}: {error:#}")),
            }
        }

        let summary = {
            let leaders = statuses
                .iter()
                .filter_map(|(index, status)| {
                    status
                        .consensus
                        .raft
                        .as_ref()
                        .filter(|raft| raft.is_leader)
                        .map(|raft| format!("node_{index}:{}", raft.node_id))
                })
                .collect::<Vec<_>>();
            let leader_views = statuses
                .iter()
                .filter_map(|(index, status)| {
                    status
                        .consensus
                        .raft
                        .as_ref()?
                        .current_leader
                        .as_ref()
                        .map(|leader| format!("node_{index}:{leader}"))
                })
                .collect::<Vec<_>>();
            format!(
                "statuses={} errors=[{}] leaders=[{}] views=[{}]",
                statuses.len(),
                errors.join(", "),
                leaders.join(", "),
                leader_views.join(", ")
            )
        };
        if summary != last_summary {
            tracing::info!("raft quorum leader check: {summary}");
            last_summary = summary;
        }

        if statuses.len() == node_indices.len()
            && statuses.iter().all(|(_, status)| status.healthy)
            && let Some(agreed_leader) = statuses
                .first()
                .and_then(|(_, status)| status.consensus.raft.as_ref()?.current_leader.clone())
            && statuses.iter().all(|(_, status)| {
                status
                    .consensus
                    .raft
                    .as_ref()
                    .and_then(|raft| raft.current_leader.as_deref())
                    == Some(agreed_leader.as_str())
            })
            && let Some((leader_index, _)) = statuses.iter().find(|(_, status)| {
                status
                    .consensus
                    .raft
                    .as_ref()
                    .is_some_and(|raft| raft.is_leader && raft.node_id == agreed_leader)
            })
        {
            return Ok(*leader_index);
        }

        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    anyhow::bail!("timed out waiting for raft leader among {node_indices:?}: {last_summary}")
}

async fn send_transfer_and_wait_for_l2_blocks(
    cluster: &MultiNodeTester,
    leader_index: usize,
    node_indices: &[usize],
) -> anyhow::Result<u64> {
    let receipt = send_transfer(cluster, leader_index).await?;
    let block_number = receipt
        .block_number
        .context("transfer receipt did not include a block number")?;
    for &index in node_indices {
        wait_for_l2_block(cluster.node(index), block_number, REPLICATION_TIMEOUT)
            .await
            .with_context(|| format!("node {index} did not reach L2 block {block_number}"))?;
    }
    Ok(block_number)
}

struct ConsensusLoadStats {
    attempts: usize,
    blocks: Vec<u64>,
    elapsed: Duration,
}

struct ConsensusRejoinLoadStats {
    attempts: usize,
    blocks: Vec<u64>,
    blocks_before_restart: usize,
    blocks_after_restart: usize,
    elapsed: Duration,
    restart_started_at: Duration,
    restart_completed_at: Duration,
    target_block_at_restart: u64,
    target_applied_at_restart: u64,
    raft_caught_up_index: u64,
    raft_caught_up_at: Duration,
    l2_caught_up_at: Duration,
    rpc_caught_up_at: Duration,
}

fn remaining_storm_send_timeout(deadline: Instant) -> Option<Duration> {
    let now = Instant::now();
    if now >= deadline {
        return None;
    }

    Some((REPLICATION_TIMEOUT + Duration::from_secs(10)).min(deadline.duration_since(now)))
}

async fn generate_consensus_transaction_storm(
    cluster: &MultiNodeTester,
    node_indices: &[usize],
    duration: Duration,
) -> anyhow::Result<ConsensusLoadStats> {
    let started_at = Instant::now();
    let deadline = started_at + duration;
    let mut attempts = 0;
    let mut blocks = Vec::new();
    let mut last_error = None;

    while Instant::now() < deadline {
        let leader_index =
            wait_for_raft_leader_among(cluster, node_indices, CLUSTER_FORMATION_TIMEOUT).await?;
        let Some(send_timeout) = remaining_storm_send_timeout(deadline) else {
            break;
        };
        attempts += 1;

        match tokio::time::timeout(
            send_timeout,
            send_transfer_and_wait_for_l2_blocks(cluster, leader_index, node_indices),
        )
        .await
        {
            Ok(Ok(block_number)) => {
                tracing::info!(
                    attempts,
                    produced_blocks = blocks.len() + 1,
                    block_number,
                    elapsed_ms = started_at.elapsed().as_millis(),
                    "consensus transaction storm produced a block"
                );
                blocks.push(block_number);
            }
            Ok(Err(error)) => {
                tracing::warn!(
                    attempts,
                    error = %error,
                    "consensus transaction storm send failed; retrying"
                );
                last_error = Some(error.to_string());
                sleep(Duration::from_millis(200)).await;
            }
            Err(_) => {
                tracing::warn!(
                    attempts,
                    "consensus transaction storm send timed out; retrying"
                );
                last_error = Some("timed out sending transfer".to_owned());
                sleep(Duration::from_millis(200)).await;
            }
        }
    }

    anyhow::ensure!(
        blocks.len() >= MIN_LONG_GAP_LOAD_BLOCKS,
        "transaction storm produced too few blocks: produced={}, attempts={}, last_error={:?}",
        blocks.len(),
        attempts,
        last_error
    );

    Ok(ConsensusLoadStats {
        attempts,
        blocks,
        elapsed: started_at.elapsed(),
    })
}

async fn observe_restarted_node_catch_up_to_target(
    cluster: &MultiNodeTester,
    restarted_node_idx: usize,
    restarted_rpc_monitor: &HttpRpcRecorder,
    target_block: u64,
    target_applied: u64,
    started_at: Instant,
    raft_caught_up: &mut Option<(u64, Duration)>,
    l2_caught_up: &mut Option<Duration>,
    rpc_caught_up: &mut Option<Duration>,
) {
    if raft_caught_up.is_none()
        && let Ok(Some(last_applied)) = node_last_applied_index(cluster, restarted_node_idx).await
        && last_applied >= target_applied
    {
        *raft_caught_up = Some((last_applied, started_at.elapsed()));
    }

    if l2_caught_up.is_none()
        && let Ok(latest_block) = latest_l2_block(cluster.node(restarted_node_idx)).await
        && latest_block >= target_block
    {
        *l2_caught_up = Some(started_at.elapsed());
    }

    if rpc_caught_up.is_none()
        && restarted_rpc_monitor
            .first_observed_block_at(target_block)
            .await
            .is_some()
    {
        *rpc_caught_up = Some(started_at.elapsed());
    }
}

async fn generate_consensus_transaction_storm_across_restart(
    cluster: &mut MultiNodeTester,
    active_node_indices: &[usize],
    restarted_node_idx: usize,
    restart_after: Duration,
    continue_after_restart: Duration,
    restarted_rpc_monitor: &HttpRpcRecorder,
) -> anyhow::Result<ConsensusRejoinLoadStats> {
    let started_at = Instant::now();
    let restart_deadline = started_at + restart_after;
    let stop_deadline = restart_deadline + continue_after_restart;
    let mut attempts = 0;
    let mut blocks = Vec::new();
    let mut blocks_before_restart = 0;
    let mut blocks_after_restart = 0;
    let mut restarted = false;
    let mut restart_started_at = None;
    let mut restart_completed_at = None;
    let mut target_block_at_restart = None;
    let mut target_applied_at_restart = None;
    let mut raft_caught_up = None;
    let mut l2_caught_up = None;
    let mut rpc_caught_up = None;
    let mut last_error = None;

    while Instant::now() < stop_deadline {
        if !restarted && Instant::now() >= restart_deadline {
            let target_block = latest_l2_block(cluster.node(active_node_indices[0])).await?;
            let target_applied = active_max_last_applied_index(cluster).await?;
            let started = started_at.elapsed();
            tracing::info!(
                restarted_node_idx,
                target_block,
                target_applied,
                elapsed_ms = started.as_millis(),
                "restarting consensus node while transaction storm continues"
            );
            cluster.start_node(restarted_node_idx).await?;
            let completed = started_at.elapsed();

            restarted = true;
            restart_started_at = Some(started);
            restart_completed_at = Some(completed);
            target_block_at_restart = Some(target_block);
            target_applied_at_restart = Some(target_applied);

            observe_restarted_node_catch_up_to_target(
                cluster,
                restarted_node_idx,
                restarted_rpc_monitor,
                target_block,
                target_applied,
                started_at,
                &mut raft_caught_up,
                &mut l2_caught_up,
                &mut rpc_caught_up,
            )
            .await;
        }

        if restarted {
            observe_restarted_node_catch_up_to_target(
                cluster,
                restarted_node_idx,
                restarted_rpc_monitor,
                target_block_at_restart.expect("target block set after restart"),
                target_applied_at_restart.expect("target applied set after restart"),
                started_at,
                &mut raft_caught_up,
                &mut l2_caught_up,
                &mut rpc_caught_up,
            )
            .await;
        }

        let leader_index =
            wait_for_raft_leader_among(cluster, active_node_indices, CLUSTER_FORMATION_TIMEOUT)
                .await?;
        let Some(send_timeout) = remaining_storm_send_timeout(stop_deadline) else {
            break;
        };
        attempts += 1;

        match tokio::time::timeout(
            send_timeout,
            send_transfer_and_wait_for_l2_blocks(cluster, leader_index, active_node_indices),
        )
        .await
        {
            Ok(Ok(block_number)) => {
                if restarted {
                    blocks_after_restart += 1;
                } else {
                    blocks_before_restart += 1;
                }
                tracing::info!(
                    attempts,
                    produced_blocks = blocks.len() + 1,
                    blocks_before_restart,
                    blocks_after_restart,
                    block_number,
                    elapsed_ms = started_at.elapsed().as_millis(),
                    "continuous consensus transaction storm produced a block"
                );
                blocks.push(block_number);
            }
            Ok(Err(error)) => {
                tracing::warn!(
                    attempts,
                    error = %error,
                    "continuous consensus transaction storm send failed; retrying"
                );
                last_error = Some(error.to_string());
                sleep(Duration::from_millis(200)).await;
            }
            Err(_) => {
                tracing::warn!(
                    attempts,
                    "continuous consensus transaction storm send timed out; retrying"
                );
                last_error = Some("timed out sending transfer".to_owned());
                sleep(Duration::from_millis(200)).await;
            }
        }

        if restarted {
            observe_restarted_node_catch_up_to_target(
                cluster,
                restarted_node_idx,
                restarted_rpc_monitor,
                target_block_at_restart.expect("target block set after restart"),
                target_applied_at_restart.expect("target applied set after restart"),
                started_at,
                &mut raft_caught_up,
                &mut l2_caught_up,
                &mut rpc_caught_up,
            )
            .await;
        }
    }

    if restarted {
        observe_restarted_node_catch_up_to_target(
            cluster,
            restarted_node_idx,
            restarted_rpc_monitor,
            target_block_at_restart.expect("target block set after restart"),
            target_applied_at_restart.expect("target applied set after restart"),
            started_at,
            &mut raft_caught_up,
            &mut l2_caught_up,
            &mut rpc_caught_up,
        )
        .await;
    }

    anyhow::ensure!(
        restarted,
        "transaction storm ended before restarting node {restarted_node_idx}"
    );
    anyhow::ensure!(
        blocks_before_restart >= MIN_LONG_GAP_LOAD_BLOCKS,
        "transaction storm produced too few blocks before restart: produced={}, attempts={}, last_error={:?}",
        blocks_before_restart,
        attempts,
        last_error
    );
    anyhow::ensure!(
        blocks_after_restart >= MIN_CONTINUED_LOAD_BLOCKS,
        "transaction storm produced too few blocks after restart: produced={}, attempts={}, last_error={:?}",
        blocks_after_restart,
        attempts,
        last_error
    );

    let (raft_caught_up_index, raft_caught_up_at) = raft_caught_up
        .context("restarted node did not Raft-apply the restart target while load continued")?;
    let l2_caught_up_at = l2_caught_up.context(
        "restarted node did not expose the restart target L2 block while load continued",
    )?;
    let rpc_caught_up_at = rpc_caught_up
        .context("RPC monitor did not observe the restarted node reaching the restart target")?;

    Ok(ConsensusRejoinLoadStats {
        attempts,
        blocks,
        blocks_before_restart,
        blocks_after_restart,
        elapsed: started_at.elapsed(),
        restart_started_at: restart_started_at.expect("restart started"),
        restart_completed_at: restart_completed_at.expect("restart completed"),
        target_block_at_restart: target_block_at_restart.expect("target block set"),
        target_applied_at_restart: target_applied_at_restart.expect("target applied set"),
        raft_caught_up_index,
        raft_caught_up_at,
        l2_caught_up_at,
        rpc_caught_up_at,
    })
}

fn first_rpc_observed_block_at(report: &HttpRpcReport, target_block: u64) -> Option<Duration> {
    report
        .samples
        .iter()
        .find(|sample| {
            sample
                .block_number
                .is_some_and(|block| block >= target_block)
        })
        .map(|sample| sample.elapsed)
}

fn assert_rpc_monitor_stayed_ready(report: &HttpRpcReport) -> anyhow::Result<()> {
    report.assert_eventually_ready()?;
    anyhow::ensure!(
        report.error_samples() == 0,
        "{} observed RPC errors while it should have stayed ready: {report}\n{}",
        report.name,
        report.format_detailed_timeline()
    );
    Ok(())
}

fn assert_rpc_monitor_recovered_after_outage(report: &HttpRpcReport) -> anyhow::Result<()> {
    let first_error_at = report.first_error_at().with_context(|| {
        format!(
            "{} did not observe the expected RPC outage while the node was stopped: {report}",
            report.name
        )
    })?;
    let first_ready_at = report.first_ready_at().with_context(|| {
        format!(
            "{} never recovered after the expected RPC outage: {report}",
            report.name
        )
    })?;

    anyhow::ensure!(
        first_error_at < first_ready_at,
        "{} observed readiness before the expected outage: {report}\n{}",
        report.name,
        report.format_detailed_timeline()
    );
    Ok(())
}

async fn wait_for_rpc_monitor_block(
    recorder: &HttpRpcRecorder,
    target_block: u64,
    timeout: Duration,
) -> anyhow::Result<Duration> {
    let deadline = Instant::now() + timeout;
    let mut last_observed = None;

    while Instant::now() < deadline {
        if let Some(observed_at) = recorder.first_observed_block_at(target_block).await {
            return Ok(observed_at);
        }

        last_observed = recorder.latest_block_number().await;
        sleep(Duration::from_millis(50)).await;
    }

    anyhow::bail!(
        "timed out waiting for RPC monitor to observe block >= {target_block}: last_observed={last_observed:?}"
    )
}

async fn wait_for_node_last_applied_index_at_or_above(
    cluster: &MultiNodeTester,
    index: usize,
    min_index: u64,
    timeout: Duration,
) -> anyhow::Result<u64> {
    let deadline = Instant::now() + timeout;
    let mut last_observed = None;
    let mut last_error = None;
    while Instant::now() < deadline {
        match raft_status(cluster, index).await {
            Ok(status) => {
                last_observed = raft_last_applied(&status, index)?;
                if let Some(last_applied) = last_observed
                    && last_applied >= min_index
                {
                    return Ok(last_applied);
                }
            }
            Err(error) => {
                last_error = Some(error.to_string());
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    anyhow::bail!(
        "timed out waiting for node {index} to reach last_applied_index >= {min_index}: last_observed={last_observed:?}, last_error={last_error:?}"
    )
}

async fn node_last_applied_index(
    cluster: &MultiNodeTester,
    index: usize,
) -> anyhow::Result<Option<u64>> {
    raft_last_applied(&raft_status(cluster, index).await?, index)
}

async fn l2_block_snapshot(cluster: &MultiNodeTester, node_indices: &[usize]) -> Vec<String> {
    let mut snapshot = Vec::with_capacity(node_indices.len());
    for &index in node_indices {
        match latest_l2_block(cluster.node(index)).await {
            Ok(block) => snapshot.push(format!("node_{index}: block={block}")),
            Err(error) => snapshot.push(format!("node_{index}: block_error={error:#}")),
        }
    }
    snapshot
}

async fn raft_last_applied_snapshot(
    cluster: &MultiNodeTester,
    node_indices: &[usize],
) -> Vec<String> {
    let mut snapshot = Vec::with_capacity(node_indices.len());
    for &index in node_indices {
        match node_last_applied_index(cluster, index).await {
            Ok(last_applied) => {
                snapshot.push(format!("node_{index}: last_applied={last_applied:?}"))
            }
            Err(error) => snapshot.push(format!("node_{index}: raft_error={error:#}")),
        }
    }
    snapshot
}

async fn active_max_last_applied_index(cluster: &MultiNodeTester) -> anyhow::Result<u64> {
    let mut max_last_applied = 0;
    for idx in 0..cluster.len() {
        if cluster.is_node_suspended(idx) {
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
    let receipt = send_transfer(cluster, leader_index).await?;
    let replicated_applied = cluster
        .wait_for_active_last_applied_index_convergence(initial_applied + 1, REPLICATION_TIMEOUT)
        .await?;
    let block_number = receipt
        .block_number
        .context("transfer receipt did not include a block number")?;
    wait_for_l1_finalization_if_batcher_active(cluster, block_number).await?;
    Ok(replicated_applied)
}

async fn wait_for_l1_finalization_if_batcher_active(
    cluster: &MultiNodeTester,
    block_number: u64,
) -> anyhow::Result<u64> {
    let batcher_idx = cluster.batcher_node_index();
    if cluster.is_node_suspended(batcher_idx) {
        tracing::info!(
            block_number,
            batcher_idx,
            "skipping L1 finalization check because the batcher node is suspended"
        );
        return Ok(block_number);
    }

    cluster
        .node(batcher_idx)
        .l2_zk_provider
        .wait_finalized_with_timeout(block_number, L1_FINALIZATION_TIMEOUT)
        .await
        .with_context(|| {
            format!(
                "block {block_number} was not finalized while batcher node {batcher_idx} was active"
            )
        })?;
    Ok(block_number)
}

#[test_log::test(tokio::test)]
async fn consensus_cluster_includes_simple_transaction_with_wait() -> anyhow::Result<()> {
    let cluster = MultiNodeTester::builder()
        .with_consensus_secret_keys(consensus_test_keys(1))
        .build()
        .await?;
    let result = async {
        let leader_index = cluster
            .wait_for_raft_cluster_formation(CLUSTER_FORMATION_TIMEOUT)
            .await?;

        let receipt = send_transfer(&cluster, leader_index).await?;
        let block_number = receipt
            .block_number
            .context("transfer receipt did not include a block number")?;
        wait_for_l1_finalization_if_batcher_active(&cluster, block_number).await?;

        Ok(())
    }
    .await;
    let shutdown_result = cluster.shutdown_all().await;
    result.and(shutdown_result)
}

#[test_log::test(tokio::test)]
async fn consensus_can_be_reenabled_after_clearing_raft_history() -> anyhow::Result<()> {
    let mut cluster = MultiNodeTester::builder()
        .with_consensus_secret_keys(consensus_test_keys(1))
        .build()
        .await?;
    let result = async {
        let leader_index = cluster
            .wait_for_raft_cluster_formation(CLUSTER_FORMATION_TIMEOUT)
            .await?;

        send_transfer(&cluster, leader_index).await?;
        send_transfer(&cluster, leader_index).await?;

        cluster.suspend_node(leader_index).await?;

        // This creates a WAL/raft gap: the restarted node clears raft history, then
        // produces a block through loopback consensus while raft is disabled.
        cluster
            .start_node_with_overrides(leader_index, |config| {
                config.consensus_config.enabled = false;
                config.consensus_config.force_clear_raft_history = true;
            })
            .await?;

        send_transfer(&cluster, leader_index).await?;

        cluster.suspend_node(leader_index).await?;

        // Re-enable consensus after the gap. The old WAL blocks are replayed locally;
        // new blocks should be raft-canonized from this point onward.
        cluster
            .start_node_with_overrides(leader_index, |config| {
                config.consensus_config.enabled = true;
                config.consensus_config.force_clear_raft_history = false;
            })
            .await?;

        let leader_index = cluster
            .wait_for_raft_cluster_formation(CLUSTER_FORMATION_TIMEOUT)
            .await?;

        send_transfer(&cluster, leader_index).await?;

        // Restart once more with consensus enabled to verify the sparse raft history
        // written after re-enable is loadable.
        cluster.suspend_node(leader_index).await?;
        cluster.start_node(leader_index).await?;

        let leader_index = cluster
            .wait_for_raft_cluster_formation(CLUSTER_FORMATION_TIMEOUT)
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
        .with_consensus_secret_keys(consensus_test_keys(3))
        .build()
        .await?;
    let result = async {
        let leader_index = cluster
            .wait_for_raft_cluster_formation(CLUSTER_FORMATION_TIMEOUT)
            .await?;
        let mut initial_applied = 0;
        for idx in 0..cluster.len() {
            initial_applied = initial_applied
                .max(raft_last_applied(&raft_status(&cluster, idx).await?, idx)?.unwrap_or(0));
        }

        let replicated_applied =
            send_transfer_and_wait_for_active_replication(&cluster, leader_index).await?;
        assert!(replicated_applied > initial_applied);

        Ok(())
    }
    .await;
    let shutdown_result = cluster.shutdown_all().await;
    result.and(shutdown_result)
}

#[test_log::test(tokio::test)]
async fn consensus_cluster_rotates_leader_after_failure() -> anyhow::Result<()> {
    let mut cluster = MultiNodeTester::builder()
        .with_consensus_secret_keys(consensus_test_keys(3))
        .build()
        .await?;
    let result = async {
        let initial_leader_idx = cluster
            .wait_for_raft_cluster_formation(CLUSTER_FORMATION_TIMEOUT)
            .await?;
        let initial_leader_node_id = raft_node_id(
            &raft_status(&cluster, initial_leader_idx).await?,
            initial_leader_idx,
        )?;

        // Warm up follower replication before taking the leader down so the surviving
        // nodes have already exchanged append entries with the elected leader.
        send_transfer_and_wait_for_active_replication(&cluster, initial_leader_idx).await?;

        cluster.suspend_node(initial_leader_idx).await?;

        let new_leader_idx = cluster
            .wait_for_active_raft_cluster_formation(CLUSTER_FORMATION_TIMEOUT)
            .await?;
        let new_leader_id = raft_node_id(
            &raft_status(&cluster, new_leader_idx).await?,
            new_leader_idx,
        )?;

        assert_ne!(initial_leader_node_id, new_leader_id);

        send_transfer_and_wait_for_active_replication(&cluster, new_leader_idx).await?;

        Ok(())
    }
    .await;
    let shutdown_result = cluster.shutdown_all().await;
    result.and(shutdown_result)
}

#[test_log::test(tokio::test)]
async fn consensus_cluster_stops_making_progress_without_quorum() -> anyhow::Result<()> {
    let mut cluster = MultiNodeTester::builder()
        .with_consensus_secret_keys(consensus_test_keys(3))
        .build()
        .await?;
    let result = async {
        let leader_idx = cluster
            .wait_for_raft_cluster_formation(CLUSTER_FORMATION_TIMEOUT)
            .await?;
        let committed_applied =
            send_transfer_and_wait_for_active_replication(&cluster, leader_idx).await?;
        let follower_indices: Vec<_> = (0..cluster.len())
            .filter(|idx| *idx != leader_idx)
            .collect();
        let survivor_idx = follower_indices[1];

        cluster.suspend_node(leader_idx).await?;
        cluster.suspend_node(follower_indices[0]).await?;

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
async fn consensus_original_leader_rejoins_and_cluster_remains_stable() -> anyhow::Result<()> {
    let mut cluster = MultiNodeTester::builder()
        .with_consensus_secret_keys(consensus_test_keys(3))
        .build()
        .await?;
    let result = async {
        let initial_leader_idx = cluster
            .wait_for_raft_cluster_formation(CLUSTER_FORMATION_TIMEOUT)
            .await?;

        send_transfer_and_wait_for_active_replication(&cluster, initial_leader_idx).await?;

        cluster.suspend_node(initial_leader_idx).await?;

        let new_leader_idx = cluster
            .wait_for_active_raft_cluster_formation(CLUSTER_FORMATION_TIMEOUT)
            .await?;

        // Advance the cluster while the original leader is absent so it has entries to catch up.
        send_transfer_and_wait_for_active_replication(&cluster, new_leader_idx).await?;
        let target_applied = active_max_last_applied_index(&cluster).await?;

        // Restart the original leader. It must rejoin without disrupting the running cluster:
        // exactly one leader must remain, all three nodes must agree, and state must converge.
        cluster.start_node(initial_leader_idx).await?;
        let final_leader_idx = cluster
            .wait_for_raft_cluster_formation(CLUSTER_FORMATION_TIMEOUT)
            .await?;
        cluster
            .wait_for_active_last_applied_index_convergence(target_applied, REPLICATION_TIMEOUT)
            .await?;

        // Verify the cluster continues to make progress after the rejoin.
        send_transfer_and_wait_for_active_replication(&cluster, final_leader_idx).await?;

        Ok(())
    }
    .await;
    let shutdown_result = cluster.shutdown_all().await;
    result.and(shutdown_result)
}

#[test_log::test(tokio::test)]
async fn consensus_cluster_recovers_after_quorum_loss() -> anyhow::Result<()> {
    let mut cluster = MultiNodeTester::builder()
        .with_consensus_secret_keys(consensus_test_keys(3))
        .build()
        .await?;
    let result = async {
        let leader_idx = cluster
            .wait_for_raft_cluster_formation(CLUSTER_FORMATION_TIMEOUT)
            .await?;
        let committed_applied =
            send_transfer_and_wait_for_active_replication(&cluster, leader_idx).await?;

        let follower_indices: Vec<_> = (0..cluster.len())
            .filter(|&idx| idx != leader_idx)
            .collect();
        let survivor_idx = follower_indices[1];

        cluster.suspend_node(leader_idx).await?;
        cluster.suspend_node(follower_indices[0]).await?;

        // Verify that quorum loss stops all progress.
        let survivor_before =
            raft_last_applied(&raft_status(&cluster, survivor_idx).await?, survivor_idx)?
                .unwrap_or(0);
        sleep(Duration::from_secs(2)).await;
        let survivor_after =
            raft_last_applied(&raft_status(&cluster, survivor_idx).await?, survivor_idx)?
                .unwrap_or(0);
        assert!(
            survivor_after <= survivor_before,
            "last_applied must not advance without quorum: before={survivor_before} after={survivor_after}",
        );

        // Restore quorum and verify the cluster recovers and makes progress.
        cluster.start_node(leader_idx).await?;
        cluster.start_node(follower_indices[0]).await?;
        let new_leader_idx = cluster
            .wait_for_raft_cluster_formation(CLUSTER_FORMATION_TIMEOUT)
            .await?;
        let recovery_applied =
            send_transfer_and_wait_for_active_replication(&cluster, new_leader_idx).await?;
        assert!(
            recovery_applied > committed_applied,
            "cluster must make progress after quorum is restored: committed={committed_applied} recovery={recovery_applied}",
        );

        Ok(())
    }
    .await;
    let shutdown_result = cluster.shutdown_all().await;
    result.and(shutdown_result)
}

#[test_log::test(tokio::test)]
async fn consensus_cluster_fully_restarts_and_recovers() -> anyhow::Result<()> {
    let mut cluster = MultiNodeTester::builder()
        .with_consensus_secret_keys(consensus_test_keys(3))
        .build()
        .await?;
    let result = async {
        let leader_idx = cluster
            .wait_for_raft_cluster_formation(CLUSTER_FORMATION_TIMEOUT)
            .await?;
        let last_applied =
            send_transfer_and_wait_for_active_replication(&cluster, leader_idx).await?;

        // Suspend all nodes: state is durably on disk before any restarts.
        for idx in 0..cluster.len() {
            cluster.suspend_node(idx).await?;
        }
        // Restart all nodes: they recover from disk, re-elect a leader, and resume.
        for idx in 0..cluster.len() {
            cluster.start_node(idx).await?;
        }

        let new_leader_idx = cluster
            .wait_for_raft_cluster_formation(CLUSTER_FORMATION_TIMEOUT)
            .await?;
        cluster
            .wait_for_active_last_applied_index_convergence(last_applied, REPLICATION_TIMEOUT)
            .await?;

        // Verify the cluster continues to make progress after the full restart.
        send_transfer_and_wait_for_active_replication(&cluster, new_leader_idx).await?;

        Ok(())
    }
    .await;
    let shutdown_result = cluster.shutdown_all().await;
    result.and(shutdown_result)
}

#[test_log::test(tokio::test)]
async fn consensus_late_node_joins_and_catches_up() -> anyhow::Result<()> {
    let mut cluster = MultiNodeTester::builder()
        .with_consensus_secret_keys(consensus_test_keys(3))
        .build()
        .await?;
    let result = async {
        // Suspend the third node before cluster formation so it hasn't participated in any
        // block production — simulating a node that joins an already-established cluster.
        let late_node_idx = 2;
        cluster.suspend_node(late_node_idx).await?;

        // Two of three nodes form a quorum; the cluster must elect a leader and make progress.
        let leader_idx = cluster
            .wait_for_active_raft_cluster_formation(CLUSTER_FORMATION_TIMEOUT)
            .await?;

        send_transfer_and_wait_for_active_replication(&cluster, leader_idx).await?;
        send_transfer_and_wait_for_active_replication(&cluster, leader_idx).await?;
        let target_applied = active_max_last_applied_index(&cluster).await?;

        // Start the late node. It must receive all missed entries via Raft log replication.
        cluster.start_node(late_node_idx).await?;
        wait_for_node_last_applied_index_at_or_above(
            &cluster,
            late_node_idx,
            target_applied,
            REPLICATION_TIMEOUT,
        )
        .await?;

        // The full 3-node cluster must be stable after the late joiner has caught up.
        cluster
            .wait_for_raft_cluster_formation(CLUSTER_FORMATION_TIMEOUT)
            .await?;

        Ok(())
    }
    .await;
    let shutdown_result = cluster.shutdown_all().await;
    result.and(shutdown_result)
}

#[test_log::test(tokio::test)]
async fn consensus_follower_restarts_and_catches_up() -> anyhow::Result<()> {
    let mut cluster = MultiNodeTester::builder()
        .with_consensus_secret_keys(consensus_test_keys(3))
        .build()
        .await?;
    let result = async {
        let leader_idx = cluster
            .wait_for_raft_cluster_formation(CLUSTER_FORMATION_TIMEOUT)
            .await?;
        let follower_idx = (0..cluster.len())
            .find(|idx| *idx != leader_idx)
            .expect("3-node cluster must have a follower");

        cluster.suspend_node(follower_idx).await?;
        let active_leader_idx = cluster
            .wait_for_active_raft_cluster_formation(CLUSTER_FORMATION_TIMEOUT)
            .await?;

        send_transfer_and_wait_for_active_replication(&cluster, active_leader_idx).await?;
        let target_applied =
            send_transfer_and_wait_for_active_replication(&cluster, active_leader_idx).await?;

        cluster.start_node(follower_idx).await?;
        wait_for_node_last_applied_index_at_or_above(
            &cluster,
            follower_idx,
            target_applied,
            REPLICATION_TIMEOUT,
        )
        .await?;

        Ok(())
    }
    .await;
    let shutdown_result = cluster.shutdown_all().await;
    result.and(shutdown_result)
}

#[test_log::test(tokio::test)]
async fn consensus_restarted_node_catches_up_after_long_transaction_storm() -> anyhow::Result<()> {
    let mut cluster = MultiNodeTester::builder()
        .with_consensus_secret_keys(consensus_test_keys(3))
        .build()
        .await?;
    let result = async {
        cluster
            .wait_for_raft_cluster_formation(CLUSTER_FORMATION_TIMEOUT)
            .await?;

        let restarted_node_idx = 2;
        let active_node_indices = (0..cluster.len())
            .filter(|idx| *idx != restarted_node_idx)
            .collect::<Vec<_>>();
        let restarted_node_rpc_url = cluster.node(restarted_node_idx).l2_rpc_url().to_owned();
        let restarted_node_initial_block = latest_l2_block(cluster.node(restarted_node_idx)).await?;

        cluster.suspend_node(restarted_node_idx).await?;
        cluster
            .wait_for_active_raft_cluster_formation(CLUSTER_FORMATION_TIMEOUT)
            .await?;

        let load_stats = generate_consensus_transaction_storm(
            &cluster,
            &active_node_indices,
            CONSENSUS_LONG_GAP_LOAD_DURATION,
        )
        .await?;
        let target_block = latest_l2_block(cluster.node(active_node_indices[0])).await?;
        let target_applied = active_max_last_applied_index(&cluster).await?;
        assert!(
            target_block > restarted_node_initial_block,
            "active cluster head did not advance while node was down: initial={restarted_node_initial_block}, target={target_block}"
        );
        tracing::info!(
            attempts = load_stats.attempts,
            tx_blocks = load_stats.blocks.len(),
            first_tx_block = load_stats.blocks.first().copied(),
            last_tx_block = load_stats.blocks.last().copied(),
            elapsed_ms = load_stats.elapsed.as_millis(),
            restarted_node_initial_block,
            target_block,
            target_applied,
            "transaction storm finished while consensus node was stopped"
        );

        let rpc_monitor = HttpRpcRecorder::start_http(
            "restarted-consensus-node",
            restarted_node_rpc_url,
            RpcRecordConfig {
                poll_interval: Duration::from_millis(200),
                request_timeout: Duration::from_secs(1),
                max_samples: 20_000,
            },
        );
        let restart_started_at = Instant::now();
        cluster.start_node(restarted_node_idx).await?;

        let catch_up_result = async {
            let raft_caught_up_index = wait_for_node_last_applied_index_at_or_above(
                &cluster,
                restarted_node_idx,
                target_applied,
                CONSENSUS_LONG_GAP_CATCH_UP_TIMEOUT,
            )
            .await
            .context("restarted node did not reach target Raft last_applied_index")?;
            let raft_caught_up_at = restart_started_at.elapsed();

            wait_for_l2_block(
                cluster.node(restarted_node_idx),
                target_block,
                CONSENSUS_LONG_GAP_CATCH_UP_TIMEOUT,
            )
            .await
            .context("restarted node Raft-applied the target but did not expose the target L2 block")?;
            let l2_caught_up_at = restart_started_at.elapsed();

            wait_for_rpc_monitor_block(&rpc_monitor, target_block, REPLICATION_TIMEOUT)
                .await
                .context("RPC monitor did not observe the restarted node's caught-up L2 head")?;

            anyhow::Ok((raft_caught_up_index, raft_caught_up_at, l2_caught_up_at))
        }
        .await;

        let rpc_report = rpc_monitor.stop().await;
        tracing::info!("restarted consensus node RPC monitor summary: {rpc_report}");
        tracing::info!(
            "restarted consensus node RPC monitor timeline:\n{}",
            rpc_report.format_detailed_timeline()
        );

        let all_node_indices = (0..cluster.len()).collect::<Vec<_>>();
        let final_l2_blocks = l2_block_snapshot(&cluster, &all_node_indices).await;
        let final_last_applied = raft_last_applied_snapshot(&cluster, &all_node_indices).await;

        let (raft_caught_up_index, raft_caught_up_at, l2_caught_up_at) =
            catch_up_result.with_context(|| {
                format!(
                    "restarted consensus node failed to catch up after long downtime: \
                     target_block={target_block}, target_applied={target_applied}, \
                     initial_block={restarted_node_initial_block}, active_nodes={active_node_indices:?}, \
                     l2_blocks=[{}], raft_last_applied=[{}], rpc_report={rpc_report}",
                    final_l2_blocks.join(", "),
                    final_last_applied.join(", "),
                )
            })?;

        rpc_report.assert_eventually_ready()?;
        let rpc_observed_target_at = first_rpc_observed_block_at(&rpc_report, target_block)
            .with_context(|| {
                format!(
                    "RPC monitor never observed restarted node reaching target block {target_block}: {rpc_report}"
                )
            })?;
        assert!(
            rpc_report
                .latest_block_number()
                .is_some_and(|block| block >= target_block),
            "RPC monitor latest block did not reach target {target_block}: {rpc_report}"
        );
        tracing::info!(
            raft_caught_up_index,
            raft_caught_up_ms = raft_caught_up_at.as_millis(),
            l2_caught_up_ms = l2_caught_up_at.as_millis(),
            rpc_observed_target_ms = rpc_observed_target_at.as_millis(),
            target_block,
            target_applied,
            "restarted consensus node caught up after long downtime"
        );

        let leader_idx = cluster
            .wait_for_raft_cluster_formation(CLUSTER_FORMATION_TIMEOUT)
            .await?;
        let post_rejoin_block =
            send_transfer_and_wait_for_l2_blocks(&cluster, leader_idx, &all_node_indices).await?;
        assert!(
            post_rejoin_block > target_block,
            "cluster did not keep producing after restarted node caught up: post_rejoin_block={post_rejoin_block}, target_block={target_block}"
        );

        Ok(())
    }
    .await;
    let shutdown_result = cluster.shutdown_all().await;
    result.and(shutdown_result)
}

#[test_log::test(tokio::test)]
async fn consensus_restarted_node_catches_up_while_transaction_storm_continues()
-> anyhow::Result<()> {
    let mut cluster = MultiNodeTester::builder()
        .with_consensus_secret_keys(consensus_test_keys(3))
        .build()
        .await?;
    let result = async {
        cluster
            .wait_for_raft_cluster_formation(CLUSTER_FORMATION_TIMEOUT)
            .await?;

        let restarted_node_idx = 2;
        let active_node_indices = (0..cluster.len())
            .filter(|idx| *idx != restarted_node_idx)
            .collect::<Vec<_>>();
        let all_node_indices = (0..cluster.len()).collect::<Vec<_>>();
        let restarted_node_rpc_url = cluster.node(restarted_node_idx).l2_rpc_url().to_owned();
        let restarted_node_initial_block = latest_l2_block(cluster.node(restarted_node_idx)).await?;

        cluster.suspend_node(restarted_node_idx).await?;
        let active_leader_idx = cluster
            .wait_for_active_raft_cluster_formation(CLUSTER_FORMATION_TIMEOUT)
            .await?;
        let active_follower_idx = active_node_indices
            .iter()
            .copied()
            .find(|idx| *idx != active_leader_idx)
            .expect("two active nodes should include one follower");

        let rpc_record_config = RpcRecordConfig {
            poll_interval: Duration::from_millis(200),
            request_timeout: Duration::from_secs(1),
            max_samples: 30_000,
        };
        let active_leader_rpc_monitor = HttpRpcRecorder::start_http(
            format!("active-leader-node-{active_leader_idx}"),
            cluster.node(active_leader_idx).l2_rpc_url().to_owned(),
            rpc_record_config.clone(),
        );
        let active_follower_rpc_monitor = HttpRpcRecorder::start_http(
            format!("active-follower-node-{active_follower_idx}"),
            cluster.node(active_follower_idx).l2_rpc_url().to_owned(),
            rpc_record_config.clone(),
        );
        let restarted_rpc_monitor = HttpRpcRecorder::start_http(
            format!("restarted-node-{restarted_node_idx}"),
            restarted_node_rpc_url,
            rpc_record_config,
        );

        let observation_result = async {
            let load_stats = generate_consensus_transaction_storm_across_restart(
                &mut cluster,
                &active_node_indices,
                restarted_node_idx,
                CONSENSUS_LONG_GAP_LOAD_DURATION,
                CONSENSUS_CONTINUED_LOAD_AFTER_RESTART_DURATION,
                &restarted_rpc_monitor,
            )
            .await?;
            let final_active_block = latest_l2_block(cluster.node(active_node_indices[0])).await?;
            assert!(
                load_stats.target_block_at_restart > restarted_node_initial_block,
                "active cluster head did not advance while node was down: initial={}, target_at_restart={}",
                restarted_node_initial_block,
                load_stats.target_block_at_restart
            );
            assert!(
                final_active_block > load_stats.target_block_at_restart,
                "active cluster did not keep producing after restart: final_active_block={}, target_at_restart={}",
                final_active_block,
                load_stats.target_block_at_restart
            );

            wait_for_l2_block(
                cluster.node(restarted_node_idx),
                final_active_block,
                CONSENSUS_LONG_GAP_CATCH_UP_TIMEOUT,
            )
            .await
            .context("restarted node did not catch up to the final active head after continued load")?;

            wait_for_rpc_monitor_block(
                &active_leader_rpc_monitor,
                final_active_block,
                REPLICATION_TIMEOUT,
            )
            .await
            .context("active leader RPC monitor did not observe the final active head")?;
            wait_for_rpc_monitor_block(
                &active_follower_rpc_monitor,
                final_active_block,
                REPLICATION_TIMEOUT,
            )
            .await
            .context("active follower RPC monitor did not observe the final active head")?;
            wait_for_rpc_monitor_block(&restarted_rpc_monitor, final_active_block, REPLICATION_TIMEOUT)
                .await
                .context("restarted node RPC monitor did not observe the final active head")?;

            anyhow::Ok((load_stats, final_active_block))
        }
        .await;

        let active_leader_rpc_report = active_leader_rpc_monitor.stop().await;
        let active_follower_rpc_report = active_follower_rpc_monitor.stop().await;
        let restarted_rpc_report = restarted_rpc_monitor.stop().await;

        tracing::info!("active leader RPC monitor summary: {active_leader_rpc_report}");
        tracing::info!(
            "active leader RPC monitor timeline:\n{}",
            active_leader_rpc_report.format_detailed_timeline()
        );
        tracing::info!("active follower RPC monitor summary: {active_follower_rpc_report}");
        tracing::info!(
            "active follower RPC monitor timeline:\n{}",
            active_follower_rpc_report.format_detailed_timeline()
        );
        tracing::info!("restarted node RPC monitor summary: {restarted_rpc_report}");
        tracing::info!(
            "restarted node RPC monitor timeline:\n{}",
            restarted_rpc_report.format_detailed_timeline()
        );

        let final_l2_blocks = l2_block_snapshot(&cluster, &all_node_indices).await;
        let final_last_applied = raft_last_applied_snapshot(&cluster, &all_node_indices).await;
        let (load_stats, final_active_block) = observation_result.with_context(|| {
            format!(
                "restarted consensus node failed to catch up while load continued: \
                 initial_block={restarted_node_initial_block}, active_nodes={active_node_indices:?}, \
                 l2_blocks=[{}], raft_last_applied=[{}], \
                 active_leader_rpc_report={active_leader_rpc_report}, \
                 active_follower_rpc_report={active_follower_rpc_report}, \
                 restarted_rpc_report={restarted_rpc_report}",
                final_l2_blocks.join(", "),
                final_last_applied.join(", "),
            )
        })?;

        assert_rpc_monitor_stayed_ready(&active_leader_rpc_report)?;
        assert_rpc_monitor_stayed_ready(&active_follower_rpc_report)?;
        assert_rpc_monitor_recovered_after_outage(&restarted_rpc_report)?;
        let active_leader_final_at =
            first_rpc_observed_block_at(&active_leader_rpc_report, final_active_block)
                .context("active leader RPC monitor never observed final active block")?;
        let active_follower_final_at =
            first_rpc_observed_block_at(&active_follower_rpc_report, final_active_block)
                .context("active follower RPC monitor never observed final active block")?;
        let restarted_final_at =
            first_rpc_observed_block_at(&restarted_rpc_report, final_active_block)
                .context("restarted RPC monitor never observed final active block")?;

        tracing::info!(
            attempts = load_stats.attempts,
            tx_blocks = load_stats.blocks.len(),
            first_tx_block = load_stats.blocks.first().copied(),
            last_tx_block = load_stats.blocks.last().copied(),
            blocks_before_restart = load_stats.blocks_before_restart,
            blocks_after_restart = load_stats.blocks_after_restart,
            elapsed_ms = load_stats.elapsed.as_millis(),
            restart_started_ms = load_stats.restart_started_at.as_millis(),
            restart_completed_ms = load_stats.restart_completed_at.as_millis(),
            target_block_at_restart = load_stats.target_block_at_restart,
            target_applied_at_restart = load_stats.target_applied_at_restart,
            raft_caught_up_index = load_stats.raft_caught_up_index,
            raft_caught_up_ms = load_stats.raft_caught_up_at.as_millis(),
            l2_caught_up_ms = load_stats.l2_caught_up_at.as_millis(),
            rpc_caught_up_ms = load_stats.rpc_caught_up_at.as_millis(),
            final_active_block,
            active_leader_final_rpc_ms = active_leader_final_at.as_millis(),
            active_follower_final_rpc_ms = active_follower_final_at.as_millis(),
            restarted_final_rpc_ms = restarted_final_at.as_millis(),
            "restarted consensus node caught up while transaction storm continued"
        );

        Ok(())
    }
    .await;
    let shutdown_result = cluster.shutdown_all().await;
    result.and(shutdown_result)
}
