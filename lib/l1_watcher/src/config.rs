use alloy::primitives::B256;
use std::time::Duration;

/// Configuration of L1 watcher.
#[derive(Clone, Debug)]
pub struct L1WatcherConfig {
    /// Max number of L1 blocks to be processed at a time.
    pub max_blocks_to_process: u64,

    /// How often to poll L1 for new priority requests.
    pub poll_interval: Duration,

    /// List of L1 transaction hashes to ignore when processing events.
    pub ignored_l1_tx_hashes: Vec<B256>,
}
