use crate::config::RaftConsensusConfig;
use crate::leadership_monitor::spawn_leadership_monitor;
use crate::model::{
    BlockCanonizationEngine, ConsensusRole, ConsensusRuntimeParts, LeadershipSignal,
    OpenRaftCanonizationEngine, RaftRuntimeExtras,
};
use crate::network::{RaftNetworkFactory, RaftRpcHandler};
use crate::state_machine::RaftStateMachineStore;
use crate::status::RaftConsensusStatus;
use crate::storage::RaftLogStore;
use anyhow::Context;
use openraft::{Config, Raft, SnapshotPolicy};
use reth_network_peers::PeerId;
use std::collections::BTreeMap;
use tokio::sync::{mpsc, watch};
use zksync_os_consensus_types::RaftNode;
use zksync_os_network::raft::protocol::RaftProtocolHandler;
use zksync_os_network::raft::protocol::RaftRouter;
use zksync_os_sequencer::execution::NoopCanonization;
use zksync_os_storage_api::ReadReplay;

/// Initialises the OpenRaft consensus engine and returns the runtime parts needed by the node.
///
/// `wal` is a read-only handle to the block replay WAL. The state machine uses it to derive
/// the last applied `LogId` directly from `wal.latest_record()`, keeping it atomically
/// consistent with what `BlockApplier` has durably persisted: a block is only considered
/// applied once it is in the WAL.
pub async fn init_consensus(
    config: RaftConsensusConfig,
    block_replay_storage: Box<dyn ReadReplay>,
) -> anyhow::Result<ConsensusRuntimeParts> {
    let router = RaftRouter::default();
    let node_id = config.node_id;
    let raft_config = Config {
        cluster_name: "zksync-os-server".to_owned(),
        snapshot_policy: SnapshotPolicy::Never,
        election_timeout_max: config.election_timeout_max.as_millis() as u64,
        election_timeout_min: config.election_timeout_min.as_millis() as u64,
        heartbeat_interval: config.heartbeat_interval.as_millis() as u64,
        ..Default::default()
    };

    let raft_config = raft_config.validate().context("invalid raft config")?;

    let log_store = RaftLogStore::open(&config.storage_path)?;
    let (canonized_sender, canonized_rx) = mpsc::unbounded_channel();
    let state_machine =
        RaftStateMachineStore::new(log_store.db(), block_replay_storage, canonized_sender);

    let nodes = peer_list_to_nodes(&config.peer_ids);
    let peer_ids: Vec<_> = nodes.keys().copied().collect();
    let network_factory = RaftNetworkFactory::new(router.clone(), &nodes, &raft_config)
        .context("build raft network factory")?;

    let raft = Raft::new(
        config.node_id,
        std::sync::Arc::new(raft_config),
        network_factory,
        log_store,
        state_machine,
    )
    .await?;

    let initial_metrics = raft.metrics().borrow().clone();
    tracing::info!(
        "openraft consensus initialized (node_id={node_id}, peers={}, bootstrap={}, last_applied={:?})",
        config.peer_ids.len(),
        config.bootstrap,
        initial_metrics.last_applied,
    );

    let (leader_tx, leader_rx) = watch::channel(ConsensusRole::Replica);
    let (status_tx, status_rx) = watch::channel::<Option<RaftConsensusStatus>>(None);
    spawn_leadership_monitor(raft.clone(), node_id.to_string(), leader_tx, status_tx);
    let rpc_handler = RaftRpcHandler::new(raft.clone());
    let protocol_handler = RaftProtocolHandler::new(rpc_handler, router.clone());

    let bootstrapper = if config.bootstrap {
        Some(crate::bootstrap::RaftBootstrapper {
            raft: raft.clone(),
            router,
            node_id,
            peer_ids,
            membership_nodes: nodes,
        })
    } else {
        None
    };

    Ok(ConsensusRuntimeParts {
        canonization_engine: BlockCanonizationEngine::OpenRaft(OpenRaftCanonizationEngine {
            raft: raft.clone(),
            canonized_blocks_rx: canonized_rx,
        }),
        leadership: LeadershipSignal::Watch(leader_rx),
        raft: Some(RaftRuntimeExtras {
            protocol_handler,
            bootstrapper,
            status_rx,
        }),
        raft_handle: Some(raft),
    })
}

pub fn loopback_consensus() -> ConsensusRuntimeParts {
    ConsensusRuntimeParts {
        canonization_engine: BlockCanonizationEngine::Noop(NoopCanonization::new()),
        leadership: LeadershipSignal::AlwaysLeader,
        raft: None,
        raft_handle: None,
    }
}

fn peer_list_to_nodes(peer_ids: &[PeerId]) -> BTreeMap<PeerId, RaftNode> {
    let mut nodes = BTreeMap::new();
    for peer_id in peer_ids {
        nodes.insert(
            *peer_id,
            RaftNode {
                addr: peer_id.to_string(),
            },
        );
        tracing::debug!("configured raft peer id: {peer_id}");
    }
    nodes
}
