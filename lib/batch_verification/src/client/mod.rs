use crate::client::metrics::BATCH_VERIFICATION_CLIENT_METRICS;
use crate::{
    BatchVerificationRequest, BatchVerificationRequestDecoder, BatchVerificationResponse,
    BatchVerificationResponseCodec, BatchVerificationResult,
};
use alloy::primitives::Address;
use alloy::signers::local::PrivateKeySigner;
use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use secrecy::{ExposeSecret, SecretString};
use std::str::FromStr;
use std::time::Duration;
use structdiff::StructDiff;
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;
use tokio_util::codec::{FramedRead, FramedWrite};
use zksync_os_batch_types::BatchSignature;
use zksync_os_batch_types::BlockMerkleTreeData;
use zksync_os_interface::types::BlockOutput;
use zksync_os_l1_sender::commitment::BatchInfo;
use zksync_os_merkle_tree::TreeBatchOutput;
use zksync_os_observability::ComponentStateHandle;
use zksync_os_observability::ComponentStateReporter;
use zksync_os_observability::GenericComponentState;
use zksync_os_observability::StateLabel;
use zksync_os_pipeline::{PeekableReceiver, PipelineComponent};
use zksync_os_socket::connect;
use zksync_os_storage_api::ReadFinality;
use zksync_os_storage_api::ReplayRecord;

mod block_cache;
mod metrics;

use block_cache::BlockCache;

/// Client that connects to the main sequencer for batch verification
pub struct BatchVerificationClient<Finality> {
    chain_id: u64,
    diamond_proxy: Address,
    server_address: String,
    signer: PrivateKeySigner,
    block_cache: BlockCache<Finality>,
}

#[derive(Debug, thiserror::Error)]
enum BatchVerificationError {
    #[error("Missing records for block {0}")]
    MissingBlock(u64),
    #[error("Tree error")]
    TreeError,
    #[error("Batch data mismatch: {0}")]
    BatchDataMismatch(String),
}

type VerificationInput = (
    BlockOutput,
    zksync_os_storage_api::ReplayRecord,
    BlockMerkleTreeData,
);

impl<Finality: ReadFinality> BatchVerificationClient<Finality> {
    pub fn new(
        finality: Finality,
        private_key: SecretString,
        chain_id: u64,
        diamond_proxy: Address,
        server_address: String,
    ) -> Self {
        Self {
            signer: PrivateKeySigner::from_str(private_key.expose_secret())
                .expect("Invalid batch verification private key"),
            chain_id,
            diamond_proxy,
            block_cache: BlockCache::new(finality),
            server_address,
        }
    }

    async fn connect_and_handle(
        &mut self,
        input: &mut PeekableReceiver<VerificationInput>,
        latency_tracker: &ComponentStateHandle<BatchVerificationClientState>,
    ) -> anyhow::Result<()> {
        let address = self.signer.address().to_string();
        let mut socket = connect(&self.server_address, "/batch_verification").await?;

        let batch_verification_version = socket.read_u32().await?;
        let (recv, send) = socket.split();
        let mut reader = FramedRead::new(
            recv,
            BatchVerificationRequestDecoder::new(batch_verification_version),
        );
        let mut writer = FramedWrite::new(
            send,
            BatchVerificationResponseCodec::new(batch_verification_version),
        );

        tracing::info!(
            address,
            "Connected to main sequencer for batch verification",
        );

        loop {
            latency_tracker.enter_state(BatchVerificationClientState::WaitingRecv);
            tokio::select! {
                block = input.recv() => {
                    match block {
                        Some((block_output, replay_record, tree_data)) => {
                            // we remove blocks from cache based on incoming singing requests.
                            // this prevent memory exhaustion / leak
                            self.block_cache.insert(
                                replay_record.block_context.block_number,
                                (block_output, replay_record, tree_data),
                            )?;
                        }
                        None => return Ok(()), // Channel closed, we are stopping now
                    }
                }
                // Handling in sequence without concurrency is fine as we shouldn't get too many requests and they should handle fast
                server_message = reader.next() => {
                    match server_message {
                        Some(Ok(message)) => {
                            latency_tracker.enter_state(BatchVerificationClientState::Processing);

                            let batch_number = message.batch_number;
                            let request_id = message.request_id;
                            let verification_result = self.handle_verification_request(message).await;

                            latency_tracker.enter_state(BatchVerificationClientState::WaitingSend);
                            match verification_result {
                                Ok(signature) => {
                                    tracing::info!(batch_number, request_id, address, "Approved batch verification request");
                                    BATCH_VERIFICATION_CLIENT_METRICS.record_request_success(request_id, batch_number);
                                    writer.send(BatchVerificationResponse { request_id, batch_number, result: BatchVerificationResult::Success(signature) }).await?;
                                },
                                Err(reason) => {
                                    tracing::info!(batch_number, request_id, address, "Batch verification failed: {}", reason);
                                    BATCH_VERIFICATION_CLIENT_METRICS.record_request_failure(request_id, batch_number);
                                    writer.send(BatchVerificationResponse { request_id, batch_number, result: BatchVerificationResult::Refused(reason.to_string()) }).await?;
                                },
                            }
                        }
                        Some(Err(parsing_err)) =>
                        {
                            tracing::error!("Error parsing verification request message. Ignoring: {}", parsing_err);
                        }
                        None => {
                            anyhow::bail!("Server has disconnected verification client");
                        }
                    }
                }
            }
        }
    }

