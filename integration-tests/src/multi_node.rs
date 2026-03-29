use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;
use tokio::time::Instant;
use zksync_os_status_server::StatusResponse;

use crate::{AnvilL1, ChainLayout, Config, LockedPort, NodeRole, PROTOCOL_VERSION, Tester};

/// Represents the consensus state of a Raft cluster based on node status responses
#[derive(Debug)]
pub struct ClusterState {
    nodes: Vec<(usize, Result<StatusResponse, String>)>,
}

impl ClusterState {
    /// Collects status from all nodes in parallel
    pub async fn collect(nodes: &[Tester]) -> Self {
        let node_states =
            futures::future::join_all(nodes.iter().enumerate().map(|(idx, node)| async move {
                (idx, node.status().await.map_err(|e| e.to_string()))
            }))
            .await;
        Self { nodes: node_states }
    }

    /// Collects status from all non-suspended nodes in parallel.
    pub async fn collect_active(nodes: &[Tester]) -> Self {
        let node_states =
            futures::future::join_all(
                nodes
                    .iter()
                    .enumerate()
                    .filter(|(_, node)| !node.is_suspended())
                    .map(|(idx, node)| async move {
                        (idx, node.status().await.map_err(|e| e.to_string()))
                    }),
            )
            .await;
        Self { nodes: node_states }
    }

    /// Returns true if all nodes are healthy and returned successful status
    pub fn all_healthy(&self) -> bool {
        self.nodes
            .iter()
            .all(|(_, result)| matches!(result, Ok(status) if status.healthy))
    }

    /// Returns indices of nodes that report themselves as leaders
    pub fn leader_indices(&self) -> Vec<usize> {
        self.nodes
            .iter()
            .filter_map(|(idx, result)| {
                result.as_ref().ok().and_then(|status| {
                    status
                        .consensus
                        .raft
                        .as_ref()
                        .filter(|r| r.is_leader)
                        .map(|_| *idx)
                })
            })
            .collect()
    }

    /// Returns true if all healthy nodes report having a current leader
    pub fn all_have_leader(&self) -> bool {
        self.nodes
            .iter()
            .filter_map(|(_, result)| result.as_ref().ok())
            .all(|status| {
                status
                    .consensus
                    .raft
                    .as_ref()
                    .and_then(|r| r.current_leader.as_ref())
                    .is_some()
            })
    }

    /// Returns the agreed-upon leader ID if all nodes agree, None otherwise
    pub fn agreed_leader(&self) -> Option<&str> {
        let leaders: Vec<_> = self
            .nodes
            .iter()
            .filter_map(|(_, result)| result.as_ref().ok())
            .filter_map(|status| status.consensus.raft.as_ref()?.current_leader.as_deref())
            .collect();

        leaders
            .first()
            .copied()
            .filter(|first| leaders.iter().all(|leader| leader == first))
    }

    /// Returns true if the cluster has successfully formed:
    /// - All nodes healthy
    /// - Exactly one leader
    /// - All nodes have a leader
    /// - All nodes agree on the same leader
    /// - The leader's node_id matches what others believe
    pub fn is_formed(&self) -> bool {
        let leader_indices = self.leader_indices();
        if leader_indices.len() != 1 {
            return false;
        }

        let agreed = self.agreed_leader();
        let leader_node_id = self
            .status_for_index(leader_indices[0])
            .and_then(|s| s.consensus.raft.as_ref())
            .map(|r| r.node_id.as_str());

        self.all_healthy() && self.all_have_leader() && agreed.is_some() && agreed == leader_node_id
    }

    /// Returns a summary string for logging cluster formation progress
    pub fn summary(&self) -> String {
        let leader_indices = self.leader_indices();
        let agreed = self.agreed_leader();
        let leader_node_id = leader_indices
            .first()
            .and_then(|&idx| self.status_for_index(idx))
            .and_then(|s| s.consensus.raft.as_ref())
            .map(|r| r.node_id.as_str());

        format!(
            "healthy={} leaders={} all_have_leader={} agreed_leader={:?} leader_node_id={:?}",
            self.all_healthy(),
            leader_indices.len(),
            self.all_have_leader(),
            agreed,
            leader_node_id
        )
    }

