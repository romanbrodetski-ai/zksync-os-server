use alloy::eips::Encodable2718;
use alloy::network::{ReceiptResponse, TransactionBuilder, TxSigner};
use alloy::primitives::{TxHash, U128, U256, address};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use regex::Regex;
use std::time::Duration;
use zksync_os_integration_tests::assert_traits::ReceiptAssert;
use zksync_os_integration_tests::contracts::EventEmitter;
use zksync_os_integration_tests::{
    CURRENT_TO_L1, NEXT_TO_GATEWAY, NEXT_TO_L1, Tester, test_casing,
};
use zksync_os_server::config::FeeConfig;

#[test_casing([CURRENT_TO_L1, NEXT_TO_L1, NEXT_TO_GATEWAY])]
#[test_log::test(tokio::test)]
async fn get_code(tester: Tester) -> anyhow::Result<()> {
    let deploy_tx_receipt = EventEmitter::deploy_builder(tester.l2_provider.clone())
        .send()
        .await?
        .expect_successful_receipt()
        .await?;
    let contract_address = deploy_tx_receipt
        .contract_address()
        .expect("no contract deployed");

    let latest_code = tester.l2_provider.get_code_at(contract_address).await?;
    assert_eq!(latest_code, EventEmitter::DEPLOYED_BYTECODE);
    let at_block_code = tester
        .l2_provider
        .get_code_at(contract_address)
        .block_id(
            deploy_tx_receipt
                .block_hash
                .expect("deploy receipt has no block hash")
                .into(),
        )
        .await?;
    assert_eq!(at_block_code, EventEmitter::DEPLOYED_BYTECODE);
    let before_block_code = tester
        .l2_provider
        .get_code_at(contract_address)
        .block_id(
            (deploy_tx_receipt
                .block_number
                .expect("deploy receipt has no block number")
                - 1)
            .into(),
        )
        .await?;
    assert!(before_block_code.is_empty());

    Ok(())
}

#[test_casing([CURRENT_TO_L1, NEXT_TO_L1, NEXT_TO_GATEWAY])]
#[test_builder(|builder| builder.block_time(Duration::from_secs(5)))]
#[test_log::test(tokio::test)]
async fn get_transaction_count(tester: Tester) -> anyhow::Result<()> {
    let alice = tester.l2_wallet.default_signer().address();
    let l2_provider = &tester.l2_provider;

    assert_eq!(l2_provider.get_transaction_count(alice).await?, 0);

    let deploy_pending_tx = EventEmitter::deploy_builder(l2_provider.clone())
        .send()
        .await?;
    assert_eq!(l2_provider.get_transaction_count(alice).pending().await?, 1);
    assert_eq!(l2_provider.get_transaction_count(alice).latest().await?, 0);
    assert_eq!(l2_provider.get_transaction_count(alice).await?, 0);

    deploy_pending_tx.expect_successful_receipt().await?;
    assert_eq!(l2_provider.get_transaction_count(alice).pending().await?, 1);
    assert_eq!(l2_provider.get_transaction_count(alice).latest().await?, 1);

    Ok(())
}

#[test_casing([CURRENT_TO_L1, NEXT_TO_L1, NEXT_TO_GATEWAY])]
#[test_log::test(tokio::test)]
async fn get_net_version(tester: Tester) -> anyhow::Result<()> {
    let net_version = tester.l2_provider.get_net_version().await?;
    let chain_id = tester.l2_provider.get_chain_id().await?;
    assert_eq!(net_version, chain_id);
    Ok(())
}

#[test_casing([CURRENT_TO_L1, NEXT_TO_L1, NEXT_TO_GATEWAY])]
#[test_log::test(tokio::test)]
async fn get_client_version(tester: Tester) -> anyhow::Result<()> {
    let client_version = tester.l2_provider.get_client_version().await?;
    let regex = Regex::new(r"^zksync-os/v(\d+)\.(\d+)\.(\d+)")?;
    assert!(regex.is_match(&client_version));
    Ok(())
}

#[test_casing([CURRENT_TO_L1, NEXT_TO_L1, NEXT_TO_GATEWAY])]
#[test_builder(|builder| {
    let known_base_fee: u128 = 100_000_000;
    let fee_config = FeeConfig {
        native_price_usd: 3e-9,
        base_fee_override: Some(U128::from(known_base_fee)),
        native_per_gas: 100,
        pubdata_price_override: Some(U128::from(1_000_000u64)),
        native_price_override: Some(U128::from(1_000_000u64)),
        pubdata_price_cap: None,
    };
    builder.fee_config(fee_config).gas_price_scale_factor(2.0)
})]
#[test_log::test(tokio::test)]
async fn get_gas_price_uses_configured_scale_factor(tester: Tester) -> anyhow::Result<()> {
    let gas_price = tester.l2_provider.get_gas_price().await?;
    assert_eq!(gas_price, 200_000_000);

    Ok(())
}