    async fn handle_verification_request(
        &self,
        request: BatchVerificationRequest,
    ) -> Result<BatchSignature, BatchVerificationError> {
        tracing::info!(
            batch_number = request.batch_number,
            request_id = request.request_id,
            "Handling batch verification request (blocks {}-{})",
            request.first_block_number,
            request.last_block_number,
        );

        let blocks: Vec<(&BlockOutput, &ReplayRecord, TreeBatchOutput)> =
            (request.first_block_number..=request.last_block_number)
                .map(|block_number| {
                    let (block_output, replay_record, tree_data) = self
                        .block_cache
                        .get(block_number)
                        .ok_or(BatchVerificationError::MissingBlock(block_number))?;

                    let (root_hash, leaf_count) = tree_data
                        .block_end
                        .clone()
                        .root_info()
                        .map_err(|_| BatchVerificationError::TreeError)?;

                    let tree_output = TreeBatchOutput {
                        root_hash,
                        leaf_count,
                    };
                    Ok((block_output, replay_record, tree_output))
                })
                .collect::<Result<Vec<_>, BatchVerificationError>>()?;

        let commit_batch_info = BatchInfo::new(
            blocks
                .iter()
                .map(|(block_output, replay_record, tree)| {
                    (
                        *block_output,
                        &replay_record.block_context,
                        replay_record.transactions.as_slice(),
                        tree,
                    )
                })
                .collect(),
            self.chain_id,
            self.diamond_proxy,
            request.batch_number,
            request.pubdata_mode,
        )
        .commit_info;

        if commit_batch_info != request.commit_data {
            let diff = request.commit_data.diff(&commit_batch_info);

            return Err(BatchVerificationError::BatchDataMismatch(format!(
                "Batch data mismatch: {diff:?}",
            )));
        }

        let signature = BatchSignature::sign_batch(&request.commit_data, &self.signer).await;

        Ok(signature)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{Address, B256};
    use secrecy::SecretString;
    use tokio::sync::watch;
    use zksync_os_contract_interface::models::{CommitBatchInfo, DACommitmentScheme};
    use zksync_os_storage_api::FinalityStatus;
    use zksync_os_types::PubdataMode;

    struct DummyFinality {
        status: FinalityStatus,
        rx: watch::Receiver<FinalityStatus>,
    }

    impl DummyFinality {
        fn new() -> Self {
            let status = FinalityStatus {
                last_committed_block: 0,
                last_committed_batch: 0,
                last_executed_block: 0,
                last_executed_batch: 0,
            };
            let (tx, rx) = watch::channel(status.clone());
            let _ = tx;
            Self { status, rx }
        }
    }

    impl ReadFinality for DummyFinality {
        fn get_finality_status(&self) -> FinalityStatus {
            self.status.clone()
        }

        fn subscribe(&self) -> watch::Receiver<FinalityStatus> {
            self.rx.clone()
        }
    }

    fn dummy_commit_batch_info() -> CommitBatchInfo {
        CommitBatchInfo {
            batch_number: 1,
            new_state_commitment: B256::ZERO,
            number_of_layer1_txs: 0,
            priority_operations_hash: B256::ZERO,
            dependency_roots_rolling_hash: B256::ZERO,
            l2_to_l1_logs_root_hash: B256::ZERO,
            l2_da_commitment_scheme: DACommitmentScheme::BlobsAndPubdataKeccak256,
            da_commitment: B256::ZERO,
            first_block_timestamp: 0,
            first_block_number: Some(1),
            last_block_timestamp: 0,
            last_block_number: Some(1),
            chain_id: 270,
            operator_da_input: Vec::new(),
        }
    }

    fn dummy_request(first_block_number: u64, last_block_number: u64) -> BatchVerificationRequest {
        BatchVerificationRequest {
            batch_number: 1,
            first_block_number,
            last_block_number,
            pubdata_mode: PubdataMode::Calldata,
            request_id: 42,
            commit_data: dummy_commit_batch_info(),
        }
    }

    fn make_client() -> BatchVerificationClient<DummyFinality> {
        let finality = DummyFinality::new();
        let private_key = SecretString::new(
            "0xf9306dd03807c08b646d47c739bd51e4d2a25b02bad0efb3d93f095982ac98cd".into(),
        );
        let chain_id = 270u64;
        let diamond_proxy = Address::ZERO;
        let server_address = "127.0.0.1:0".to_string();

        BatchVerificationClient::new(
            finality,
            private_key,
            chain_id,
            diamond_proxy,
            server_address,
        )
    }

    #[tokio::test]
    async fn handle_verification_request_missing_block_returns_error() {
        let client = make_client();
        let first_block_number = 10u64;
        let request = dummy_request(first_block_number, first_block_number);

        let result = client.handle_verification_request(request).await;

        match result {
            Err(BatchVerificationError::MissingBlock(block)) => {
                assert_eq!(block, first_block_number);
            }
            _ => panic!("expected MissingBlock error"),
        }
    }
}

enum BatchVerificationClientState {
    Connecting,
    WaitingRecv,
    Processing,
    WaitingSend,
}

impl StateLabel for BatchVerificationClientState {
    fn generic(&self) -> GenericComponentState {
        match self {
            BatchVerificationClientState::Connecting => GenericComponentState::WaitingRecv,
            BatchVerificationClientState::WaitingRecv => GenericComponentState::WaitingRecv,
            BatchVerificationClientState::Processing => GenericComponentState::Processing,
            BatchVerificationClientState::WaitingSend => GenericComponentState::WaitingSend,
        }
    }

