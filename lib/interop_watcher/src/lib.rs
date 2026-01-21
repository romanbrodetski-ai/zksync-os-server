use std::time::Duration;

use alloy::rpc::types::{Filter, Log};
use alloy::sol_types::SolEvent;
use alloy::{
    primitives::Address,
    providers::{DynProvider, Provider},
};
use tokio::sync::mpsc;
use zksync_os_contract_interface::IMessageRoot::NewInteropRoot;
use zksync_os_contract_interface::{Bridgehub, InteropRoot};
use zksync_os_types::{InteropRootsEnvelope, InteropRootsLogIndex};

pub const INTEROP_ROOTS_PER_IMPORT: u64 = 100;

const TOO_MANY_RESULTS_INFURA: &str = "query returned more than";
const TOO_MANY_RESULTS_ALCHEMY: &str = "response size exceeded";
const TOO_MANY_RESULTS_RETH: &str = "length limit exceeded";
const TOO_BIG_RANGE_RETH: &str = "query exceeds max block range";
const TOO_MANY_RESULTS_CHAINSTACK: &str = "range limit exceeded";
const REQUEST_REJECTED_503: &str = "Request rejected `503`";
const RETRY_LIMIT: u8 = 5;

pub struct InteropRootsWatcher {
    contract_address: Address,
    provider: DynProvider,
    // first number is block number, second is log index
    next_log_to_scan_from: InteropRootsLogIndex,
    output: mpsc::Sender<InteropRootsEnvelope>,
}

impl InteropRootsWatcher {
    pub async fn new(
        bridgehub: Bridgehub<DynProvider>,
        output: mpsc::Sender<InteropRootsEnvelope>,
        next_log_to_scan_from: InteropRootsLogIndex,
    ) -> anyhow::Result<Self> {
        let provider = bridgehub.provider().clone();
        let contract_address = bridgehub
            .message_root()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to get message root: {}", e))?;

        Ok(Self {
            provider,
            contract_address,
            next_log_to_scan_from,
            output,
        })
    }

    pub async fn run(mut self) -> anyhow::Result<()> {
        let mut timer = tokio::time::interval(Duration::from_secs(5));
        loop {
            timer.tick().await;
            self.poll().await?;
        }
    }

    async fn fetch_events(
        &mut self,
        start_log_index: InteropRootsLogIndex,
        to_block: u64,
    ) -> anyhow::Result<(Vec<InteropRoot>, InteropRootsLogIndex)> {
        let logs = self
            .get_events_recursively(start_log_index.block_number, to_block, RETRY_LIMIT)
            .await?;

        if logs.is_empty() {
            return Ok((Vec::new(), InteropRootsLogIndex::default()));
        }

        let mut interop_roots = Vec::new();
        for log in &logs {
            let log_index = InteropRootsLogIndex {
                block_number: log.block_number.unwrap(),
                index_in_block: log.log_index.unwrap(),
            };

            if log_index < start_log_index {
                continue;
            }
            let interop_root_event = NewInteropRoot::decode_log(&log.inner)?.data;

            anyhow::ensure!(
                interop_root_event.sides.len() == 1,
                "Expected exactly one side for interop root, found {}",
                interop_root_event.sides.len()
            );

            let interop_root = InteropRoot {
                chainId: interop_root_event.chainId,
                blockOrBatchNumber: interop_root_event.blockNumber,
                sides: interop_root_event.sides,
            };

            interop_roots.push(interop_root);

            self.next_log_to_scan_from = InteropRootsLogIndex {
                block_number: log_index.block_number,
                index_in_block: log_index.index_in_block + 1,
            };

            if interop_roots.len() >= INTEROP_ROOTS_PER_IMPORT as usize {
                break;
            }
        }

        // if we didn't get enough interop roots, it should be safe to continue from the last block we already scanned
        // edge case would be if the last root we included was already in the last block, then we should leave the value as is(it was updated before)
        if interop_roots.len() < INTEROP_ROOTS_PER_IMPORT as usize
            && self.next_log_to_scan_from.block_number < to_block
        {
            self.next_log_to_scan_from = InteropRootsLogIndex {
                block_number: to_block,
                index_in_block: 0,
            };
        }

        let last_log = logs.last().unwrap();

        let last_log_index = InteropRootsLogIndex {
            block_number: last_log.block_number.unwrap(),
            index_in_block: last_log.log_index.unwrap(),
        };

        Ok((interop_roots, last_log_index))
    }

    // this was mostly copy-pasted from zksync-era, since we want to capture as much of logs as possible
    #[async_recursion::async_recursion]
    async fn get_events_recursively(
        &mut self,
        from_block: u64,
        to_block: u64,
        retries_left: u8,
    ) -> anyhow::Result<Vec<Log>> {
        let filter = Filter::new()
            .from_block(from_block)
            .to_block(to_block)
            .address(self.contract_address)
            .event_signature(NewInteropRoot::SIGNATURE_HASH);

        let result = self.provider.get_logs(&filter).await;

        match result {
            Ok(logs) => return Ok(logs),
            Err(err) => {
                if let Some(error_resp) = err.as_error_resp()
                    && (error_resp.message.contains(TOO_MANY_RESULTS_INFURA)
                        || error_resp.message.contains(TOO_MANY_RESULTS_ALCHEMY)
                        || error_resp.message.contains(TOO_MANY_RESULTS_RETH)
                        || error_resp.message.contains(TOO_BIG_RANGE_RETH)
                        || error_resp.message.contains(TOO_MANY_RESULTS_CHAINSTACK)
                        || error_resp.message.contains(REQUEST_REJECTED_503))
                // maybe here also should be timeout
                {
                    let mid = (from_block + to_block) / 2;
                    let mut first_half = self
                        .get_events_recursively(from_block, mid, RETRY_LIMIT)
                        .await?;
                    let mut second_half = self
                        .get_events_recursively(mid + 1, to_block, RETRY_LIMIT)
                        .await?;
                    first_half.append(&mut second_half);
                    return Ok(first_half);
                } else {
                    if let Some(error) = err.as_transport_err()
                        && error.is_retry_err()
                        && retries_left > 0
                    {
                        return self
                            .get_events_recursively(from_block, to_block, retries_left - 1)
                            .await;
                    } else {
                        return Err(anyhow::anyhow!("Failed to get events: {}", err));
                    }
                }
            }
        }
    }

    async fn poll(&mut self) -> anyhow::Result<()> {
        let latest_block = self.provider.get_block_number().await?;

        let (interop_roots, last_log_index) = self
            .fetch_events(self.next_log_to_scan_from.clone(), latest_block)
            .await?;

        if !interop_roots.is_empty() {
            self.output
                .send(InteropRootsEnvelope::from_interop_roots(
                    interop_roots,
                    last_log_index,
                ))
                .await?;
        }

        Ok(())
    }
}
