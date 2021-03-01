use crate::util::ckb_tx_generator::Generator;
use crate::util::ckb_util::{parse_cell, parse_merkle_cell_data};
use crate::util::config::ForceConfig;
use crate::util::eth_util::Web3Client;
use crate::util::rocksdb;
use anyhow::{anyhow, Result};
use ckb_sdk::Address;
use ckb_types::packed::Script;
use force_sdk::cell_collector::get_live_cell_by_typescript;
use log::info;
use serde::export::Clone;
use shellexpand::tilde;
use sparse_merkle_tree::traits::Value;
use std::path::Path;
use std::str::FromStr;
use web3::types::U64;

pub const HEADER_LIMIT_IN_TX: usize = 14;

pub struct ETHRocksdb {
    pub eth_client: Web3Client,
    pub generator: Generator,
    pub config_path: String,
    pub config: ForceConfig,
}

impl ETHRocksdb {
    pub fn new(config_path: String, network: Option<String>) -> Result<Self> {
        let config_path = tilde(config_path.as_str()).into_owned();
        let force_config = ForceConfig::new(config_path.as_str())?;
        let deployed_contracts = force_config
            .deployed_contracts
            .as_ref()
            .ok_or_else(|| anyhow!("contracts should be deployed"))?;
        let eth_rpc_url = force_config.get_ethereum_rpc_url(&network)?;
        let ckb_rpc_url = force_config.get_ckb_rpc_url(&network)?;
        let ckb_indexer_url = force_config.get_ckb_indexer_url(&network)?;

        let generator = Generator::new(ckb_rpc_url, ckb_indexer_url, deployed_contracts.clone())
            .map_err(|e| anyhow::anyhow!(e))?;
        let eth_client = Web3Client::new(eth_rpc_url);
        let mut addresses = vec![];
        for item in deployed_contracts.multisig_address.addresses.clone() {
            let address = Address::from_str(&item).unwrap();
            addresses.push(address);
        }

        Ok(ETHRocksdb {
            eth_client,
            generator,
            config_path,
            config: force_config,
        })
    }

    pub async fn start(&mut self) -> Result<()> {
        // get the latest output cell
        let deployed_contracts = self
            .config
            .deployed_contracts
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no cell found"))?;
        let cell_script = parse_cell(
            deployed_contracts
                .light_client_cell_script
                .cell_script
                .as_str(),
        )?;
        self.do_naive_relay_loop(cell_script).await?;
        Ok(())
    }

    pub async fn naive_relay(
        &mut self,
        cell_script: Script,
        mut latest_submit_header_number: u64,
    ) -> Result<u64> {
        let force_config = ForceConfig::new(self.config_path.as_str())?;
        // make tx
        let cell =
            get_live_cell_by_typescript(&mut self.generator.indexer_client, cell_script.clone())
                .map_err(|err| anyhow::anyhow!(err))?
                .ok_or_else(|| anyhow::anyhow!("no cell found"))?;

        let last_cell_output_data = cell.output_data.as_bytes();

        let (start_height, latest_height, merkle_root) =
            parse_merkle_cell_data(last_cell_output_data.to_vec())?;

        if latest_submit_header_number >= latest_height {
            info!(
                "waiting for new eth header. latest_height: {}, latest_submit_header_number: {}",
                latest_height, latest_submit_header_number
            );
            return Ok(latest_submit_header_number);
        }

        let eth_rocksdb_path = force_config.rocksdb_path;
        let db_dir = shellexpand::tilde(eth_rocksdb_path.as_str()).into_owned();
        let db_path = Path::new(db_dir.as_str());
        let mut smt_tree = match db_path.exists() {
            false => {
                let rocksdb_store = rocksdb::RocksDBStore::new(eth_rocksdb_path.clone());
                rocksdb::SMT::new(sparse_merkle_tree::H256::zero(), rocksdb_store)
            }
            true => {
                let rocksdb_store = rocksdb::RocksDBStore::open(eth_rocksdb_path.clone());
                rocksdb::SMT::new(merkle_root.into(), rocksdb_store)
            }
        };

        let mut number = latest_height;
        while number >= start_height {
            let block_number = U64([number]);

            let mut key = [0u8; 32];
            let mut height = [0u8; 8];
            height.copy_from_slice(number.to_le_bytes().as_ref());
            key[..8].clone_from_slice(&height);

            let chain_block = self.eth_client.get_block(block_number.into()).await?;
            let chain_block_hash = chain_block.hash.expect("block hash should not be none");

            let db_block_hash = smt_tree.get(&key.into()).expect("should return ok");
            if chain_block_hash.0.as_slice() != db_block_hash.to_h256().as_slice() {
                smt_tree
                    .update(key.into(), chain_block_hash.0.into())
                    .expect("update smt tree");
            } else {
                break;
            }
            number = number - 1;
        }

        let rocksdb_store = smt_tree.store_mut();
        rocksdb_store.commit();
        info!(
            "Successfully relayed the headers from {} to {}",
            number + 1,
            latest_height
        );
        latest_submit_header_number = latest_height;
        Ok(latest_submit_header_number)
    }

    pub async fn do_naive_relay_loop(&mut self, cell_script: Script) -> Result<()> {
        let mut latest_submit_header_number = 0;
        loop {
            let res = self
                .naive_relay(cell_script.clone(), latest_submit_header_number)
                .await;
            match res {
                Ok(new_submit_header_number) => {
                    latest_submit_header_number = new_submit_header_number
                }
                Err(e) => log::error!(
                    "unexpected error relay header from {}, err: {}",
                    latest_submit_header_number,
                    e
                ),
            }
            tokio::time::delay_for(std::time::Duration::from_secs(30)).await;
        }
    }
}
