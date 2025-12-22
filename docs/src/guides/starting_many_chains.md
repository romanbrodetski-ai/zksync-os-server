# Starting many chains

## Creating ecosystem with 1 chain

Use commands from [generate_l1_state.md] to generate ecosystem with a single chain.

Put all its data into 'ecosystem' dir.

## Adding new chain

Now go to the ecosystem dir and run:

For zkstack - use the path that the tool above would print.

```shell
PATH_TO/zkstack chain create --chain-name gateway --chain-id 277  --wallet-creation random  --prover-mode no-proofs --l1-batch-commit-data-generator-mode rollup  --base-token-address 0x0000000000000000000000000000000000000001 --base-token-price-nominator 1 --base-token-price-denominator 1 --evm-emulator false --zksync-os 
```

Now you have to create (empty) general.yaml file in chains/gateway/configs

Now send the funds to all the wallets there:
(TODO: use the script from update.sh)


Now we have to init the chain:

```
PATH_TO/zkstack chain init  --deploy-paymaster=false --l1-rpc-url=http://localhost:8545  --chain gateway
```

Now update general.yaml with the ports:
```
api:
  prometheus:
    listener_port: 4812
  web3_json_rpc:
    http_port: 4850
  merkle_tree:
    port: 4872
data_handler:
  http_port: 4820
```

Now you can run the chain (from zksync-os-server dir)

```shell
STATUS_SERVER_ADDRESS=0.0.0.0:3871 general_zkstack_cli_config_dir=../ecosystem/.tmp/ecosystem/local_v1/chains/gateway  cargo run --release
```

Converting to gateway:

```shell
zkstack chain gateway create-tx-filterer --chain gateway
zkstack chain gateway convert-to-gateway --chain gateway
```

## Issues:

Contracts might fail due to compilation issues on 'convert-to-gateway' steps.
You might also have to fill out the 'secrets.yaml' to put the l1_rpc address there.
