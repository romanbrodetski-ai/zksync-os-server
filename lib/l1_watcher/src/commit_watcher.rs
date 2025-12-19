use crate::watcher::{L1Watcher, L1WatcherError};
use crate::{L1WatcherConfig, ProcessL1Event, util};
use alloy::primitives::Address;
use alloy::providers::{DynProvider, Provider};
use alloy::rpc::types::Log;
use zksync_os_contract_interface::IExecutor::{BlockCommit, ReportCommittedBatchRangeZKsyncOS};
use zksync_os_contract_interface::ZkChain;
use zksync_os_storage_api::WriteFinality;

pub struct L1CommitWatcher<Finality> {
    provider: DynProvider,
    contract_address: Address,
    next_batch_number: u64,
    finality: Finality,
}

impl<Finality: WriteFinality> L1CommitWatcher<Finality> {
    pub async fn create_watcher(
        config: L1WatcherConfig,
        zk_chain: ZkChain<DynProvider>,
        finality: Finality,
    ) -> anyhow::Result<L1Watcher> {
        let current_l1_block = zk_chain.provider().get_block_number().await?;
        let last_committed_batch = finality.get_finality_status().last_committed_batch;
        tracing::info!(
            current_l1_block,
            last_committed_batch,
            config.max_blocks_to_process,
            ?config.poll_interval,
            zk_chain_address = ?zk_chain.address(),
            "initializing L1 commit watcher"
        );
        let last_l1_block = util::find_l1_commit_block_by_batch_number(
            zk_chain.clone(),
            last_committed_batch,
            config.max_blocks_to_process,
        )
        .await?;
        tracing::info!(last_l1_block, "resolved on L1");

        let this = Self {
            provider: zk_chain.provider().clone(),
            contract_address: *zk_chain.address(),
            next_batch_number: last_committed_batch + 1,
            finality,
        };
        let l1_watcher = L1Watcher::new(
            zk_chain.provider().clone(),
            // We start from last L1 block as it may contain more committed batches apart from the last
            // one.
            last_l1_block,
            config.max_blocks_to_process,
            config.poll_interval,
            this.into(),
        );

        Ok(l1_watcher)
    }
}

#[async_trait::async_trait]
impl<Finality: WriteFinality> ProcessL1Event for L1CommitWatcher<Finality> {
    const NAME: &'static str = "block_commit";

    type SolEvent = BlockCommit;
    type WatchedEvent = BlockCommit;

    fn contract_address(&self) -> Address {
        self.contract_address
    }

    async fn process_event(
        &mut self,
        batch_commit: BlockCommit,
        log: Log,
    ) -> Result<(), L1WatcherError> {
        let batch_number = batch_commit.batchNumber.to::<u64>();
        let batch_hash = batch_commit.batchHash;
        let batch_commitment = batch_commit.commitment;
        if batch_number < self.next_batch_number {
            tracing::debug!(
                batch_number,
                ?batch_hash,
                ?batch_commitment,
                "skipping already processed committed batch",
            );
        } else {
            tracing::debug!(
                batch_number,
                ?batch_hash,
                ?batch_commitment,
                "discovered committed batch"
            );
            let tx_hash = log
                .transaction_hash
                .expect("indexed log does not belong to any transaction");
            // todo: retry-backoff logic in case tx is missing
            let tx_receipt = self
                .provider
                .get_transaction_receipt(tx_hash)
                .await?
                .expect("tx not found");
            let report = tx_receipt
                .inner
                .into_logs()
                .into_iter()
                .find_map(|log| {
                    let log = log.log_decode::<ReportCommittedBatchRangeZKsyncOS>().ok()?;
                    if log.inner.data.batchNumber == batch_number {
                        Some(log)
                    } else {
                        None
                    }
                })
                .expect("report range not found");
            let last_committed_block = report.inner.lastBlockNumber;
            self.finality.update_finality_status(|finality| {
                assert!(
                    batch_number > finality.last_committed_batch,
                    "non-monotonous committed batch"
                );
                assert!(
                    last_committed_block > finality.last_committed_block,
                    "non-monotonous committed block"
                );
                finality.last_committed_batch = batch_number;
                finality.last_committed_block = last_committed_block;
            });
        }
        Ok(())
    }
}
