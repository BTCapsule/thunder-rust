use std::{collections::HashMap, sync::Arc};

use parking_lot::RwLock;
use thunder::{
    bip300301::{self, bitcoin, MainClient},
    format_deposit_address,
    miner::{self, Miner},
    node::{self, Node, THIS_SIDECHAIN},
    types::{self, OutPoint, Output, Transaction},
    wallet::{self, Wallet},
};
use tokio::sync::RwLock as TokioRwLock;

use crate::cli::Config;

#[derive(Clone)]
pub struct App {
    pub node: Arc<Node>,
    pub wallet: Arc<Wallet>,
    pub miner: Arc<TokioRwLock<Miner>>,
    pub utxos: Arc<RwLock<HashMap<OutPoint, Output>>>,
    pub transaction: Transaction,
    pub runtime: Arc<tokio::runtime::Runtime>,
}

impl App {
    pub fn new(config: &Config) -> Result<Self, Error> {
        // Node launches some tokio tasks for p2p networking, that is why we need a tokio runtime
        // here.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        let wallet = Wallet::new(&config.datadir.join("wallet.mdb"))?;
        let miner = Miner::new(
            THIS_SIDECHAIN,
            config.main_addr,
            &config.main_user,
            &config.main_password,
        )?;
        let node = runtime.block_on(async {
            let node = match Node::new(
                &config.datadir,
                config.net_addr,
                config.main_addr,
                &config.main_user,
                &config.main_password,
            ) {
                Ok(node) => node,
                Err(err) => return Err(err),
            };
            Ok(node)
        })?;
        let utxos = {
            let mut utxos = wallet.get_utxos()?;
            let transactions = node.get_all_transactions()?;
            for transaction in &transactions {
                for input in &transaction.transaction.inputs {
                    utxos.remove(input);
                }
            }
            utxos
        };
        Ok(Self {
            node: Arc::new(node),
            wallet: Arc::new(wallet),
            miner: Arc::new(TokioRwLock::new(miner)),
            utxos: Arc::new(RwLock::new(utxos)),
            transaction: Transaction {
                inputs: vec![],
                outputs: vec![],
            },
            runtime: Arc::new(runtime),
        })
    }

    pub fn sign_and_send(&mut self) -> Result<(), Error> {
        let authorized_transaction =
            self.wallet.authorize(self.transaction.clone())?;
        self.runtime
            .block_on(self.node.submit_transaction(&authorized_transaction))?;
        self.transaction = Transaction {
            inputs: vec![],
            outputs: vec![],
        };
        self.update_utxos()?;
        Ok(())
    }

    pub fn get_new_main_address(
        &self,
    ) -> Result<bitcoin::Address<bitcoin::address::NetworkChecked>, Error> {
        let address = self.runtime.block_on({
            let miner = self.miner.clone();
            async move {
                let miner_read = miner.read().await;
                miner_read
                    .drivechain
                    .client
                    .getnewaddress("", "legacy")
                    .await
            }
        })?;
        let address: bitcoin::Address<bitcoin::address::NetworkChecked> =
            address.require_network(bitcoin::Network::Regtest).unwrap();
        Ok(address)
    }

    const EMPTY_BLOCK_BMM_BRIBE: bip300301::bitcoin::Amount =
        bip300301::bitcoin::Amount::from_sat(1000);

    pub async fn mine(
        &self,
        fee: Option<bip300301::bitcoin::Amount>,
    ) -> Result<(), Error> {
        const NUM_TRANSACTIONS: usize = 1000;
        let (txs, tx_fees) = self.node.get_transactions(NUM_TRANSACTIONS)?;
        let coinbase = match tx_fees {
            0 => vec![],
            _ => vec![types::Output {
                address: self.wallet.get_new_address()?,
                content: types::Content::Value(tx_fees),
            }],
        };
        let body = types::Body::new(txs, coinbase);
        let prev_side_hash = self.node.get_best_hash()?;
        let prev_main_hash = self
            .miner
            .read()
            .await
            .drivechain
            .get_mainchain_tip()
            .await?;
        let header = types::Header {
            merkle_root: body.compute_merkle_root(),
            prev_side_hash,
            prev_main_hash,
        };
        let bribe = fee.unwrap_or_else(|| {
            if tx_fees > 0 {
                bip300301::bitcoin::Amount::from_sat(tx_fees)
            } else {
                Self::EMPTY_BLOCK_BMM_BRIBE
            }
        });
        let mut miner_write = self.miner.write().await;
        miner_write
            .attempt_bmm(bribe.to_sat(), 0, header, body)
            .await?;
        // miner_write.generate().await?;
        tracing::trace!("confirming bmm...");
        if let Some((header, body)) = miner_write.confirm_bmm().await? {
            tracing::trace!("confirmed bmm, submitting block");
            self.node.submit_block(&header, &body).await?;
        }
        self.update_wallet()?;
        self.update_utxos()?;
        Ok(())
    }

    fn update_wallet(&self) -> Result<(), Error> {
        let addresses = self.wallet.get_addresses()?;
        let utxos = self.node.get_utxos_by_addresses(&addresses)?;
        let outpoints: Vec<_> = self.wallet.get_utxos()?.into_keys().collect();
        let spent: Vec<_> = self
            .node
            .get_spent_utxos(&outpoints)?
            .into_iter()
            .map(|(outpoint, spent_output)| (outpoint, spent_output.inpoint))
            .collect();
        self.wallet.put_utxos(&utxos)?;
        self.wallet.spend_utxos(&spent)?;
        Ok(())
    }

    fn update_utxos(&self) -> Result<(), Error> {
        let mut utxos = self.wallet.get_utxos()?;
        let transactions = self.node.get_all_transactions()?;
        for transaction in &transactions {
            for input in &transaction.transaction.inputs {
                utxos.remove(input);
            }
        }
        *self.utxos.write() = utxos;
        Ok(())
    }

    pub fn deposit(
        &mut self,
        amount: bitcoin::Amount,
        fee: bitcoin::Amount,
    ) -> Result<(), Error> {
        self.runtime.block_on(async {
            let address = self.wallet.get_new_address()?;
            let address =
                format_deposit_address(THIS_SIDECHAIN, &format!("{address}"));
            self.miner
                .read()
                .await
                .drivechain
                .client
                .createsidechaindeposit(
                    THIS_SIDECHAIN,
                    &address,
                    amount.into(),
                    fee.into(),
                )
                .await?;
            Ok(())
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("node error")]
    Node(#[from] node::Error),
    #[error("wallet error")]
    Wallet(#[from] wallet::Error),
    #[error("miner error")]
    Miner(#[from] miner::Error),
    #[error("drivechain error")]
    Drivechain(#[from] bip300301::Error),
    #[error("io error")]
    Io(#[from] std::io::Error),
    #[error("jsonrpsee error")]
    Jsonrpsee(#[from] jsonrpsee::core::Error),
}