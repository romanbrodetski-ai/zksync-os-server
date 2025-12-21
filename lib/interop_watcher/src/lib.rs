use std::time::Duration;

use alloy::rpc::types::Filter;
use alloy::sol_types::SolEvent;
use alloy::{
    primitives::Address,
    providers::{DynProvider, Provider},
};
use tokio::sync::mpsc;
use zksync_os_contract_interface::{InteropRoot, NewInteropRoot};
use zksync_os_types::InteropRootsEnvelope;
pub const INTEROP_ROOTS_PER_IMPORT: u64 = 100;

pub struct L1InteropRootsWatcher {
    contract_address: Address,

    provider: DynProvider,
    // first number is block number, second is log index
    next_log_to_scan_from: (u64, u64),

    poll_interval: Duration,

    output: mpsc::Sender<InteropRootsEnvelope>,
}

impl L1InteropRootsWatcher {
    pub async fn new(
        provider: DynProvider,
        contract_address: Address,
        next_log_to_scan_from: (u64, u64),
        poll_interval: Duration,
        output: mpsc::Sender<InteropRootsEnvelope>,
    ) -> Self {
        Self {
            provider,
            contract_address,
            next_log_to_scan_from,
            poll_interval,
            output,
        }
    }

    pub async fn run(mut self) -> anyhow::Result<()> {
        let mut timer = tokio::time::interval(self.poll_interval);
        loop {
            timer.tick().await;
            self.poll().await?;
        }
    }

    async fn fetch_events(
        &mut self,
        from_block: u64,
        to_block: u64,
        start_log_index: u64,
    ) -> anyhow::Result<Vec<InteropRoot>> {
        let filter = Filter::new()
            .from_block(from_block)
            .to_block(to_block)
            .address(self.contract_address)
            .event_signature(NewInteropRoot::SIGNATURE_HASH);
        let logs = self.provider.get_logs(&filter).await?;

        let mut interop_roots = Vec::new();
        for log in logs {
            let log_block_number = log.block_number.unwrap();
            let log_index_in_block = log.log_index.unwrap();

            if log_block_number == from_block && log_index_in_block <= start_log_index {
                continue;
            }
            let interop_root_event = NewInteropRoot::decode_log(&log.inner)?.data;

            if interop_root_event.sides.len() != 1 {
                anyhow::bail!("Expected 1 side, got {}", interop_root_event.sides.len());
            }

            let interop_root = InteropRoot {
                chainId: interop_root_event.chainId,
                blockOrBatchNumber: interop_root_event.blockNumber,
                sides: interop_root_event.sides,
            };
            interop_roots.push(interop_root);

            self.next_log_to_scan_from = (log_block_number, log_index_in_block + 1);

            if interop_roots.len() >= INTEROP_ROOTS_PER_IMPORT as usize {
                break;
            }
        }

        // if we didn't get enough interop roots, it should be safe to continue from the last block we already scanned
        // edge case would be if the last root we included was already in the last block, then we should leave the value as is(it was updated before)
        if interop_roots.len() < INTEROP_ROOTS_PER_IMPORT as usize
            && self.next_log_to_scan_from.0 < to_block
        {
            self.next_log_to_scan_from = (to_block, 0);
        }

        Ok(interop_roots)
    }

    async fn poll(&mut self) -> anyhow::Result<()> {
        let latest_block = self.provider.get_block_number().await?;

        Ok(())
    }
}