    /// Returns a detailed explanation of why cluster formation failed
    pub fn failure_reason(&self) -> String {
        let mut reasons = Vec::new();

        if !self.all_healthy() {
            let unhealthy: Vec<_> = self
                .nodes
                .iter()
                .filter_map(|(idx, result)| match result {
                    Ok(status) if !status.healthy => Some(format!("node_{}: healthy=false", idx)),
                    Err(err) => Some(format!("node_{}: error={:?}", idx, err)),
                    _ => None,
                })
                .collect();
            reasons.push(format!("Unhealthy nodes: [{}]", unhealthy.join(", ")));
        }

        let leader_indices = self.leader_indices();
        if leader_indices.len() != 1 {
            let leader_info: Vec<_> = leader_indices
                .iter()
                .filter_map(|&idx| {
                    self.nodes[idx]
                        .1
                        .as_ref()
                        .ok()
                        .and_then(|s| s.consensus.raft.as_ref())
                        .map(|r| format!("node_{} (id={})", idx, r.node_id))
                })
                .collect();
            reasons.push(format!(
                "Expected 1 leader, found {}: [{}]",
                leader_indices.len(),
                leader_info.join(", ")
            ));
        }

        if !self.all_have_leader() {
            let without_leader: Vec<_> = self
                .nodes
                .iter()
                .filter_map(|(idx, result)| {
                    result.as_ref().ok().and_then(|status| {
                        if status.consensus.raft.as_ref()?.current_leader.is_none() {
                            Some(format!("node_{}", idx))
                        } else {
                            None
                        }
                    })
                })
                .collect();
            reasons.push(format!(
                "Nodes without leader: [{}]",
                without_leader.join(", ")
            ));
        }

        if let Some(agreed) = self.agreed_leader() {
            let leader_node_id = leader_indices
                .first()
                .and_then(|&idx| self.status_for_index(idx))
                .and_then(|s| s.consensus.raft.as_ref())
                .map(|r| r.node_id.as_str());

            if leader_node_id != Some(agreed) {
                reasons.push(format!(
                    "Leader mismatch: cluster agrees on '{}' but leader reports '{:?}'",
                    agreed, leader_node_id
                ));
            }
        } else {
            let leader_views: Vec<_> = self
                .nodes
                .iter()
                .filter_map(|(idx, result)| {
                    result
                        .as_ref()
                        .ok()
                        .and_then(|s| s.consensus.raft.as_ref()?.current_leader.as_ref())
                        .map(|leader| format!("node_{}: {}", idx, leader))
                })
                .collect();
            if !leader_views.is_empty() {
                reasons.push(format!(
                    "Nodes disagree on leader: [{}]",
                    leader_views.join(", ")
                ));
            }
        }

        if reasons.is_empty() {
            "Unknown reason".to_string()
        } else {
            reasons.join("; ")
        }
    }

    /// Returns true if all healthy nodes report the same non-empty `last_applied_index`.
    pub fn all_have_same_last_applied_index_at_or_above(&self, min_index: u64) -> bool {
        let mut last_applied = self.nodes.iter().filter_map(|(_, result)| {
            result
                .as_ref()
                .ok()?
                .consensus
                .raft
                .as_ref()?
                .last_applied_index
        });
        let Some(first) = last_applied.next() else {
            return false;
        };
        first >= min_index && last_applied.all(|idx| idx == first) && self.all_healthy()
    }

    pub fn agreed_last_applied_index(&self) -> Option<u64> {
        let mut last_applied = self.nodes.iter().filter_map(|(_, result)| {
            result
                .as_ref()
                .ok()?
                .consensus
                .raft
                .as_ref()?
                .last_applied_index
        });
        let first = last_applied.next()?;
        last_applied.all(|idx| idx == first).then_some(first)
    }

    fn status_for_index(&self, index: usize) -> Option<&StatusResponse> {
        self.nodes
            .iter()
            .find(|(idx, _)| *idx == index)
            .and_then(|(_, result)| result.as_ref().ok())
    }
}

/// Test harness for multi-node consensus testing
pub struct MultiNodeTester {
    pub nodes: Vec<Tester>,
}

impl MultiNodeTester {
    pub fn builder() -> MultiNodeTesterBuilder {
        MultiNodeTesterBuilder::default()
    }

    pub fn node(&self, index: usize) -> &Tester {
        &self.nodes[index]
    }

