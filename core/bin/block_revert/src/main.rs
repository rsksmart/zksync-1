use anyhow::{bail, ensure, format_err};
use ethabi::Token;
use std::str::FromStr;
use structopt::StructOpt;
use tokio::time::Duration;
use web3::{
    contract::Options,
    types::{TransactionReceipt, U256, U64},
};
use zksync_config::{ContractsConfig, ETHClientConfig, ETHSenderConfig};
use zksync_eth_client::RootstockGateway;
use zksync_storage::StorageProcessor;
use zksync_types::{aggregated_operations::stored_block_info, block::Block, BlockNumber, H256};

// TODO: don't use anyhow (ZKS-588)
async fn revert_blocks_in_storage(
    storage: &mut StorageProcessor<'_>,
    last_block: BlockNumber,
) -> anyhow::Result<()> {
    let mut transaction = storage.start_transaction().await?;

    transaction
        .chain()
        .mempool_schema()
        .return_executed_txs_to_mempool(last_block)
        .await?;
    println!("`mempool_txs`, `executed_transactions` tables are updated");
    transaction
        .chain()
        .state_schema()
        .clear_current_nonce_table(last_block)
        .await?;
    println!("`committed_nonce` table is updated");
    transaction
        .chain()
        .block_schema()
        .remove_blocks(last_block)
        .await?;
    println!("`block` table is cleaned");
    transaction
        .chain()
        .block_schema()
        .remove_pending_block()
        .await?;
    println!("`pending_block` table is cleaned");
    transaction
        .chain()
        .tree_cache_schema_bincode()
        .remove_new_account_tree_cache(last_block)
        .await?;
    println!("`account_tree_cache` table is cleaned");

    transaction
        .chain()
        .state_schema()
        .remove_account_balance_updates(last_block)
        .await?;
    println!("`account_balance_updates` table is cleaned");
    transaction
        .chain()
        .state_schema()
        .remove_account_creates(last_block)
        .await?;
    println!("`account_creates` table is cleaned");
    transaction
        .chain()
        .state_schema()
        .remove_account_pubkey_updates(last_block)
        .await?;
    println!("`account_pubkey_updates` table is cleaned");

    transaction
        .chain()
        .state_schema()
        .remove_mint_nft_updates(last_block)
        .await?;
    println!("`mint_nft_updates` table is cleaned");

    transaction
        .chain()
        .operations_schema()
        .remove_eth_unprocessed_aggregated_ops()
        .await?;
    println!("`eth_unprocessed_aggregated_ops` table is cleaned");
    transaction
        .chain()
        .operations_schema()
        .remove_aggregate_operations_and_bindings(last_block)
        .await?;
    println!("`aggregate_operations`, `eth_aggregated_ops_binding`, `eth_tx_hashes`, `eth_operations` tables are cleaned");

    transaction
        .prover_schema()
        .remove_witnesses(last_block)
        .await?;
    println!("`block_witness` table is cleaned");
    transaction
        .prover_schema()
        .remove_proofs(last_block)
        .await?;
    println!("`proofs` table is cleaned");
    transaction
        .prover_schema()
        .remove_aggregated_proofs(last_block)
        .await?;
    println!("`aggregated_proofs` table is cleaned");
    transaction
        .prover_schema()
        .remove_prover_jobs(last_block)
        .await?;
    println!("`prover_job_queue` table is cleaned");

    transaction
        .rootstock_schema()
        .update_eth_parameters(last_block)
        .await?;
    println!("`eth_parameters` table is updated");

    transaction.commit().await?;

    println!("Blocks were reverted in storage");
    Ok(())
}

// TODO: don't use anyhow (ZKS-588)
async fn send_raw_tx_and_wait_confirmation(
    client: &RootstockGateway,
    raw_tx: Vec<u8>,
) -> Result<TransactionReceipt, anyhow::Error> {
    let tx_hash = client
        .send_raw_tx(raw_tx)
        .await
        .map_err(|e| format_err!("Failed to send raw tx: {}", e))?;

    let mut poller = tokio::time::interval(Duration::from_millis(100));
    let start = std::time::Instant::now();
    let confirmation_timeout = Duration::from_secs(1000);

    loop {
        if let Some(receipt) = client
            .tx_receipt(tx_hash)
            .await
            .map_err(|e| format_err!("Failed to get receipt from eth node: {}", e))?
        {
            return Ok(receipt);
        }

        if start.elapsed() > confirmation_timeout {
            bail!("Operation timeout");
        }
        poller.tick().await;
    }
}

