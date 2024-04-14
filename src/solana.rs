use std::time::Duration;

use anyhow::{anyhow, Context, Result};

use serde::de::DeserializeOwned;
use solana_client::{
    nonblocking::rpc_client::RpcClient,
    rpc_client::SerializableTransaction,
    rpc_config::{RpcSendTransactionConfig, RpcSimulateTransactionConfig},
};
use solana_sdk::{
    account::Account, clock::Slot, commitment_config::CommitmentConfig, hash::Hash, pubkey::Pubkey,
    transaction::Transaction,
};
use solana_transaction_status::UiTransactionEncoding;
use tracing::{debug, info};

// TODO: Make configurable.
const DEFAULT_RPC_COMMITMENT: CommitmentConfig = CommitmentConfig::processed();

pub struct SolanaRpc {
    client: RpcClient,
}

impl SolanaRpc {
    pub fn new(url: String, timeout: Duration) -> Self {
        Self {
            client: RpcClient::new_with_timeout_and_commitment(
                url,
                timeout,
                DEFAULT_RPC_COMMITMENT,
            ),
        }
    }

    pub async fn get_blockhash_and_slot(&self) -> Result<(Hash, Slot)> {
        self.client
            .get_latest_blockhash_with_commitment(self.client.commitment())
            .await
            .context("Unable to get latest blockhash")
    }

    pub async fn read_account<Account>(&self, address: &Pubkey) -> Result<Option<Account>>
    where
        Account: DeserializeOwned,
    {
        let maybe_data = self
            .read_account_data(address)
            .await
            .with_context(|| format!("Unable to read data for account {}", address))?;
        match maybe_data {
            Some(data) => Ok(Some(bincode::deserialize::<Account>(&data).with_context(
                || format!("Unable to deserialize data for account {}", address),
            )?)),

            None => Ok(None),
        }
    }

    pub async fn read_account_data(&self, address: &Pubkey) -> Result<Option<Vec<u8>>> {
        match self.client.get_account_data(address).await {
            Ok(data) => Ok(Some(data)),

            Err(error) => {
                if format!("{}", error).contains("AccountNotFound") {
                    Ok(None)
                } else {
                    Err(error).context("Account read failed")
                }
            }
        }
    }

    pub async fn simulate(
        &self,
        transaction: &Transaction,
        slot: Slot,
        transaction_is_complete: bool,
    ) -> Result<u64> {
        self.client
            .simulate_transaction_with_config(
                transaction,
                RpcSimulateTransactionConfig {
                    sig_verify: transaction_is_complete,
                    replace_recent_blockhash: !transaction_is_complete,
                    commitment: Some(self.client.commitment()),
                    encoding: Some(UiTransactionEncoding::Base64),
                    accounts: None,
                    min_context_slot: Some(slot),
                    inner_instructions: false,
                },
            )
            .await
            .context("Simulation request failed")
            .and_then(|response| {
                if let Some(error) = response.value.err {
                    Err(anyhow!("Simulation failed: {}", error))
                } else {
                    Ok(response
                        .value
                        .units_consumed
                        .expect("Logic error: no units consumed but no error either"))
                }
            })
    }

    pub async fn get_account(&self, address: &Pubkey) -> Result<Account> {
        self.client
            .get_account(address)
            .await
            .with_context(|| format!("Unable to get account {}", address))
    }

    pub async fn send_and_confirm(&self, transaction: &Transaction) -> Result<()> {
        let transaction_id = transaction.get_signature();
        info!("Sending and configrming transacton {}", transaction_id);

        let _ = self
            .client
            .send_and_confirm_transaction(transaction)
            .await
            .context("Failed to send and confirm transaction")?;

        info!("Transacton {} confirmed", transaction_id);
        Ok(())
    }

    pub async fn send_fast(&self, transaction: &Transaction) -> Result<()> {
        let send_config = RpcSendTransactionConfig {
            skip_preflight: true,
            preflight_commitment: Some(solana_sdk::commitment_config::CommitmentLevel::Processed),
            encoding: Some(UiTransactionEncoding::Base64),
            max_retries: Some(usize::MAX),
            min_context_slot: None,
        };

        let transaction_id = transaction.get_signature();
        debug!("Sending transacton {}", transaction_id);

        self
            .client
            .send_transaction_with_config(transaction, send_config)
            .await
            .context("Failed to send transaction")
            .map(|signature| {
                info!("Transacton {} sent", signature);
            })
    }
}
