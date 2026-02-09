use std::collections::HashMap;

use alloy::primitives::ruint::FromUintError;
use alloy::rpc::types::{Log, Topic, ValueOrArray};
use alloy::sol_types::SolEvent;
use alloy::{primitives::Address, providers::DynProvider};
use zksync_os_contract_interface::IMessageRoot::NewInteropRoot;
use zksync_os_contract_interface::{Bridgehub, InteropRoot};
use zksync_os_mempool::InteropTxPool;
use zksync_os_types::IndexedInteropRoot;

use crate::util::find_l1_block_by_interop_root_id;
use crate::watcher::{L1Watcher, L1WatcherError};
use crate::{L1WatcherConfig, ProcessRawEvents};

pub struct InteropWatcher {
    contract_address: Address,
    starting_interop_root_id: u64,
    tx_pool: InteropTxPool,
}

impl InteropWatcher {
    pub async fn create_watcher(
        bridgehub: Bridgehub<DynProvider>,
        config: L1WatcherConfig,
        starting_interop_root_id: u64,
        tx_pool: InteropTxPool,
    ) -> anyhow::Result<L1Watcher> {
        let contract_address = bridgehub.message_root_address().await?;

        tracing::info!(
            contract_address = ?contract_address,
            starting_interop_root_id = ?starting_interop_root_id,
            "initializing interop watcher"
        );

        let this = Self {
            contract_address,
            starting_interop_root_id,
            tx_pool,
        };

        let next_l1_block =
            find_l1_block_by_interop_root_id(bridgehub.clone(), starting_interop_root_id).await?;

        Ok(L1Watcher::new(
            bridgehub.provider().clone(),
            next_l1_block,
            config.max_blocks_to_process,
            config.poll_interval,
            Box::new(this),
        ))
    }
}

#[async_trait::async_trait]
impl ProcessRawEvents for InteropWatcher {
    fn name(&self) -> &'static str {
        "interop_root"
    }

    fn event_signatures(&self) -> Topic {
        NewInteropRoot::SIGNATURE_HASH.into()
    }

    fn contract_addresses(&self) -> ValueOrArray<Address> {
        self.contract_address.into()
    }

    fn filter_events(&self, logs: Vec<Log>) -> Vec<Log> {
        // we want to accept only the latest event for each log id
        let mut indexes = HashMap::new();

        for log in logs {
            let sol_event = NewInteropRoot::decode_log(&log.inner)
                .expect("failed to decode log")
                .data;
            indexes.insert(sol_event.logId, log);
        }

        indexes.into_values().collect()
    }

    async fn process_raw_event(&mut self, log: Log) -> Result<(), L1WatcherError> {
        let event = NewInteropRoot::decode_log(&log.inner)?.data;

        if event.logId < self.starting_interop_root_id {
            tracing::debug!(
                log_id = ?event.logId,
                starting_interop_root_id = self.starting_interop_root_id,
                "skipping interop root event before starting index",
            );
            return Ok(());
        }
        let interop_root = InteropRoot {
            chainId: event.chainId,
            blockOrBatchNumber: event.blockNumber,
            sides: event.sides.clone(),
        };

        self.tx_pool.add_root(IndexedInteropRoot {
            log_id: event
                .logId
                .try_into()
                .map_err(|e: FromUintError<u64>| L1WatcherError::Other(e.into()))?,
            root: interop_root,
        });
        Ok(())
    }
}