// TODO: don't use anyhow (ZKS-588)
async fn revert_blocks_on_contract(
    storage: &mut StorageProcessor<'_>,
    client: &RootstockGateway,
    blocks: &[Block],
) -> anyhow::Result<()> {
    let tx_arg = Token::Array(blocks.iter().map(stored_block_info).collect());
    let data = client.encode_tx_data("revertBlocks", tx_arg);
    let gas_limit = 200000 + 15000 * blocks.len();
    let signed_tx = client
        .sign_prepared_tx(data, Options::with(|f| f.gas = Some(U256::from(gas_limit))))
        .await
        .map_err(|e| format_err!("Revert blocks send err: {}", e))?;
    let receipt = send_raw_tx_and_wait_confirmation(client, signed_tx.raw_tx).await?;
    storage.rootstock_schema().get_next_nonce().await
        .expect("Rootstock tx has been sent but updating operator nonce in storage has failed. You need to update it manually");
    if receipt.status != Some(U64::from(1)) {
        let reason = client.failure_reason(signed_tx.hash).await?;
        anyhow::bail!("Tx to contract failed {:?}", reason);
    }

    println!("Blocks were reverted on contract");
    Ok(())
}

// TODO: don't use anyhow (ZKS-588)
async fn get_blocks(
    last_commited_block: BlockNumber,
    blocks_to_revert: u32,
    storage: &mut StorageProcessor<'_>,
) -> Result<Vec<Block>, anyhow::Error> {
    let mut blocks = Vec::new();
    let last_block_to_revert = *last_commited_block - blocks_to_revert + 1;
    let range_to_revert = last_block_to_revert..=*last_commited_block;
    for block_number in range_to_revert.rev() {
        let block = storage
            .chain()
            .block_schema()
            .get_block(BlockNumber(block_number))
            .await?
            .unwrap_or_else(|| panic!("No block {} in storage", block_number));
        blocks.push(block);
    }
    Ok(blocks)
}

#[derive(Debug, StructOpt)]
enum Command {
    /// Reverts blocks on contract and in storage
    All,
    /// Reverts blocks on contract
    Contract,
    /// Reverts blocks in storage
    Storage,
}

#[derive(Debug, StructOpt)]
#[structopt(name = "Rollup block revert tool", author = "Matter Labs")]
#[structopt(about = "Tool to revert blocks in Rollup network on contract and/or in storage")]
struct Opt {
    /// Last correct block, tool reverts blocks with numbers greater than this field.
    #[structopt(long)]
    last_correct_block: u32,
    #[structopt(subcommand)]
    command: Command,
    /// Private key of operator which will call the contract function.
    #[structopt(long = "key", env = "ETH_SENDER_SENDER_OPERATOR_PRIVATE_KEY")]
    operator_private_key: String,
}

// TODO: don't use anyhow (ZKS-588)
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let opt = Opt::from_args();

    let key_without_prefix = opt
        .operator_private_key
        .strip_prefix("0x")
        .unwrap_or(opt.operator_private_key.as_str());

    let contracts = ContractsConfig::from_env();
    let eth_client_config = ETHClientConfig::from_env();
    let mut eth_sender_config = ETHSenderConfig::from_env();

    eth_sender_config.sender.operator_private_key =
        H256::from_str(key_without_prefix).expect("Cannot deserialize private key");

    let mut storage = StorageProcessor::establish_connection().await?;
    let client = RootstockGateway::from_config(
        &eth_client_config,
        &eth_sender_config,
        contracts.contract_addr,
    );

    let last_commited_block = storage
        .chain()
        .block_schema()
        .get_last_committed_confirmed_block()
        .await?;
    let last_verified_block = storage
        .chain()
        .block_schema()
        .get_last_verified_confirmed_block()
        .await?;

    println!(
        "Last committed block {} verified {}",
        &last_commited_block, &last_verified_block
    );
    ensure!(
        *last_verified_block <= opt.last_correct_block,
        "Some blocks to revert are already verified"
    );

    let blocks_to_revert = *last_commited_block - opt.last_correct_block;
    let last_block = BlockNumber(opt.last_correct_block);

    match opt.command {
        Command::All => {
            println!("Start reverting blocks in database and in contract");
            let blocks = get_blocks(last_commited_block, blocks_to_revert, &mut storage).await?;
            println!("Last block for revert {}", &last_block);
            revert_blocks_on_contract(&mut storage, &client, &blocks).await?;
            revert_blocks_in_storage(&mut storage, last_block).await?;
        }
        Command::Contract => {
            println!("Start reverting blocks in contract");
            let blocks = get_blocks(last_commited_block, blocks_to_revert, &mut storage).await?;
            revert_blocks_on_contract(&mut storage, &client, &blocks).await?;
        }
        Command::Storage => {
            println!("Start reverting blocks in database");
            revert_blocks_in_storage(&mut storage, last_block).await?;
        }
    }

    Ok(())
}
