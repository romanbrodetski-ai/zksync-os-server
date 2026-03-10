use alloy::eips::eip1559::Eip1559Estimation;
use alloy::network::TxSigner;
use alloy::primitives::{Address, B256, U256, address};
use alloy::providers::utils::Eip1559Estimator;
use alloy::providers::{PendingTransactionBuilder, Provider};
use alloy::rpc::types::TransactionReceipt;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol_types::SolValue;
use zksync_os_contract_interface::Bridgehub;
use zksync_os_contract_interface::IMailbox::NewPriorityRequest;
use zksync_os_integration_tests::assert_traits::ReceiptAssert;
use zksync_os_integration_tests::contracts::TestERC20::TestERC20Instance;
use zksync_os_integration_tests::contracts::{IL2AssetRouter, L1AssetRouter, TestERC20};
use zksync_os_integration_tests::dyn_wallet_provider::EthDynProvider;
use zksync_os_integration_tests::provider::ZksyncApi;
use zksync_os_integration_tests::{
    CURRENT_TO_L1, NEXT_TO_GATEWAY, NEXT_TO_L1, Tester, test_casing,
};
use zksync_os_types::{L2ToL1Log, REQUIRED_L1_TO_L2_GAS_PER_PUBDATA_BYTE, ZkTxType};

#[test_casing([CURRENT_TO_L1, NEXT_TO_L1, NEXT_TO_GATEWAY])]
#[test_log::test(tokio::test)]
async fn erc20_deposit(tester: Tester) -> anyhow::Result<()> {
    let alice = tester.l1_wallet().default_signer().address();
    let mint_amount = U256::from(100u64);
    let deposit_amount = U256::from(40u64);
    let l1_erc20 = deploy_l1_token_and_mint(&tester, mint_amount).await?;

    assert_eq!(l1_erc20.balanceOf(alice).call().await?, mint_amount);

    let l1_deposit_receipt = deposit_erc20(&tester, &l1_erc20, alice, deposit_amount).await?;
    assert_successful_deposit_l2_part(&tester, l1_deposit_receipt).await?;

    let l2_erc20_address = l2_token_address(&tester, *l1_erc20.address()).await?;
    let l2_erc20 = TestERC20::new(l2_erc20_address, tester.l2_provider.clone());
    let l2_balance = l2_erc20.balanceOf(alice).call().await?;
    assert_eq!(l2_balance, deposit_amount);

    let l1_balance = l1_erc20.balanceOf(alice).call().await?;
    assert_eq!(l1_balance, mint_amount - deposit_amount);

    Ok(())
}

#[test_casing([CURRENT_TO_L1, NEXT_TO_L1, NEXT_TO_GATEWAY])]
#[test_log::test(tokio::test)]
async fn erc20_transfer(tester: Tester) -> anyhow::Result<()> {
    let alice = tester.l2_wallet.default_signer().address();
    let bob_signer = PrivateKeySigner::random();
    let bob = bob_signer.address();

    let mint_amount = U256::from(100u64);
    let l1_erc20 = deploy_l1_token_and_mint(&tester, mint_amount).await?;
    let l1_deposit_receipt = deposit_erc20(&tester, &l1_erc20, alice, mint_amount).await?;
    assert_successful_deposit_l2_part(&tester, l1_deposit_receipt).await?;

    let l2_erc20_address = l2_token_address(&tester, *l1_erc20.address()).await?;
    let l2_erc20 = TestERC20::new(l2_erc20_address, tester.l2_provider.clone());

    let transfer_amount = U256::from(40u64);
    l2_erc20
        .transfer(bob, transfer_amount)
        .from(alice)
        .send()
        .await?
        .expect_successful_receipt()
        .await?;

    let alice_l2_balance = l2_erc20.balanceOf(alice).call().await?;
    let bob_l2_balance = l2_erc20.balanceOf(bob).call().await?;

    assert_eq!(alice_l2_balance, mint_amount - transfer_amount);
    assert_eq!(bob_l2_balance, transfer_amount);

    Ok(())
}

#[test_casing([CURRENT_TO_L1, NEXT_TO_L1, NEXT_TO_GATEWAY])]
#[test_log::test(tokio::test)]
async fn erc20_withdrawal(tester: Tester) -> anyhow::Result<()> {
    let alice = tester.l2_wallet.default_signer().address();

    let mint_amount = U256::from(100u64);
    let l1_erc20 = deploy_l1_token_and_mint(&tester, mint_amount).await?;
    let l1_deposit_receipt = deposit_erc20(&tester, &l1_erc20, alice, mint_amount).await?;
    assert_successful_deposit_l2_part(&tester, l1_deposit_receipt).await?;

    let l2_erc20_address = l2_token_address(&tester, *l1_erc20.address()).await?;
    let l2_erc20 = TestERC20::new(l2_erc20_address, tester.l2_provider.clone());
    let l2_asset_router_address = address!("0x0000000000000000000000000000000000010003");
    let l2_asset_router =
        IL2AssetRouter::new(l2_asset_router_address, tester.l2_zk_provider.clone());

    let withdraw_amount = U256::from(40u64);
    let l2_receipt = l2_asset_router
        .withdraw(alice, l2_erc20_address, withdraw_amount)
        .send()
        .await?
        .expect_to_execute()
        .await?;
    let l1_asset_router =
        L1AssetRouter::new(tester.l1_provider().clone(), tester.l2_zk_provider.clone()).await?;
    let l1_nullifier = l1_asset_router.l1_nullifier().await?;
    l1_nullifier.finalize_withdrawal(l2_receipt).await?;

    let l1_balance = l1_erc20.balanceOf(alice).call().await?;
    let l2_balance = l2_erc20.balanceOf(alice).call().await?;

    assert_eq!(l1_balance, withdraw_amount);
    assert_eq!(l2_balance, mint_amount - withdraw_amount);

    Ok(())
}