    /// Shuts down all active nodes and drops suspended ones.
    pub async fn shutdown_all(self) -> anyhow::Result<()> {
        for node in self.nodes {
            if node.is_suspended() {
                continue;
            }
            node.shutdown().await?;
        }
        Ok(())
    }

    /// Permanently shut down a node and remove it from the cluster.
    pub async fn shutdown_node(&mut self, index: usize) -> anyhow::Result<()> {
        tracing::info!(index, "shutting down node...");
        self.nodes.remove(index).shutdown().await
    }

    /// Suspend a node (shut down its process, retain its state). The slot remains in `nodes`
    /// as a suspended `Tester` that can be restarted later with [`Self::start_node`].
    pub async fn suspend_node(&mut self, index: usize) {
        tracing::info!(index, "suspending node...");
        let tester = self.nodes.remove(index);
        self.nodes.insert(index, tester.suspend().await);
    }

    /// Restart a previously suspended node.
    pub async fn start_node(&mut self, index: usize) -> anyhow::Result<()> {
        tracing::info!(index, "starting suspended node...");
        assert!(
            self.nodes[index].is_suspended(),
            "node {index} is not suspended"
        );
        let suspended = self.nodes.remove(index);
        self.nodes.insert(index, suspended.resume().await?);
        Ok(())
    }

    /// Waits for the Raft cluster to form with a single elected leader
    /// Returns the index of the leader node
    pub async fn wait_for_raft_cluster_formation(
        &self,
        timeout: Duration,
    ) -> anyhow::Result<usize> {
        self.wait_for_raft_cluster_formation_inner(timeout, false)
            .await
    }

    /// Same as `wait_for_raft_cluster_formation`, but ignores suspended nodes.
    pub async fn wait_for_active_raft_cluster_formation(
        &self,
        timeout: Duration,
    ) -> anyhow::Result<usize> {
        self.wait_for_raft_cluster_formation_inner(timeout, true)
            .await
    }

    pub async fn wait_for_active_last_applied_index_convergence(
        &self,
        min_index: u64,
        timeout: Duration,
    ) -> anyhow::Result<u64> {
        let deadline = Instant::now() + timeout;
        let mut last_summary = String::new();

        while Instant::now() < deadline {
            let cluster_state = ClusterState::collect_active(&self.nodes).await;
            let summary = cluster_state.summary();

            if summary != last_summary {
                tracing::info!(%summary, min_index, "raft last_applied convergence check");
                last_summary = summary;
            }

            if cluster_state.all_have_same_last_applied_index_at_or_above(min_index) {
                let last_applied = cluster_state
                    .agreed_last_applied_index()
                    .expect("checked above");
                tracing::info!(last_applied, "raft last_applied converged");
                return Ok(last_applied);
            }

            tokio::time::sleep(Duration::from_millis(200)).await;
        }

        let final_state = ClusterState::collect_active(&self.nodes).await;

        tracing::error!(
            final_statuses = ?final_state.nodes,
            min_index,
            "failed to converge raft last_applied index"
        );

        anyhow::bail!(
            "timed out waiting for active nodes to converge on last_applied_index >= {min_index}: {}",
            final_state.summary()
        )
    }

    async fn wait_for_raft_cluster_formation_inner(
        &self,
        timeout: Duration,
        active_only: bool,
    ) -> anyhow::Result<usize> {
        let deadline = Instant::now() + timeout;
        let mut last_summary = String::new();

        while Instant::now() < deadline {
            let cluster_state = if active_only {
                ClusterState::collect_active(&self.nodes).await
            } else {
                ClusterState::collect(&self.nodes).await
            };
            let summary = cluster_state.summary();

            if summary != last_summary {
                tracing::info!(%summary, active_only, "raft cluster formation check");
                last_summary = summary;
            }

            if cluster_state.is_formed() {
                let leader_index = cluster_state.leader_indices()[0];
                tracing::info!(leader_index, active_only, "raft cluster formed");
                return Ok(leader_index);
            }

            tokio::time::sleep(Duration::from_millis(200)).await;
        }

        let final_state = if active_only {
            ClusterState::collect_active(&self.nodes).await
        } else {
            ClusterState::collect(&self.nodes).await
        };

        tracing::error!(
            final_statuses = ?final_state.nodes,
            reason = %final_state.failure_reason(),
            active_only,
            "failed to form raft cluster"
        );

        anyhow::bail!(
            "timed out waiting for raft cluster formation: {}",
            final_state.failure_reason()
        )
    }
}

