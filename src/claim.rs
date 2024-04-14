use std::collections::BTreeMap;
use std::{num::NonZeroUsize, str::FromStr, time::Duration};

use anyhow::{ensure, Context, Result};
use futures::future::try_join_all;
use solana_sdk::hash::Hash;
use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::native_token::lamports_to_sol;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Keypair;
use solana_sdk::signer::Signer;
use solana_sdk::transaction::Transaction;
use spl_associated_token_account::get_associated_token_address;
use spl_associated_token_account::instruction::create_associated_token_account;
use tracing::info;
use tracing_log::log::trace;

use crate::ore::{
    self, generate_proof_addresses, read_proof_accounts, ClaimArgs, OreInstruction, MINT_ADDRESS,
    TREASURY_ADDRESS,
};
use crate::{
    keys::{generate_key_pairs_incl_base, read_keypair},
    solana::SolanaRpc,
};

// How many claiming instructions we can stuff into one transaction.
const MINI_BATCH_LEN: usize = 6;

pub async fn claim(
    url: String,
    keypair: String,
    accounts: NonZeroUsize,
    timeout: Duration,
    beneficiary: String,
) -> Result<()> {
    let solana = SolanaRpc::new(url, timeout);

    let fee_payer = read_keypair(&keypair)?;
    let keypairs = generate_key_pairs_incl_base(&fee_payer, accounts);

    let beneficiary = Pubkey::from_str(&beneficiary)
        .with_context(|| format!("Invalid beneficiary address: {}", beneficiary))?;

    // Read proof accounts.
    let proof_addresses = generate_proof_addresses(keypairs.keys());
    let proofs = read_proof_accounts(&solana, &proof_addresses).await?;
    let rewards: Vec<_> = proofs.iter().map(|proof| proof.claimable_rewards).collect();

    // Check beneficiary account.
    let beneficiary_acc = solana.get_account(&beneficiary).await?;
    ensure!(
        beneficiary_acc.lamports != 0,
        "Beneficiary account {} is not funded",
        beneficiary,
    );
    info!(
        "Beneficiary balance: {}",
        lamports_to_sol(beneficiary_acc.lamports)
    );

    // Generate claiming transactions.
    let claims: Vec<_> = keypairs
        .keys()
        .zip(
            proof_addresses
                .iter()
                .map(|(proof_address, _)| proof_address),
        )
        .zip(rewards.iter())
        .map(|((signer, proof_address), amount)| (signer, proof_address, amount))
        .filter(|(_, _, amount)| **amount != 0)
        .collect();
    if claims.is_empty() {
        info!("No unclaimed ORE rewards");
        return Ok(());
    }

    let claims = {
        let mut mini_batches = Vec::new();
        for batch_start in (0..claims.len()).step_by(MINI_BATCH_LEN) {
            let mini_batch =
                claims[batch_start..(batch_start + MINI_BATCH_LEN).min(claims.len())].to_vec();
            mini_batches.push(mini_batch);
        }
        mini_batches
    };
    info!("{} claiming transaction batches generated", claims.len());

    // Check associated token account.
    let token_address = get_associated_token_address(&beneficiary, &MINT_ADDRESS);
    if solana.read_account_data(&token_address).await?.is_none() {
        info!(
            "Associated token account {} does not exist, creating",
            token_address
        );

        let instruction = create_associated_token_account(
            &fee_payer.pubkey(),
            &beneficiary,
            &MINT_ADDRESS,
            &spl_token::id(),
        );
        let mut transaction =
            Transaction::new_with_payer(&[instruction], Some(&fee_payer.pubkey()));

        trace!("ATA create transaction: {:#?}", transaction);

        let (blockhash, _) = solana.get_blockhash_and_slot().await?;

        transaction.sign(&[&fee_payer], blockhash);
        assert!(
            transaction.is_signed(),
            "Logic error: transaction is not fully signed"
        );

        solana.send_and_confirm(&transaction).await?;
    } else {
        info!("Associated token account {} already exist", token_address);
    };

    let (blockhash, _) = solana.get_blockhash_and_slot().await?;
    let claims: Vec<_> = claims
        .iter()
        .map(|mini_batch| {
            build_claim_transaction(&fee_payer, &keypairs, mini_batch, &token_address, blockhash)
        })
        .map(|transaction| {
            let solana = &solana;
            async move { solana.send_and_confirm(&transaction).await }
        })
        .collect();

    let _ = try_join_all(claims).await?;

    info!("Done!");
    Ok(())
}

fn build_claim_transaction(
    fee_payer: &Keypair,
    keypairs: &BTreeMap<Pubkey, Keypair>,
    mini_batch: &[(&Pubkey, &Pubkey, &u64)],
    token_address: &Pubkey,
    blockhash: Hash,
) -> Transaction {
    let instructions: Vec<_> = mini_batch
        .iter()
        .map(|(signer, proof_address, amount)| {
            build_claim_instruction(signer, token_address, proof_address, **amount)
        })
        .collect();

    let mut transaction = Transaction::new_with_payer(&instructions, Some(&fee_payer.pubkey()));

    // Sign.
    let mut already_have_fee_payer = false;
    for (signer, _, _) in mini_batch {
        transaction.partial_sign(&[&keypairs[signer]], blockhash);

        if **signer == fee_payer.pubkey() {
            already_have_fee_payer = true;
        }
    }

    if !already_have_fee_payer {
        transaction.partial_sign(&[fee_payer], blockhash);
    }

    transaction
}

fn build_claim_instruction(
    signer: &Pubkey,
    token_address: &Pubkey,
    proof_address: &Pubkey,
    amount: u64,
) -> Instruction {
    let treasury_tokens = get_associated_token_address(&TREASURY_ADDRESS, &MINT_ADDRESS);

    Instruction {
        program_id: ore::id(),
        accounts: vec![
            AccountMeta::new(*signer, true),
            AccountMeta::new(*token_address, false),
            AccountMeta::new(*proof_address, false),
            AccountMeta::new(TREASURY_ADDRESS, false),
            AccountMeta::new(treasury_tokens, false),
            AccountMeta::new_readonly(spl_token::id(), false),
        ],
        data: [
            OreInstruction::Claim.get_vec(),
            bytemuck::bytes_of(&ClaimArgs {
                amount: amount.to_le_bytes(),
            })
            .to_vec(),
        ]
        .concat(),
    }
}
