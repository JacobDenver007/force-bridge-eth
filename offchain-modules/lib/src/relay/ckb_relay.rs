use crate::transfer::to_eth::{get_add_ckb_headers_func, get_msg_hash, get_msg_signature};
use crate::util::ckb_proof_helper::CBMT;
use crate::util::ckb_tx_generator::Generator;
use crate::util::ckb_util::covert_to_h256;
use crate::util::config::ForceConfig;
use crate::util::eth_util::{
    convert_eth_address, parse_private_key, parse_secret_key, relay_header_transaction, Web3Client,
};
use anyhow::{anyhow, bail, Result};
use ckb_sdk::HttpRpcClient;
use ethabi::Token;
use ethereum_types::U256;
use log::info;
use secp256k1::SecretKey;
use std::time::Instant;
use web3::types::{CallRequest, H160, H256};

pub struct CKBRelayer {
    pub contract_addr: H160,
    pub priv_key: H256,
    pub ckb_client: Generator,
    pub web3_client: Web3Client,
    pub gas_price: U256,
    pub multisig_privkeys: Vec<SecretKey>,
    pub ckb_rpc_url: String,
    pub eth_rpc_url: String,
    pub ckb_init_height: u64,
}

impl CKBRelayer {
    pub fn new(
        config_path: String,
        network: Option<String>,
        priv_key_path: String,
        multisig_privkeys: Vec<String>,
        gas_price: u64,
    ) -> Result<CKBRelayer> {
        let force_config = ForceConfig::new(config_path.as_str())?;
        let deployed_contracts = force_config
            .deployed_contracts
            .as_ref()
            .ok_or_else(|| anyhow!("contracts should be deployed"))?;

        if multisig_privkeys.len() < deployed_contracts.ckb_relay_mutlisig_threshold.threshold {
            bail!(
                "the mutlisig privkeys number is less. expect {}, actual {} ",
                deployed_contracts.ckb_relay_mutlisig_threshold.threshold,
                multisig_privkeys.len()
            );
        }

        let eth_rpc_url = force_config.get_ethereum_rpc_url(&network)?;
        let ckb_rpc_url = force_config.get_ckb_rpc_url(&network)?;
        let ckb_indexer_url = force_config.get_ckb_indexer_url(&network)?;
        let priv_key = parse_private_key(&priv_key_path, &force_config, &network)?;
        let multisig_privkeys = multisig_privkeys
            .into_iter()
            .map(|k| parse_private_key(&k, &force_config, &network))
            .collect::<Result<Vec<H256>>>()?;

        let contract_addr = convert_eth_address(&deployed_contracts.eth_ckb_chain_addr)?;
        let mut ckb_client =
            Generator::new(ckb_rpc_url.clone(), ckb_indexer_url, Default::default())
                .map_err(|e| anyhow!("failed to crate generator: {}", e))?;
        let web3_client = Web3Client::new(eth_rpc_url.clone());
        let gas_price = U256::from(gas_price);

        let ckb_init_height = CKBRelayer::get_ckb_contract_deloy_height(
            &mut ckb_client,
            deployed_contracts
                .recipient_typescript
                .outpoint
                .tx_hash
                .clone(),
        )?;

        Ok(CKBRelayer {
            ckb_rpc_url,
            eth_rpc_url,
            ckb_init_height,
            contract_addr,
            priv_key,
            ckb_client,
            web3_client,
            gas_price,
            multisig_privkeys: multisig_privkeys
                .iter()
                .map(|&privkey| parse_secret_key(privkey))
                .collect::<Result<Vec<SecretKey>>>()?,
        })
    }

    pub async fn start(&mut self) -> Result<()> {
        let ckb_current_height = self
            .ckb_client
            .rpc_client
            .get_tip_block_number()
            .map_err(|e| anyhow!("failed to get ckb current height : {}", e))?;
        let merkle_root = self.get_history_merkle_root(self.ckb_init_height, ckb_current_height)?;
        let nonce = self.web3_client.get_eth_nonce(&self.priv_key).await?;
        let sign_tx = self
            .relay_headers(self.ckb_init_height, ckb_current_height, merkle_root, nonce)
            .await?;
        let task_future = relay_header_transaction(self.eth_rpc_url.clone(), sign_tx);
        let timeout_future = tokio::time::delay_for(std::time::Duration::from_secs(1800));
        let now = Instant::now();
        tokio::select! {
            v = task_future => { v?; }
            _ = timeout_future => {
                bail!("relay headers timeout");
            }
        }
        info!("relay headers time elapsed: {:?}", now.elapsed());
        Ok(())
    }

