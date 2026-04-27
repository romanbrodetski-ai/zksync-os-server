use crate::model::ConsensusRole;
use crate::status::RaftConsensusStatus;
use openraft::{Raft, ServerState};
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::timeout;
use zksync_os_consensus_types::RaftTypeConfig;

/// Spawns a background task that translates OpenRaft metrics into two node-facing signals:
/// a coarse `ConsensusRole` watch channel used by the sequencer, and a richer
/// `RaftConsensusStatus` watch channel exposed by the status server.
///
/// OpenRaft may briefly report `Leader` while a node is still replaying committed entries after
/// an election. To avoid producing blocks too early, this monitor only upgrades the node to
/// `ConsensusRole::Leader` after `ensure_linearizable()` succeeds within a short timeout.
/// If the node steps down or the confirmation probe fails, the role falls back to `Replica`.
///
/// The task exits automatically when the OpenRaft metrics channel closes or when all receivers
/// for both output watch channels are dropped.
pub fn spawn_leadership_monitor(
    raft: Raft<RaftTypeConfig>,
    node_id_str: String,
    leader_tx: watch::Sender<ConsensusRole>,
    status_tx: watch::Sender<Option<RaftConsensusStatus>>,
) {
    let mut metrics_rx = raft.metrics();
    tokio::spawn(async move {
        let mut last_logged = None;
        let mut leader_confirmed = false;
        let mut prev_role = ConsensusRole::Replica;

        loop {
            if metrics_rx.changed().await.is_err() {
                panic!("OpenRaft metrics channel closed unexpectedly; consensus subsystem is gone");
            }
            let metrics = metrics_rx.borrow().clone();

            let log_key = (metrics.state, metrics.current_term, metrics.current_leader);
            if last_logged != Some(log_key) {
                tracing::debug!(
                    "OpenRaft metrics changed: state={:?}, term={}, leader={:?}",
                    metrics.state,
                    metrics.current_term,
                    metrics.current_leader
                );
                last_logged = Some(log_key);
            }

            let claims_leader = matches!(metrics.state, ServerState::Leader);
            if !claims_leader {
                leader_confirmed = false;
            } else if !leader_confirmed {
                // Metrics show Leader, but that can happen transiently while the node is
                // replaying committed logs after an election. `ensure_linearizable()` does
                // a quorum round-trip that only succeeds once we hold an active lease,
                // confirming we are the current leader and can safely produce blocks.
                leader_confirmed =
                    match timeout(Duration::from_secs(2), raft.ensure_linearizable()).await {
                        Ok(Ok(_)) => {
                            tracing::info!("OpenRaft leader confirmation succeeded");
                            true
                        }
                        Ok(Err(err)) => {
                            tracing::warn!("OpenRaft leader confirmation failed: {err}");
                            false
                        }
                        Err(_) => {
                            tracing::warn!("OpenRaft leader confirmation timed out");
                            false
                        }
                    };
            }

            let role = if claims_leader && leader_confirmed {
                ConsensusRole::Leader
            } else {
                ConsensusRole::Replica
            };
            if role != prev_role {
                tracing::info!("OpenRaft leadership status changed: {role:?}");
                prev_role = role;
            }

            let status = RaftConsensusStatus {
                node_id: node_id_str.clone(),
                state: format!("{:?}", metrics.state),
                is_leader: role == ConsensusRole::Leader,
                current_leader: metrics.current_leader.map(|id| id.to_string()),
                current_term: metrics.current_term,
                last_applied_index: metrics.last_applied.map(|id| id.index),
            };
            // status_tx may have no receivers if the status server is disabled; that's fine.
            let _ = status_tx.send(Some(status));
            if leader_tx.send(role).is_err() {
                break;
            }
        }
    });
}
