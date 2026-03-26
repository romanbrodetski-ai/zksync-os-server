use alloy::network::{EthereumWallet, TransactionBuilder};
use alloy::primitives::{Address, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::LocalSigner;
use anyhow::Context;
use backon::{ConstantBuilder, Retryable};
use std::str::FromStr;
use std::time::Duration;
use zksync_os_integration_tests::assert_traits::ReceiptAssert;
use zksync_os_integration_tests::{CURRENT_TO_L1, Tester, test_multisetup};
use zksync_os_server::config::RebuildBlocksConfig;

#[test_multisetup([CURRENT_TO_L1])]
#[test_runtime(flavor = "multi_thread")]
async fn rebuild_blocks_during_restart_keeps_node_alive() -> anyhow::Result<()> {
    let tester = Tester::builder()
        .block_time(Duration::from_millis(50))
        .build()
        .await?;

    let second_wallet = EthereumWallet::new(LocalSigner::from_str(
        "0xac1e09fe4f8c7b2e9e13ab632d2f6a77b8cf57fb9f3f35e6c5c7d8f1b2a3c4d5",
    )?);
    let second_signer = ProviderBuilder::new()
        .wallet(second_wallet.clone())
        .connect(tester.l2_rpc_url())
        .await
        .context("failed to connect second signer to L2")?;
    let second_address = second_wallet.default_signer().address();

    tester
        .l2_provider
        .send_transaction(
            TransactionRequest::default()
                .with_to(second_address)
                .with_value(U256::from(1_000_000_000_000_000u64)),
        )
        .await?
        .expect_successful_receipt()
        .await?;

    let mut primary_last_block = 1;
    for _ in 0..23 {
        let receipt = tester
            .l2_provider
            .send_transaction(
                TransactionRequest::default()
                    .with_to(Address::random())
                    .with_value(U256::from(1u64)),
            )
            .await?
            .expect_successful_receipt()
            .await?;
        primary_last_block = receipt
            .block_number
            .expect("transfer receipt should have a block number");
    }
    let second_sender_receipt = second_signer
        .send_transaction(
            TransactionRequest::default()
                .with_to(Address::random())
                .with_value(U256::from(1u64)),
        )
        .await?
        .expect_successful_receipt()
        .await?;
    let final_rebuilt_block = second_sender_receipt
        .block_number
        .expect("second sender receipt should have a block number");
    let block_to_empty = primary_last_block - 4;
    let final_rebuilt_tx_hash = second_sender_receipt.transaction_hash;

    let original_emptied_block_hash = tester
        .l2_provider
        .get_block_by_number(block_to_empty.into())
        .await?
        .context("original block should exist")?
        .header
        .hash_slow();
    let original_emptied_block_tx_count = tester
        .l2_provider
        .get_block_transaction_count_by_number(block_to_empty.into())
        .await?
        .expect("original block should exist");
    assert!(
        original_emptied_block_tx_count > 0u64.into(),
        "block to empty should contain at least one transaction"
    );

    let original_final_block_hash = tester
        .l2_provider
        .get_block_by_number(final_rebuilt_block.into())
        .await?
        .context("final block should exist")?
        .header
        .hash_slow();
    let original_final_block_tx_count = tester
        .l2_provider
        .get_block_transaction_count_by_number(final_rebuilt_block.into())
        .await?
        .expect("final block should exist");
    assert!(
        original_final_block_tx_count > 0u64.into(),
        "final block should contain at least one transaction"
    );
    let original_final_tx = tester
        .l2_provider
        .get_transaction_by_hash(final_rebuilt_tx_hash)
        .await?
        .context("final rebuilt transaction should exist")?;
    assert_eq!(
        original_final_tx.block_number,
        Some(final_rebuilt_block),
        "final rebuilt transaction should initially belong to the last block"
    );

    let restarted = tester
        .restart_with_overrides(|config| {
            config.sequencer_config.block_time = Duration::from_millis(5);
            config.sequencer_config.block_rebuild = Some(RebuildBlocksConfig {
                from_block: block_to_empty,
                blocks_to_empty: vec![block_to_empty],
            });
        })
        .await?;

    (|| async {
        let restarted_provider = ProviderBuilder::new()
            .connect(restarted.l2_rpc_url())
            .await
            .with_context(|| format!("failed to connect to restarted node at {}", restarted.l2_rpc_url()))?;
        let rebuilt_emptied_block = restarted_provider
            .get_block_by_number(block_to_empty.into())
            .await?
            .context("rebuilt emptied block should exist")?;
        let rebuilt_emptied_block_tx_count = restarted_provider
            .get_block_transaction_count_by_number(block_to_empty.into())
            .await?
            .context("rebuilt emptied block tx count should exist")?;
        let rebuilt_final_block = restarted_provider
            .get_block_by_number(final_rebuilt_block.into())
            .await?
            .context("rebuilt final block should exist")?;
        let rebuilt_final_block_tx_count = restarted_provider
            .get_block_transaction_count_by_number(final_rebuilt_block.into())
            .await?
            .context("rebuilt final block tx count should exist")?;
        let rebuilt_final_tx = restarted_provider
            .get_transaction_by_hash(final_rebuilt_tx_hash)
            .await?
            .context("rebuilt final transaction should exist")?;

        let rebuilt_emptied_block_hash = rebuilt_emptied_block.header.hash_slow();
        let rebuilt_final_block_hash = rebuilt_final_block.header.hash_slow();
        if rebuilt_emptied_block_hash != original_emptied_block_hash
            && rebuilt_emptied_block_tx_count == 0
            && rebuilt_final_block_hash != original_final_block_hash
            && rebuilt_final_block_tx_count == original_final_block_tx_count
            && rebuilt_final_tx.block_number == Some(final_rebuilt_block)
        {
            Ok(())
        } else {
            anyhow::bail!(
                "rebuild not finished yet: emptied_block={} hash={} original_hash={} tx_count={} \
                 final_block={} hash={} original_hash={} tx_count={} original_tx_count={} \
                 final_tx_block={:?}",
                block_to_empty,
                rebuilt_emptied_block_hash,
                original_emptied_block_hash,
                rebuilt_emptied_block_tx_count,
                final_rebuilt_block,
                rebuilt_final_block_hash,
                original_final_block_hash,
                rebuilt_final_block_tx_count,
                original_final_block_tx_count,
                rebuilt_final_tx.block_number
            );
        }
    })
    .retry(
        ConstantBuilder::default()
            .with_delay(Duration::from_millis(200))
            .with_max_times(100),
    )
    .await?;
    Ok(())
}