    fn specific(&self) -> &'static str {
        match self {
            BatchVerificationClientState::Connecting => "connecting",
            BatchVerificationClientState::WaitingRecv => {
                GenericComponentState::WaitingRecv.specific()
            }
            BatchVerificationClientState::Processing => {
                GenericComponentState::Processing.specific()
            }
            BatchVerificationClientState::WaitingSend => {
                GenericComponentState::WaitingSend.specific()
            }
        }
    }
}

#[async_trait]
impl<Finality: ReadFinality> PipelineComponent for BatchVerificationClient<Finality> {
    type Input = VerificationInput;
    type Output = ();

    const NAME: &'static str = "batch_verification_client";
    const OUTPUT_BUFFER_SIZE: usize = 5;

    async fn run(
        mut self,
        mut input: PeekableReceiver<Self::Input>,
        _output: mpsc::Sender<Self::Output>,
    ) -> anyhow::Result<()> {
        // Did not use backon due to borrowing issues
        let latency_tracker = ComponentStateReporter::global().handle_for(
            "batch_verification_client",
            BatchVerificationClientState::Connecting,
        );
        loop {
            let result = self.connect_and_handle(&mut input, &latency_tracker).await;

            match result {
                Ok(()) => {
                    // Normal shutdown - input channel closed
                    return Ok(());
                }
                Err(err) => {
                    latency_tracker.enter_state(BatchVerificationClientState::Connecting);
                    tracing::info!(
                        ?err,
                        "Connection to batch verification server closed. Reconnecting in 5 seconds..."
                    );
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    // Continue loop to retry
                }
            }
        }
    }
}
