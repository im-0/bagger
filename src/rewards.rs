use std::{num::NonZeroUsize, time::Duration};

use anyhow::Result;

use crate::{
    keys::{generate_key_pairs_incl_base, read_keypair},
    ore::{generate_proof_addresses, read_proof_accounts},
    solana::SolanaRpc,
};

pub async fn get_ore_rewards(
    url: String,
    timeout: Duration,
    keypair: String,
    accounts: NonZeroUsize,
) -> Result<u64> {
    let solana = SolanaRpc::new(url, timeout);

    let fee_payer = read_keypair(&keypair)?;
    let keypairs = generate_key_pairs_incl_base(&fee_payer, accounts);

    let proof_addresses = generate_proof_addresses(keypairs.keys());
    let proofs = read_proof_accounts(&solana, &proof_addresses).await?;

    let rewards: Vec<_> = proofs.iter().map(|proof| proof.claimable_rewards).collect();
    Ok(rewards.iter().sum())
}
