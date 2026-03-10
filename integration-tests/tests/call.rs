use alloy::consensus::BlobTransactionSidecar;
use alloy::eips::BlockId;
use alloy::primitives::{U256, b256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use alloy::rpc::types::state::{AccountOverride, StateOverride};
use std::collections::HashMap;
use zksync_os_integration_tests::assert_traits::EthCallAssert;
use zksync_os_integration_tests::contracts::{EventEmitter, SimpleRevert, TracingSecondary};
use zksync_os_integration_tests::{
    CURRENT_TO_L1, NEXT_TO_GATEWAY, NEXT_TO_L1, Tester, test_casing,
};

#[test_casing([CURRENT_TO_L1, NEXT_TO_L1, NEXT_TO_GATEWAY])]
#[test_log::test(tokio::test)]
async fn call_genesis(tester: Tester) -> anyhow::Result<()> {
    tester
        .l2_provider
        .call(TransactionRequest::default())
        .block(0.into())
        .await?;
    Ok(())
}

#[test_casing([CURRENT_TO_L1, NEXT_TO_L1, NEXT_TO_GATEWAY])]
#[test_log::test(tokio::test)]
async fn call_pending(tester: Tester) -> anyhow::Result<()> {
    tester
        .l2_provider
        .call(TransactionRequest::default())
        .block(BlockId::pending())
        .await?;
    Ok(())
}

#[test_casing([CURRENT_TO_L1, NEXT_TO_L1, NEXT_TO_GATEWAY])]
#[test_log::test(tokio::test)]
async fn call_fail(tester: Tester) -> anyhow::Result<()> {
    tester
        .l2_provider
        .call(TransactionRequest {
            sidecar: Some(BlobTransactionSidecar {
                blobs: vec![],
                commitments: vec![],
                proofs: vec![],
            }),
            ..Default::default()
        })
        .expect_to_fail("EIP-4844 transactions are not supported")
        .await;
    tester
        .l2_provider
        .call(TransactionRequest {
            authorization_list: Some(vec![]),
            ..Default::default()
        })
        .expect_to_fail("EIP-7702 transactions are not supported")
        .await;
    tester
        .l2_provider
        .call(TransactionRequest::default())
        .block((u32::MAX as u64).into())
        .expect_to_fail("block `0xffffffff` not found")
        .await;
    tester
        .l2_provider
        .call(TransactionRequest {
            gas_price: Some(100),
            max_fee_per_gas: Some(100),
            ..Default::default()
        })
        .expect_to_fail("both `gasPrice` and (`maxFeePerGas` or `maxPriorityFeePerGas`) specified")
        .await;
    tester
        .l2_provider
        .call(TransactionRequest {
            max_fee_per_gas: Some(1),
            max_priority_fee_per_gas: Some(1),
            ..Default::default()
        })
        .expect_to_fail("`maxFeePerGas` less than `block.baseFee`")
        .await;
    tester
        .l2_provider
        .call(TransactionRequest {
            max_fee_per_gas: Some(1_000_000_001),
            max_priority_fee_per_gas: Some(1_000_000_002),
            ..Default::default()
        })
        .expect_to_fail("`maxPriorityFeePerGas` higher than `maxFeePerGas`")
        .await;
    tester
        .l2_provider
        .call(TransactionRequest {
            max_fee_per_gas: Some(1),
            max_priority_fee_per_gas: Some(1),
            ..Default::default()
        })
        .expect_to_fail("`maxFeePerGas` less than `block.baseFee`")
        .await;
    tester
        .l2_provider
        .call(TransactionRequest {
            max_priority_fee_per_gas: Some(u128::MAX),
            ..Default::default()
        })
        .expect_to_fail("`maxPriorityFeePerGas` is too high")
        .await;
    tester
        .l2_provider
        .call(TransactionRequest {
            max_fee_per_gas: Some(1_000_000_001),
            ..Default::default()
        })
        .expect_to_fail("missing `maxPriorityFeePerGas` field for EIP-1559 transaction")
        .await;

    Ok(())
}

#[test_casing([CURRENT_TO_L1, NEXT_TO_L1, NEXT_TO_GATEWAY])]
#[test_log::test(tokio::test)]
async fn call_deploy(tester: Tester) -> anyhow::Result<()> {
    let result = EventEmitter::deploy_builder(tester.l2_provider.clone())
        .call()
        .await?;
    assert_eq!(result, EventEmitter::DEPLOYED_BYTECODE);
    Ok(())
}

#[test_casing([CURRENT_TO_L1, NEXT_TO_L1, NEXT_TO_GATEWAY])]
#[test_log::test(tokio::test)]
async fn call_revert(tester: Tester) -> anyhow::Result<()> {
    let simple_revert = SimpleRevert::deploy(tester.l2_provider.clone()).await?;
    let error = simple_revert
        .simpleRevert()
        .call_raw()
        .await
        .expect_err("call did not result in revert error")
        .to_string();
    assert_eq!(
        error,
        "server returned an error response: error code 3: execution reverted, data: \"0xc2bb947c\""
    );
    let error = simple_revert
        .stringRevert()
        .call_raw()
        .await
        .expect_err("call did not result in revert error")
        .to_string();
    assert_eq!(
        error,
        "server returned an error response: error code 3: execution reverted: my message, data: \"0x08c379a00000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000000000000000000a6d79206d65737361676500000000000000000000000000000000000000000000\""
    );

    Ok(())
}

#[test_casing([CURRENT_TO_L1, NEXT_TO_L1, NEXT_TO_GATEWAY])]
#[test_log::test(tokio::test)]
async fn call_with_state_overrides(tester: Tester) -> anyhow::Result<()> {
    let initial_data = U256::from(1);
    let contract = TracingSecondary::deploy(tester.l2_provider.clone(), initial_data).await?;
    let tx_req = contract.multiply(U256::from(1)).into_transaction_request();

    let out = tester.l2_provider.call(tx_req.clone()).await?;
    let baseline = U256::from_be_slice(&out);
    assert_eq!(baseline, initial_data);

    let overrides = StateOverride::from_iter([(
        *contract.address(),
        AccountOverride {
            balance: None,
            nonce: None,
            code: None,
            state: Some(HashMap::from_iter([(
                b256!("0x0000000000000000000000000000000000000000000000000000000000000000"),
                b256!("0x0000000000000000000000000000000000000000000000000000000000000002"),
            )])),
            state_diff: None,
            move_precompile_to: None,
        },
    )]);

    let out_overridden = tester.l2_provider.call(tx_req).overrides(overrides).await?;
    let overridden = U256::from_be_slice(&out_overridden);
    assert_eq!(overridden, U256::from(2));

    Ok(())
}
