use crate::watcher::{L1Watcher, L1WatcherError};
use crate::{L1WatcherConfig, ProcessL1Event, util};
use alloy::primitives::Address;
use alloy::providers::{DynProvider, Provider};
use alloy::rpc::types::Log;
use anyhow::Context;
use zksync_os_contract_interface::IExecutor::BlockExecution;
use zksync_os_contract_interface::ZkChain;
use zksync_os_storage_api::WriteFinality;

pub struct L1ExecuteWatcher<Finality> {
    zk_chain: ZkChain<DynProvider>,
    next_batch_number: u64,
    finality: Finality,
    max_blocks_to_process: u64,
}

impl<Finality: WriteFinality> L1ExecuteWatcher<Finality> {
    pub async fn create_watcher(
        config: L1WatcherConfig,
        zk_chain: ZkChain<DynProvider>,
        finality: Finality,
    ) -> anyhow::Result<L1Watcher> {
        let current_l1_block = zk_chain.provider().get_block_number().await?;
        let last_executed_batch = finality.get_finality_status().last_executed_batch;
        tracing::info!(
            current_l1_block,
            last_executed_batch,
            config.max_blocks_to_process,
            ?config.poll_interval,
            zk_chain_address = ?zk_chain.address(),
            "initializing L1 execute watcher"
        );
        let last_l1_block =
            util::find_l1_execute_block_by_batch_number(zk_chain.clone(), last_executed_batch)
                .await?;
        tracing::info!(last_l1_block, "resolved on L1");

        let this = Self {
            zk_chain: zk_chain.clone(),
            next_batch_number: last_executed_batch + 1,
            finality,
            max_blocks_to_process: config.max_blocks_to_process,
        };
        let l1_watcher = L1Watcher::new(
            zk_chain.provider().clone(),
            // We start from last L1 block as it may contain more executed batches apart from the last
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
impl<Finality: WriteFinality> ProcessL1Event for L1ExecuteWatcher<Finality> {
    const NAME: &'static str = "block_execution";

    type SolEvent = BlockExecution;
    type WatchedEvent = BlockExecution;

    fn contract_address(&self) -> Address {
        *self.zk_chain.address()
    }

    async fn process_event(
        &mut self,
        batch_execute: BlockExecution,
        _log: Log,
    ) -> Result<(), L1WatcherError> {
        let batch_number = batch_execute.batchNumber.to::<u64>();
        let batch_hash = batch_execute.batchHash;
        let batch_commitment = batch_execute.commitment;
        if batch_number < self.next_batch_number {
            tracing::debug!(
                batch_number,
                ?batch_hash,
                ?batch_commitment,
                "skipping already processed executed batch",
            );
        } else {
            // todo: This can take a while. For the majority of batches we have already pulled the
            //       range in commit watcher, we should find a way to reuse it.
            let batch = util::find_stored_batch_data_by_batch_number(
                &self.zk_chain,
                batch_number,
                self.max_blocks_to_process,
            )
            .await
            .unwrap()
            .context("could not find where batch was committed on L1")
            .unwrap();
            let last_executed_block = batch.last_block_number;
            self.finality.update_finality_status(|finality| {
                assert!(
                    batch_number > finality.last_executed_batch,
                    "non-monotonous executed batch"
                );
                assert!(
                    last_executed_block > finality.last_executed_block,
                    "non-monotonous executed block"
                );
                finality.last_executed_batch = batch_number;
                finality.last_executed_block = last_executed_block;
            });
            tracing::debug!(
                batch_number,
                ?batch_hash,
                ?batch_commitment,
                last_executed_block,
                "discovered executed batch"
            );
        }
        Ok(())
    }
}
