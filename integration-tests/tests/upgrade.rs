use alloy::{
    eips::eip1559::Eip1559Estimation,
    primitives::{Address, Bytes, U256},
    providers::utils::Eip1559Estimator,
    providers::{PendingTransactionBuilder, Provider},
    rpc::types::TransactionRequest,
};
use std::collections::BTreeMap;
use zksync_os_contract_interface::Bridgehub;
use zksync_os_contract_interface::IMailbox::NewPriorityRequest;
use zksync_os_integration_tests::Tester;
use zksync_os_integration_tests::contracts::SampleForceDeployment;
use zksync_os_integration_tests::upgrade::UpgradeTester;
use zksync_os_integration_tests::assert_traits::ReceiptAssert;
use zksync_os_integration_tests::provider::ZksyncApi;
use zksync_os_server::default_protocol_version::NEXT_PROTOCOL_VERSION;
use zksync_os_types::{L1PriorityTxType, L1TxType, REQUIRED_L1_TO_L2_GAS_PER_PUBDATA_BYTE};

async fn fund_wallet_via_l1_deposit(
    tester: &zksync_os_integration_tests::Tester,
    wallet: Address,
    amount: U256,
) -> anyhow::Result<()> {
    let chain_id = tester.l2_provider.get_chain_id().await?;
    let bridgehub = Bridgehub::new(
        tester.l2_zk_provider.get_bridgehub_contract().await?,
        tester.l1_provider.clone(),
        chain_id,
    );
    let max_priority_fee_per_gas = tester.l1_provider.get_max_priority_fee_per_gas().await?;
    let base_l1_fees_data = tester
        .l1_provider
        .estimate_eip1559_fees_with(Eip1559Estimator::new(|base_fee_per_gas, _| {
            Eip1559Estimation {
                max_fee_per_gas: base_fee_per_gas * 3 / 2,
                max_priority_fee_per_gas: 0,
            }
        }))
        .await?;
    let max_fee_per_gas = base_l1_fees_data.max_fee_per_gas + max_priority_fee_per_gas;
    let gas_limit = tester
        .l2_provider
        .estimate_gas(
            TransactionRequest::default()
                .transaction_type(L1PriorityTxType::TX_TYPE)
                .from(wallet)
                .to(wallet)
                .value(amount),
        )
        .await?;
    let tx_base_cost = bridgehub
        .l2_transaction_base_cost(
            max_fee_per_gas + max_priority_fee_per_gas,
            gas_limit,
            REQUIRED_L1_TO_L2_GAS_PER_PUBDATA_BYTE,
        )
        .await?;
    let l1_deposit_request = bridgehub
        .request_l2_transaction_direct(
            amount + tx_base_cost,
            wallet,
            amount,
            vec![],
            gas_limit,
            REQUIRED_L1_TO_L2_GAS_PER_PUBDATA_BYTE,
            wallet,
        )
        .value(amount + tx_base_cost)
        .max_fee_per_gas(max_fee_per_gas)
        .max_priority_fee_per_gas(max_priority_fee_per_gas)
        .into_transaction_request();
    let l1_deposit_receipt = tester
        .l1_provider
        .send_transaction(l1_deposit_request)
        .await?
        .expect_successful_receipt()
        .await?;
    let l1_to_l2_tx_log = l1_deposit_receipt
        .logs()
        .iter()
        .filter_map(|log| log.log_decode::<NewPriorityRequest>().ok())
        .next()
        .expect("no L1->L2 logs produced by deposit tx");
    let l2_tx_hash = l1_to_l2_tx_log.inner.txHash;
    PendingTransactionBuilder::new(tester.l2_zk_provider.root().clone(), l2_tx_hash)
        .expect_successful_receipt()
        .await?;
    Ok(())
}

