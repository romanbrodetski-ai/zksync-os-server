use futures::future::try_join_all;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;
use tokio::time::Instant;
use zksync_os_status_server::StatusResponse;

use crate::{
    AnvilL1, ChainLayout, Config, LockedPort, NodeRole, PROTOCOL_VERSION, StoppedTester, Tester,
};

const TEST_HEARTBEAT_INTERVAL: Duration = Duration::from_millis(100);
const TEST_ELECTION_TIMEOUT_MIN: Duration = Duration::from_millis(500);
const TEST_ELECTION_TIMEOUT_MAX: Duration = Duration::from_millis(1_500);

#[derive(Debug)]
enum NodeSlot {
    Running(Tester),
    Suspended(StoppedTester),
}

impl NodeSlot {
    fn running(&self) -> Option<&Tester> {
        match self {
            Self::Running(tester) => Some(tester),
            Self::Suspended(_) => None,
        }
    }
}

/// Represents the consensus state of a Raft cluster based on node status responses
#[derive(Debug)]
pub struct ClusterState {
    nodes: Vec<(usize, Result<StatusResponse, String>)>,
}

impl ClusterState {
    /// Collects status from all nodes in parallel
    async fn collect(nodes: &[NodeSlot]) -> Self {
        let node_states =
            futures::future::join_all(nodes.iter().enumerate().map(|(idx, node)| async move {
                let status = match node {
                    NodeSlot::Running(node) => node.status().await.map_err(|e| e.to_string()),
                    NodeSlot::Suspended(_) => Err("node is suspended".to_string()),
                };
                (idx, status)
            }))
            .await;
        Self { nodes: node_states }
    }

    /// Collects status from all non-suspended nodes in parallel.
    async fn collect_active(nodes: &[NodeSlot]) -> Self {
        let node_states =
            futures::future::join_all(
                nodes
                    .iter()
                    .enumerate()
                    .filter_map(|(idx, node)| node.running().map(|tester| (idx, tester)))
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
    nodes: Vec<NodeSlot>,
}

impl MultiNodeTester {
    pub fn builder() -> MultiNodeTesterBuilder {
        MultiNodeTesterBuilder::default()
    }

    pub fn node(&self, index: usize) -> &Tester {
        self.nodes[index]
            .running()
            .unwrap_or_else(|| panic!("node {index} is suspended"))
    }

    pub fn is_node_suspended(&self, index: usize) -> bool {
        matches!(self.nodes[index], NodeSlot::Suspended(_))
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Shuts down all active nodes and drops suspended ones.
    pub async fn shutdown_all(self) -> anyhow::Result<()> {
        for node in self.nodes {
            match node {
                NodeSlot::Running(node) => node.shutdown().await?,
                NodeSlot::Suspended(node) => node.shutdown().await?,
            }
        }
        Ok(())
    }

    /// Permanently shut down a node and remove it from the cluster.
    pub async fn shutdown_node(&mut self, index: usize) -> anyhow::Result<()> {
        tracing::info!(index, "shutting down node...");
        match self.nodes.remove(index) {
            NodeSlot::Running(node) => node.shutdown().await,
            NodeSlot::Suspended(node) => node.shutdown().await,
        }
    }

    /// Suspend a node (shut down its process, retain its state). The slot remains in `nodes`
    /// as a suspended [`StoppedTester`] that can be restarted later with [`Self::start_node`].
    pub async fn suspend_node(&mut self, index: usize) -> anyhow::Result<()> {
        tracing::info!(index, "suspending node...");
        let tester = self.nodes.remove(index);
        let stopped = match tester {
            NodeSlot::Running(tester) => tester.stop().await?,
            NodeSlot::Suspended(_) => panic!("node {index} is already suspended"),
        };
        self.nodes.insert(index, NodeSlot::Suspended(stopped));
        Ok(())
    }

    /// Restart a previously suspended node.
    pub async fn start_node(&mut self, index: usize) -> anyhow::Result<()> {
        tracing::info!(index, "starting suspended node...");
        let suspended = self.nodes.remove(index);
        let started = match suspended {
            NodeSlot::Suspended(tester) => tester.start().await?,
            NodeSlot::Running(_) => panic!("node {index} is not suspended"),
        };
        self.nodes.insert(index, NodeSlot::Running(started));
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

        let launches = self
            .consensus_secret_keys
            .into_iter()
            .take(num_nodes)
            .zip(locked_ports.into_iter())
            .enumerate()
            .map(|(i, (secret, locked_port))| {
                let peers = peer_ids.clone();
                let boot_nodes: Vec<zksync_os_network::TrustedPeer> =
                    node_records.iter().copied().map(Into::into).collect();
                let l1 = l1.clone();
                async move {
                    let network_port = locked_port.port;
                    // Launch bootstrap node last in configuration terms, but start every node concurrently.
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
                        "starting node..."
                    );

                    let node = Tester::launch_node_with_network_port(
                        l1,
                        false,
                        Some(move |config: &mut Config| {
                            config.general_config.node_role = NodeRole::MainNode;
                            config.general_config.main_node_rpc_url = None;
                            config.batcher_config.enabled = false;
                            config.batcher_config.enabled = false;
                            config.network_config.enabled = true;
                            config.network_config.secret_key = Some(secret);
                            config.network_config.address = Ipv4Addr::LOCALHOST;
                            config.network_config.port = network_port;
                            config.network_config.boot_nodes = boot_nodes.clone();
                            config.consensus_config.enabled = true;
                            config.consensus_config.bootstrap = bootstrap;
                            config.consensus_config.peer_ids = peers.clone();
                            // Keep elections fast, but leave enough jitter to avoid repeated
                            // split votes after a leader disappears in a 3-node cluster.
                            config.consensus_config.election_timeout_min =
                                TEST_ELECTION_TIMEOUT_MIN;
                            config.consensus_config.election_timeout_max =
                                TEST_ELECTION_TIMEOUT_MAX;
                            config.consensus_config.heartbeat_interval = TEST_HEARTBEAT_INTERVAL;
                        }),
                        ChainLayout::Default {
                            protocol_version: PROTOCOL_VERSION,
                        },
                        locked_port,
                        false,
                    )
                    .await?;
                    tracing::info!(
                        node_index = i,
                        %expected_node_id,
                        "node started with tempfile: {}",
                        node.tempdir.path().display()
                    );
                    anyhow::Ok(NodeSlot::Running(node))
                }
            });

        Ok(MultiNodeTester {
            nodes: try_join_all(launches).await?,
        })
    }
}