async fn deploy_l1_token_and_mint(
    tester: &Tester,
    mint_amount: U256,
) -> anyhow::Result<TestERC20Instance<EthDynProvider>> {
    let l1_erc20 = TestERC20::deploy(
        tester.l1_provider().clone(),
        U256::ZERO,
        "Test token".to_string(),
        "TEST".to_string(),
    )
    .await?;
    l1_erc20
        .mint(tester.l1_wallet().default_signer().address(), mint_amount)
        .send()
        .await?
        .expect_successful_receipt()
        .await?;
    Ok(l1_erc20)
}

async fn deposit_erc20(
    tester: &Tester,
    l1_erc20: &TestERC20Instance<EthDynProvider>,
    to: Address,
    amount: U256,
) -> anyhow::Result<TransactionReceipt> {
    let chain_id = tester.l2_provider.get_chain_id().await?;
    let bridgehub = Bridgehub::new(
        tester.l2_zk_provider.get_bridgehub_contract().await?,
        tester.l1_provider().clone(),
        chain_id,
    );

    let max_priority_fee_per_gas = tester.l1_provider().get_max_priority_fee_per_gas().await?;
    let base_l1_fees_data = tester
        .l1_provider()
        .estimate_eip1559_fees_with(Eip1559Estimator::new(|base_fee_per_gas, _| {
            Eip1559Estimation {
                max_fee_per_gas: base_fee_per_gas * 3 / 2,
                max_priority_fee_per_gas: 0,
            }
        }))
        .await?;
    let max_fee_per_gas = base_l1_fees_data.max_fee_per_gas + max_priority_fee_per_gas;

    let l2_gas_limit = 1_500_000;
    let tx_base_cost = bridgehub
        .l2_transaction_base_cost(
            max_fee_per_gas + max_priority_fee_per_gas,
            l2_gas_limit,
            REQUIRED_L1_TO_L2_GAS_PER_PUBDATA_BYTE,
        )
        .await?;
    let shared_bridge_address = bridgehub.shared_bridge_address().await?;
    let second_bridge_calldata = (*l1_erc20.address(), amount, to).abi_encode();

    l1_erc20
        .approve(shared_bridge_address, amount)
        .max_fee_per_gas(max_fee_per_gas)
        .max_priority_fee_per_gas(max_priority_fee_per_gas)
        .send()
        .await?
        .expect_successful_receipt()
        .await?;

    let deposit_request = bridgehub
        .request_l2_transaction_two_bridges(
            tx_base_cost,
            U256::ZERO,
            l2_gas_limit,
            REQUIRED_L1_TO_L2_GAS_PER_PUBDATA_BYTE,
            to,
            shared_bridge_address,
            U256::ZERO,
            second_bridge_calldata,
        )
        .value(tx_base_cost)
        .max_fee_per_gas(max_fee_per_gas)
        .max_priority_fee_per_gas(max_priority_fee_per_gas)
        .into_transaction_request();

    tester
        .l1_provider()
        .send_transaction(deposit_request)
        .await?
        .expect_successful_receipt()
        .await
}

async fn assert_successful_deposit_l2_part(
    tester: &Tester,
    l1_deposit_receipt: TransactionReceipt,
) -> anyhow::Result<()> {
    let l1_to_l2_tx_log = l1_deposit_receipt
        .logs()
        .iter()
        .filter_map(|log| log.log_decode::<NewPriorityRequest>().ok())
        .next()
        .expect("no L1->L2 logs produced by deposit tx");
    let l2_tx_hash = l1_to_l2_tx_log.inner.txHash;

    let receipt = PendingTransactionBuilder::new(tester.l2_zk_provider.root().clone(), l2_tx_hash)
        .expect_successful_receipt()
        .await?;
    assert_eq!(receipt.inner.tx_type(), ZkTxType::L1);

    let mut l2_to_l1_logs = receipt.inner.l2_to_l1_logs().to_vec();
    assert_eq!(l2_to_l1_logs.len(), 1);
    let l2_to_l1_log: L2ToL1Log = l2_to_l1_logs.remove(0).into();
    assert_eq!(
        l2_to_l1_log,
        L2ToL1Log {
            l2_shard_id: 0,
            is_service: true,
            tx_number_in_block: receipt.transaction_index.unwrap() as u16,
            sender: address!("0x0000000000000000000000000000000000008001"),
            key: l2_tx_hash,
            value: B256::from(U256::from(1)),
        }
    );

    Ok(())
}

async fn l2_token_address(tester: &Tester, l1_token: Address) -> anyhow::Result<Address> {
    let l2_asset_router_address = address!("0x0000000000000000000000000000000000010003");
    let l2_asset_router = IL2AssetRouter::new(l2_asset_router_address, tester.l2_provider.clone());
    Ok(l2_asset_router.l2TokenAddress(l1_token).call().await?)
}