#[derive(Default)]
pub struct MultiNodeTesterBuilder {
    consensus_secret_keys: Vec<zksync_os_network::SecretKey>,
    consensus_nodes_to_spawn: Option<usize>,
}

impl MultiNodeTesterBuilder {
    pub fn with_consensus_secret_keys(mut self, keys: Vec<zksync_os_network::SecretKey>) -> Self {
        self.consensus_secret_keys = keys;
        self
    }

    pub fn spawn_consensus_nodes(mut self, count: usize) -> Self {
        self.consensus_nodes_to_spawn = Some(count);
        self
    }

    pub async fn build(self) -> anyhow::Result<MultiNodeTester> {
        let membership_nodes = self.consensus_secret_keys.len();
        assert!(
            membership_nodes > 0,
            "MultiNodeTester requires at least 1 node"
        );
        let num_nodes = self.consensus_nodes_to_spawn.unwrap_or(membership_nodes);
        assert!(
            num_nodes > 0 && num_nodes <= membership_nodes,
            "spawn_consensus_nodes must be in 1..={membership_nodes}"
        );

        let mut locked_ports = Vec::with_capacity(membership_nodes);
        for _ in 0..membership_nodes {
            locked_ports.push(LockedPort::acquire_unused().await?);
        }

        let node_records = self
            .consensus_secret_keys
            .iter()
            .zip(locked_ports.iter())
            .map(|(secret, port)| {
                zksync_os_network::NodeRecord::from_secret_key(
                    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port.port),
                    secret,
                )
            })
            .collect::<Vec<_>>();
        let peer_ids = node_records
            .iter()
            .map(|record| record.id)
            .collect::<Vec<_>>();

        let l1 = AnvilL1::start(ChainLayout::Default {
            protocol_version: PROTOCOL_VERSION,
        })
        .await?;

        let mut nodes = Vec::with_capacity(num_nodes);
        for (i, (secret, locked_port)) in self
            .consensus_secret_keys
            .into_iter()
            .take(num_nodes)
            .zip(locked_ports.into_iter())
            .enumerate()
        {
            let peers = peer_ids.clone();
            let boot_nodes: Vec<zksync_os_network::TrustedPeer> =
                node_records.iter().copied().map(Into::into).collect();
            let network_port = locked_port.port;
            let run_batcher_subsystem = i == 0;
            // Launch bootstrap node last so other peers are already up.
            let bootstrap = i + 1 == num_nodes;
            let expected_node_id = zksync_os_network::NodeRecord::from_secret_key(
                SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), network_port),
                &secret,
            )
            .id;
            tracing::info!(
                node_index = i,
                node_id = %expected_node_id,
                network_port,
                bootstrap,
                run_batcher_subsystem,
                "starting node..."
            );

            let node = Tester::launch_node_without_wait_with_network_port(
                l1.clone(),
                false,
                Some(move |config: &mut Config| {
                    config.general_config.node_role = NodeRole::MainNode;
                    config.general_config.main_node_rpc_url = None;
                    config.general_config.run_batcher_subsystem = run_batcher_subsystem;
                    config.network_config.enabled = true;
                    config.network_config.secret_key = Some(secret);
                    config.network_config.address = Ipv4Addr::LOCALHOST;
                    config.network_config.port = network_port;
                    config.network_config.boot_nodes = boot_nodes.clone();
                    config.consensus_config.enabled = true;
                    config.consensus_config.bootstrap = bootstrap;
                    config.consensus_config.peer_ids = peers.clone();
                }),
                ChainLayout::Default {
                    protocol_version: PROTOCOL_VERSION,
                },
                locked_port,
            )
            .await?;
            let tempdir_path = node.tempdir.path().to_path_buf();
            tracing::info!(
                node_index = i,
                %expected_node_id,
                "node started with tempfile: {}",
                tempdir_path.display()
            );
            tokio::spawn(async move {
                loop {
                    if !tempdir_path.exists() {
                        tracing::info!(
                            node_index = i,
                            %expected_node_id,
                            "tempfile removed: {}",
                            tempdir_path.display()
                        );
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            });
            nodes.push(node);
        }

        Ok(MultiNodeTester { nodes })
    }
}