#[test_casing([CURRENT_TO_L1, NEXT_TO_L1, NEXT_TO_GATEWAY])]
#[test_log::test(tokio::test)]
async fn send_raw_transaction_sync(tester: Tester) -> anyhow::Result<()> {
    let alice = tester.l2_wallet.default_signer().address();
    let fees = tester.l2_provider.estimate_eip1559_fees().await?;
    let tx = TransactionRequest::default()
        .to(alice)
        .value(U256::from(1))
        .nonce(0)
        .gas_price(fees.max_fee_per_gas)
        .gas_limit(50_000);
    let tx_envelope = tx.build(&tester.l2_wallet).await?;
    let encoded = tx_envelope.encoded_2718();

    let receipt = tester
        .l2_provider
        .send_raw_transaction_sync(&encoded)
        .await?;
    assert!(receipt.status());
    assert_eq!(receipt.to(), Some(alice));
    assert!(receipt.block_number().is_some());
    assert_ne!(receipt.transaction_hash(), TxHash::ZERO);

    Ok(())
}

#[test_casing([CURRENT_TO_L1, NEXT_TO_L1, NEXT_TO_GATEWAY])]
#[test_log::test(tokio::test)]
async fn send_raw_transaction_sync_timeout(tester: Tester) -> anyhow::Result<()> {
    let alice = tester.l2_wallet.default_signer().address();
    let fees = tester.l2_provider.estimate_eip1559_fees().await?;
    let tx = TransactionRequest::default()
        .to(alice)
        .value(U256::from(1))
        .nonce(1)
        .gas_price(fees.max_fee_per_gas)
        .gas_limit(50_000);
    let tx_envelope = tx.build(&tester.l2_wallet).await?;
    let encoded = tx_envelope.encoded_2718();

    let error = tester
        .l2_provider
        .send_raw_transaction_sync(&encoded)
        .await
        .expect_err("should fail");
    assert!(
        error
            .to_string()
            .contains("The transaction was added to the mempool but wasn't processed within")
    );

    Ok(())
}

#[test_casing([CURRENT_TO_L1, NEXT_TO_L1, NEXT_TO_GATEWAY])]
#[test_builder(|builder| {
    let fee_config = FeeConfig {
        native_price_usd: 3e-9,
        pubdata_price_override: Some(U128::from(10_000_000_000_000u64)),
        native_price_override: Some(U128::from(1_000_000u64)),
        base_fee_override: Some(U128::from(100_000_000u64)),
        native_per_gas: 100,
        pubdata_price_cap: None,
    };
    builder
        .fee_config(fee_config)
        .estimate_gas_pubdata_price_factor(1.0)
})]
#[test_log::test(tokio::test)]
async fn estimate_gas_with_high_prices(tester: Tester) -> anyhow::Result<()> {
    let to = address!("0xa5d85D1D865F89a23A95d4F5F74850f289Dbc5f9");
    let tx = TransactionRequest::default().to(to).value(U256::ONE);

    let _gas = tester.l2_provider.estimate_gas(tx.clone()).await?;
    tester
        .l2_provider
        .send_transaction(tx)
        .await?
        .expect_successful_receipt()
        .await?;

    Ok(())
}

#[test_casing([CURRENT_TO_L1, NEXT_TO_L1, NEXT_TO_GATEWAY])]
#[test_log::test(tokio::test)]
async fn estimate_gas_without_balance(tester: Tester) -> anyhow::Result<()> {
    let req = TransactionRequest::default()
        .to(address!("0xF8fF3e62E94807a5C687f418Fe36942dD3a24525"))
        .from(address!("0x38711eC715A5A32180427792Dc0e97f8E3303072"));
    let txs_requests = [
        req.clone(),
        req.clone().gas_price(0),
        req.clone().max_priority_fee_per_gas(0),
        req.clone().max_fee_per_gas(0).max_priority_fee_per_gas(0),
    ];
    for tx_request in txs_requests {
        let _estimated_gas = tester.l2_provider.estimate_gas(tx_request).await?;
    }
    Ok(())
}
