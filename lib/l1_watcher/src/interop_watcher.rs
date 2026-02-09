use alloy::primitives::ruint::FromUintError;
use alloy::rpc::types::Log;
use alloy::{primitives::Address, providers::DynProvider};
use zksync_os_contract_interface::IMessageRoot::NewInteropRoot;
use zksync_os_contract_interface::{Bridgehub, InteropRoot};
use zksync_os_mempool::InteropTxPool;
use zksync_os_types::IndexedInteropRoot;

use crate::util::find_l1_block_by_interop_root_id;
use crate::watcher::{L1Watcher, L1WatcherError};
use crate::{L1WatcherConfig, ProcessL1Event};

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
            this.into(),
        ))
    }
}

#[async_trait::async_trait]
impl ProcessL1Event for InteropWatcher {
    const NAME: &'static str = "interop_root";

    type SolEvent = NewInteropRoot;
    type WatchedEvent = NewInteropRoot;

    fn contract_address(&self) -> Address {
        self.contract_address
    }

    async fn process_event(&mut self, tx: NewInteropRoot, _log: Log) -> Result<(), L1WatcherError> {
        if tx.logId < self.starting_interop_root_id {
            tracing::debug!(
                log_id = ?tx.logId,
                starting_interop_root_id = self.starting_interop_root_id,
                "skipping interop root event before starting index",
            );
            return Ok(());
        }
        let interop_root = InteropRoot {
            chainId: tx.chainId,
            blockOrBatchNumber: tx.blockNumber,
            sides: tx.sides.clone(),
        };

        self.tx_pool.add_root(IndexedInteropRoot {
            log_id: tx
                .logId
                .try_into()
                .map_err(|e: FromUintError<u64>| L1WatcherError::Other(e.into()))?,
            root: interop_root,
        });

        Ok(())
    }
}
