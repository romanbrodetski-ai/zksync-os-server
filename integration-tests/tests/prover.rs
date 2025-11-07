#![cfg(feature = "prover-tests")]

use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use zksync_os_integration_tests::Tester;

#[test_log::test(tokio::test)]
async fn prover() -> anyhow::Result<()> {
    // Test that prover can successfully prove at least one batch
    let tester = Tester::builder().enable_prover().build().await?;
    run_txs(&tester).await?;

    // Test environment comes with some L1 transactions by default, so one batch should be provable
    // without any new transactions inside the test.
    tester.prover_tester.wait_for_batch_proven(1).await?;

    // todo: consider expanding this test to prove multiple batches on top of the first batch
    //       also to test L2 transactions are provable too

    Ok(())
}

async fn run_txs(tester: &Tester) -> anyhow::Result<()> {
    for nonce in 0..60000 {
        let tx = TransactionRequest::default()
            .with_to(Address::random())
            .with_value(U256::from(1))
            .nonce(nonce)
            .gas_limit(21_000);

        let _ = tester.l2_provider.send_transaction(tx).await?;
    }

    Ok(())
}