    pub async fn relay_headers(
        &mut self,
        init_block_number: u64,
        latest_block_number: u64,
        history_tx_root: [u8; 32],
        asec_nonce: U256,
    ) -> Result<Vec<u8>> {
        info!("relay headers. init_block_number: {:?}, latest_block_number: {:?}, history_tx_root: {:?}, asec_nonce: {:?}",
            init_block_number,
            latest_block_number,
            &history_tx_root,
            &asec_nonce,
        );
        let add_headers_func = get_add_ckb_headers_func();
        let chain_id = self.web3_client.client().eth().chain_id().await?;

        let headers_msg_hash = get_msg_hash(
            chain_id,
            self.contract_addr,
            init_block_number,
            latest_block_number,
            history_tx_root,
        )?;

        let mut signatures: Vec<u8> = vec![];
        for &privkey in self.multisig_privkeys.iter() {
            let mut signature = get_msg_signature(&headers_msg_hash, privkey)?;
            signatures.append(&mut signature);
        }
        info!("msg signatures {}", hex::encode(&signatures));

        let add_headers_abi = add_headers_func.encode_input(&[
            Token::Uint(init_block_number.into()),
            Token::Uint(latest_block_number.into()),
            Token::FixedBytes(history_tx_root.to_vec()),
            Token::Bytes(signatures),
        ])?;
        let gas_price = self.web3_client.client().eth().gas_price().await?.as_u128();

        let request = CallRequest {
            from: None,
            to: Some(self.contract_addr),
            gas: None,
            gas_price: None,
            value: Some(0x0.into()),
            data: Some(add_headers_abi.to_vec().into()),
        };
        let gas_limit = self
            .web3_client
            .client()
            .eth()
            .estimate_gas(request, None)
            .await?
            .as_u128();
        let signed_tx = self
            .web3_client
            .build_sign_tx(
                self.contract_addr,
                self.priv_key,
                add_headers_abi,
                U256::from(gas_price),
                Some(U256::from(gas_limit)),
                U256::from(0u64),
                asec_nonce,
            )
            .await?;
        Ok(signed_tx)
    }

    pub fn get_ckb_contract_deloy_height(
        ckb_client: &mut Generator,
        tx_hash: String,
    ) -> Result<u64> {
        let hash = covert_to_h256(&tx_hash)?;

        let block_hash = ckb_client
            .rpc_client
            .get_transaction(hash)
            .map_err(|err| anyhow!(err))?
            .ok_or_else(|| anyhow!("failed to get block height : tx is none"))?
            .tx_status
            .block_hash
            .ok_or_else(|| anyhow!("failed to get block height : block hash is none"))?;

        let ckb_height = ckb_client
            .rpc_client
            .get_block(block_hash)
            .map_err(|err| anyhow!(err))?
            .ok_or_else(|| anyhow!("failed to get block height : block is none"))?
            .header
            .inner
            .number;
        Ok(ckb_height)
    }

    pub fn get_history_merkle_root(
        &self,
        start_height: u64,
        latest_height: u64,
    ) -> Result<[u8; 32]> {
        let mut rpc_client = HttpRpcClient::new(self.ckb_rpc_url.clone());
        let mut all_tx_roots = vec![];
        // TODO: use rocksdb here to persist header data
        for number in start_height..=latest_height {
            match rpc_client
                .get_header_by_number(number)
                .map_err(|e| anyhow!("get_header_by_number err: {:?}", e))?
            {
                Some(header_view) => {
                    let root = header_view.inner.transactions_root;
                    all_tx_roots.push(root.0)
                }
                None => {
                    bail!(
                        "cannot get the block transactions root, block_number = {}",
                        number
                    );
                }
            }
        }
        Ok(CBMT::build_merkle_root(&all_tx_roots))
    }
}
