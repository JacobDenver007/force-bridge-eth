use anyhow::Result;
use force_eth_lib::dapp::indexer::eth_indexer::EthIndexer;
use types::*;

pub mod types;

pub async fn dapp_handle(command: DappCommand) -> Result<()> {
    match command {
        DappCommand::Server(args) => server(args).await,
        DappCommand::ETHIndexer(args) => eth_indexer(args).await,
        DappCommand::CKBIndexer(args) => ckb_indexer(args).await,
        DappCommand::CkbTxRelayer(args) => ckb_tx_relay(args).await,
        DappCommand::EthTxRelayer(args) => eth_tx_relay(args).await,
    }
}

async fn server(_args: ServerArgs) -> Result<()> {
    // TODO
    Ok(())
}

async fn eth_indexer(args: IndexerArgs) -> Result<()> {
    let mut eth_indexer = EthIndexer::new(args.config_path, args.network, args.db_path).await?;
    loop {
        let res = eth_indexer.start().await;
        if let Err(err) = res {
            log::error!("An error occurred during the eth_indexer. Err: {:?}", err)
        }
        tokio::time::delay_for(std::time::Duration::from_secs(1)).await;
    }
}

async fn ckb_indexer(_args: IndexerArgs) -> Result<()> {
    // TODO

    // loop {
    //     ckb_monitor.start();
    // }
    Ok(())
}

async fn ckb_tx_relay(_args: CkbTxRelayerArgs) -> Result<()> {
    // TODO
    Ok(())
}

async fn eth_tx_relay(_args: EthTxRelayerArgs) -> Result<()> {
    // TODO
    Ok(())
}