use alloy::consensus::transaction::{Recovered, TransactionInfo};
use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, IntoLogData, TxHash, U256};
use alloy::providers::Provider;
use alloy::pubsub::Subscription;
use alloy::rpc::json_rpc::RpcRecv;
use alloy::rpc::types::{Filter, Header, Log, Transaction, TransactionRequest};
use alloy::sol_types::SolEvent;
use futures::StreamExt;
use tokio::time::error::Elapsed;
use zksync_os_integration_tests::assert_traits::ReceiptAssert;
use zksync_os_integration_tests::contracts::EventEmitter;
use zksync_os_integration_tests::contracts::EventEmitter::{EventEmitterInstance, TestEvent};
use zksync_os_integration_tests::dyn_wallet_provider::EthDynProvider;
use zksync_os_integration_tests::{
    CURRENT_TO_L1, NEXT_TO_GATEWAY, NEXT_TO_L1, Tester, test_casing,
};

trait PubsubSuite: Sized {
    type Expected: RpcRecv + PartialEq;
    async fn init(tester: &Tester) -> anyhow::Result<Self>;
    async fn subscribe(&self, tester: &Tester) -> anyhow::Result<Subscription<Self::Expected>>;
    async fn prepare_expected(&self, tester: &Tester) -> anyhow::Result<Self::Expected>;
}

async fn run_test<S: PubsubSuite>(tester: Tester) -> anyhow::Result<()> {
    let suite = S::init(&tester).await?;
    let mut stream = suite.subscribe(&tester).await?.into_stream();
    let expected_item = suite.prepare_expected(&tester).await?;

    let actual_item = stream.next().await.expect("stream ended unexpectedly");
    assert_eq!(actual_item, expected_item);

    let _: Elapsed = tokio::time::timeout(std::time::Duration::from_secs(1), stream.next())
        .await
        .expect_err("stream should not return anything after expected item");

    Ok(())
}

struct NewBlockSuite;

impl PubsubSuite for NewBlockSuite {
    type Expected = Header;
    async fn init(_tester: &Tester) -> anyhow::Result<Self> {
        Ok(NewBlockSuite)
    }
    async fn subscribe(&self, tester: &Tester) -> anyhow::Result<Subscription<Self::Expected>> {
        Ok(tester.l2_provider.subscribe_blocks().await?)
    }
    async fn prepare_expected(&self, tester: &Tester) -> anyhow::Result<Self::Expected> {
        let receipt = tester
            .l2_provider
            .send_transaction(
                TransactionRequest::default()
                    .with_to(Address::random())
                    .with_value(U256::from(100)),
            )
            .await?
            .expect_successful_receipt()
            .await?;
        let block = tester
            .l2_provider
            .get_block_by_hash(receipt.block_hash.expect("receipt has no block hash"))
            .hashes()
            .await?
            .expect("could not retrieve block header");
        Ok(block.header)
    }
}

struct PendingTxSuite<const FULL: bool>;

impl PubsubSuite for PendingTxSuite<false> {
    type Expected = TxHash;
    async fn init(_tester: &Tester) -> anyhow::Result<Self> {
        Ok(PendingTxSuite)
    }
    async fn subscribe(&self, tester: &Tester) -> anyhow::Result<Subscription<Self::Expected>> {
        Ok(tester.l2_provider.subscribe_pending_transactions().await?)
    }
    async fn prepare_expected(&self, tester: &Tester) -> anyhow::Result<Self::Expected> {
        let pending_tx = tester
            .l2_provider
            .send_transaction(
                TransactionRequest::default()
                    .with_to(Address::random())
                    .with_value(U256::from(100)),
            )
            .await?
            .expect_register()
            .await?;
        Ok(*pending_tx.tx_hash())
    }
}

