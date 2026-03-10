#![cfg(feature = "prover-tests")]

use zksync_os_integration_tests::{
    CURRENT_TO_L1, NEXT_TO_GATEWAY, NEXT_TO_L1, TesterBuilder, test_casing,
};

#[test_casing([CURRENT_TO_L1, NEXT_TO_L1, NEXT_TO_GATEWAY])]
#[test_log::test(tokio::test)]
async fn prover(builder: TesterBuilder) -> anyhow::Result<()> {
    let tester = builder.enable_prover().build().await?;
    tester.prover_tester.wait_for_batch_proven(1).await?;
    Ok(())
}
