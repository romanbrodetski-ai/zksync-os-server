// OpenRaft trait impls use `-> impl Future` instead of `async fn`.
#![allow(clippy::manual_async_fn)]
// OpenRaft's StorageError is large by design.
#![allow(clippy::result_large_err)]

pub mod config;
pub mod init;
pub mod model;
pub mod network;
mod state_machine;
pub mod status;
pub mod storage;

pub use config::RaftConsensusConfig;
pub use init::{init_consensus, loopback_consensus};
pub use model::{
    BlockCanonizationEngine, ConsensusBootstrapper, ConsensusNetworkProtocol, ConsensusRole,
    ConsensusRuntimeParts, ConsensusStatusSource, LeadershipSignal, OpenRaftCanonizationEngine,
};
pub use state_machine::RaftStateMachineStore;
pub use status::RaftConsensusStatus;
pub use storage::RaftLogStore;