impl PubsubSuite for PendingTxSuite<true> {
    type Expected = Transaction;
    async fn init(_tester: &Tester) -> anyhow::Result<Self> {
        Ok(PendingTxSuite)
    }
    async fn subscribe(&self, tester: &Tester) -> anyhow::Result<Subscription<Self::Expected>> {
        Ok(tester
            .l2_provider
            .subscribe_full_pending_transactions()
            .await?)
    }
    async fn prepare_expected(&self, tester: &Tester) -> anyhow::Result<Self::Expected> {
        let fees = tester.l2_provider.estimate_eip1559_fees().await?;
        let from = tester.l2_wallet.default_signer().address();
        let nonce = tester.l2_provider.get_transaction_count(from).await?;
        let tx_envelope = TransactionRequest::default()
            .with_to(Address::random())
            .with_value(U256::from(100))
            .with_nonce(nonce)
            .with_gas_limit(100_000)
            .with_max_fee_per_gas(fees.max_fee_per_gas)
            .with_max_priority_fee_per_gas(fees.max_priority_fee_per_gas)
            .with_chain_id(tester.l2_provider.get_chain_id().await?)
            .build(&tester.l2_wallet)
            .await?;
        tester
            .l2_provider
            .send_tx_envelope(tx_envelope.clone())
            .await?
            .expect_register()
            .await?;
        Ok(Transaction::from_transaction(
            Recovered::new_unchecked(tx_envelope, tester.l2_wallet.default_signer().address()),
            TransactionInfo::default(),
        ))
    }
}

struct NewLogsSuite {
    event_emitter: EventEmitterInstance<EthDynProvider>,
}

impl PubsubSuite for NewLogsSuite {
    type Expected = Log;
    async fn init(tester: &Tester) -> anyhow::Result<Self> {
        let event_emitter = EventEmitter::deploy(tester.l2_provider.clone()).await?;
        Ok(NewLogsSuite { event_emitter })
    }
    async fn subscribe(&self, tester: &Tester) -> anyhow::Result<Subscription<Self::Expected>> {
        let filter = Filter::new()
            .address(*self.event_emitter.address())
            .event_signature(TestEvent::SIGNATURE_HASH);
        Ok(tester.l2_provider.subscribe_logs(&filter).await?)
    }
    async fn prepare_expected(&self, tester: &Tester) -> anyhow::Result<Self::Expected> {
        let event_number = U256::from(42);
        let receipt = self
            .event_emitter
            .emitEvent(event_number)
            .send()
            .await?
            .expect_successful_receipt()
            .await?;
        let block = tester
            .l2_provider
            .get_block_by_number(receipt.block_number.unwrap().into())
            .await?
            .expect("no block found");

        Ok(Log {
            inner: alloy::primitives::Log {
                address: *self.event_emitter.address(),
                data: TestEvent {
                    number: event_number,
                }
                .into_log_data(),
            },
            block_hash: receipt.block_hash,
            block_number: Some(block.header.number),
            block_timestamp: Some(block.header.timestamp),
            transaction_hash: Some(receipt.transaction_hash),
            transaction_index: receipt.transaction_index,
            log_index: Some(0),
            removed: false,
        })
    }
}

#[test_casing([CURRENT_TO_L1, NEXT_TO_L1, NEXT_TO_GATEWAY])]
#[test_log::test(tokio::test)]
async fn new_block_pubsub(tester: Tester) -> anyhow::Result<()> {
    run_test::<NewBlockSuite>(tester).await
}

#[test_casing([CURRENT_TO_L1, NEXT_TO_L1, NEXT_TO_GATEWAY])]
#[test_log::test(tokio::test)]
async fn pending_tx_hash_pubsub(tester: Tester) -> anyhow::Result<()> {
    run_test::<PendingTxSuite<false>>(tester).await
}

#[test_casing([CURRENT_TO_L1, NEXT_TO_L1, NEXT_TO_GATEWAY])]
#[test_log::test(tokio::test)]
async fn pending_tx_full_pubsub(tester: Tester) -> anyhow::Result<()> {
    run_test::<PendingTxSuite<true>>(tester).await
}

#[test_casing([CURRENT_TO_L1, NEXT_TO_L1, NEXT_TO_GATEWAY])]
#[test_log::test(tokio::test)]
async fn new_log_pubsub(tester: Tester) -> anyhow::Result<()> {
    run_test::<NewLogsSuite>(tester).await
}
