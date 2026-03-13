//! Noop network factory for single-node consensus.
//!
//! OpenRaft requires a `RaftNetworkFactory` even when running as a single member.
//! This implementation always returns `Unreachable` errors since there are no peers
//! to communicate with. The real networked implementation will be added when
//! multi-node consensus support lands.

use openraft::error::RaftError;
use openraft::error::{Fatal, RPCError, ReplicationClosed, StreamingError, Unreachable};
use openraft::network::{RPCOption, RaftNetwork, RaftNetworkFactory};
use openraft::raft::{
    AppendEntriesRequest, AppendEntriesResponse, InstallSnapshotRequest, InstallSnapshotResponse,
    SnapshotResponse, VoteRequest, VoteResponse,
};
use openraft::{Snapshot, Vote};
use reth_network_peers::PeerId;
use std::future::Future;
use zksync_os_consensus_types::{RaftNode, RaftTypeConfig};

/// Network factory that creates clients which always fail.
/// Used for single-node consensus where no network communication is needed.
#[derive(Clone)]
pub struct NoopNetworkFactory;

impl RaftNetworkFactory<RaftTypeConfig> for NoopNetworkFactory {
    type Network = NoopNetworkClient;

    fn new_client(
        &mut self,
        _target: PeerId,
        _node: &RaftNode,
    ) -> impl Future<Output = Self::Network> + Send {
        async move { NoopNetworkClient }
    }
}

/// Network client that always returns unreachable errors.
#[derive(Clone)]
pub struct NoopNetworkClient;

fn unreachable_err<E: std::error::Error>() -> RPCError<PeerId, RaftNode, E> {
    let err = std::io::Error::other("single-node consensus: no network peers");
    RPCError::Unreachable(Unreachable::new(&err))
}

impl RaftNetwork<RaftTypeConfig> for NoopNetworkClient {
    fn append_entries(
        &mut self,
        _rpc: AppendEntriesRequest<RaftTypeConfig>,
        _option: RPCOption,
    ) -> impl Future<
        Output = Result<
            AppendEntriesResponse<PeerId>,
            RPCError<PeerId, RaftNode, RaftError<PeerId>>,
        >,
    > + Send {
        async move { Err(unreachable_err()) }
    }

    fn vote(
        &mut self,
        _rpc: VoteRequest<PeerId>,
        _option: RPCOption,
    ) -> impl Future<
        Output = Result<VoteResponse<PeerId>, RPCError<PeerId, RaftNode, RaftError<PeerId>>>,
    > + Send {
        async move { Err(unreachable_err()) }
    }

    fn install_snapshot(
        &mut self,
        _rpc: InstallSnapshotRequest<RaftTypeConfig>,
        _option: RPCOption,
    ) -> impl Future<
        Output = Result<
            InstallSnapshotResponse<PeerId>,
            RPCError<PeerId, RaftNode, RaftError<PeerId, openraft::error::InstallSnapshotError>>,
        >,
    > + Send {
        async move { Err(unreachable_err()) }
    }

    fn full_snapshot(
        &mut self,
        _vote: Vote<PeerId>,
        _snapshot: Snapshot<RaftTypeConfig>,
        _cancel: impl Future<Output = ReplicationClosed> + Send + 'static,
        _option: RPCOption,
    ) -> impl Future<
        Output = Result<SnapshotResponse<PeerId>, StreamingError<RaftTypeConfig, Fatal<PeerId>>>,
    > + Send {
        async move {
            let err = std::io::Error::other("single-node consensus: snapshotting disabled");
            Err(StreamingError::Unreachable(Unreachable::new(&err)))
        }
    }
}