/// Executes the simplest patch protocol upgrade:
/// - no contracts are deployed
/// - patch version is bumped by 1
/// - upgrade timestamp is 0
/// Importance of this test: unlike minor version upgrades, patch upgrades
/// do not include an upgrade transaction in the block. Hence, we need to ensure that
/// the system can handle patch upgrades correctly.
#[test_log::test(tokio::test)]
async fn upgrade_patch_no_deployments() -> anyhow::Result<()> {
    let upgrade_timestamp = U256::from(0); // Protocol upgrade can be executed immediately.
    let deadline = U256::MAX; // The protocol version will not have any deadline in this upgrade

    // Test that we can deposit L2 funds from a rich L1 account
    let tester = Tester::builder()
        .protocol_version(NEXT_PROTOCOL_VERSION)
        .build()
        .await?;
    let upgrade_tester = UpgradeTester::for_default_upgrade(tester).await?;
    let l2_wallet = upgrade_tester.tester.l2_wallet.default_signer().address();
    let deposit_amount = U256::from(10) * U256::from(10).pow(U256::from(18));
    fund_wallet_via_l1_deposit(&upgrade_tester.tester, l2_wallet, deposit_amount).await?;

    // Prepare protocol upgrade
    let protocol_upgrade = upgrade_tester
        .protocol_upgrade_builder()
        .await?
        .bump_patch(1)
        .with_force_deployments(BTreeMap::new())
        .with_timestamp(upgrade_timestamp)
        .build();

    upgrade_tester
        .execute_default_upgrade(&protocol_upgrade, deadline, upgrade_timestamp, true)
        .await?;

    Ok(())
}

/// Performs a minor protocol upgrade which also does a force deployment.
#[test_log::test(tokio::test)]
async fn upgrade_minor_with_deployments() -> anyhow::Result<()> {
    let upgrade_timestamp = U256::from(0); // Protocol upgrade can be executed immediately.
    let deadline = U256::MAX; // The protocol version will not have any deadline in this upgrade

    let sample_force_deployment_address: Address = "0x000000000000000000000000000000000000dead"
        .parse()
        .unwrap();

    let force_deployments: BTreeMap<Address, Bytes> = [(
        sample_force_deployment_address,
        SampleForceDeployment::DEPLOYED_BYTECODE.clone(),
    )]
    .into_iter()
    .collect();

    // Test that we can deposit L2 funds from a rich L1 account
    let tester = Tester::builder()
        .protocol_version(NEXT_PROTOCOL_VERSION)
        .build()
        .await?;
    let upgrade_tester = UpgradeTester::for_default_upgrade(tester).await?;
    let l2_wallet = upgrade_tester.tester.l2_wallet.default_signer().address();
    let deposit_amount = U256::from(10) * U256::from(10).pow(U256::from(18));
    fund_wallet_via_l1_deposit(&upgrade_tester.tester, l2_wallet, deposit_amount).await?;

    // Publish the bytecodes for upgrade beforehand.
    // TODO: we need to use bytecode instead of deployed bytecode for now, since under the hood `publish_bytecodes`
    // actually deploys contracts since BytecodesSupplier is not ready for zksync os
    // Once this is fixed, also check the logic for `ForceDeploymentBytecodeInfo` in the builder.
    upgrade_tester
        .publish_bytecodes([SampleForceDeployment::BYTECODE.clone()])
        .await?;

    // Prepare protocol upgrade
    let protocol_upgrade = upgrade_tester
        .protocol_upgrade_builder()
        .await?
        .bump_minor(1)
        .with_force_deployments(force_deployments)
        .with_timestamp(upgrade_timestamp)
        .build();

    upgrade_tester
        .execute_default_upgrade(&protocol_upgrade, deadline, upgrade_timestamp, false)
        .await?;

    // Ensure that the contract is now callable.
    let force_deployed_contract = SampleForceDeployment::new(
        sample_force_deployment_address,
        upgrade_tester.tester.l2_provider.clone(),
    );
    let stored_value = force_deployed_contract.return42().call().await?;
    assert_eq!(stored_value, U256::from(42));

    let main_node_block = upgrade_tester.tester.l2_provider.get_block_number().await?;

    // Ensure that EN can sync from the upgraded node.
    let en1 = upgrade_tester.tester.launch_external_node().await?;

    while en1.l2_provider.get_block_number().await? < main_node_block {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }

    Ok(())
}
