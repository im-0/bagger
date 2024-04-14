use std::{
    collections::BTreeMap,
    fmt::{self, Display, Formatter},
    mem::replace,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{anyhow, Result};
use futures::future::try_join_all;
use solana_sdk::{
    clock::{Clock, Slot},
    compute_budget::ComputeBudgetInstruction,
    hash::Hash,
    instruction::{AccountMeta, Instruction},
    keccak,
    pubkey::Pubkey,
    signature::Keypair,
    signer::Signer as _,
    system_instruction, sysvar,
    transaction::Transaction,
};
use tokio::{sync::watch, time::sleep, try_join};
use tracing::{debug, error, info, trace, warn};

use crate::{
    cli::SignerOptions,
    keys::{generate_key_pairs_incl_base, read_keypair},
    ore::{
        self, generate_proof_addresses, read_maybe_proof_accounts, OreCommon, BUS_ADDRESSES,
        TREASURY_ADDRESS,
    },
    pow::{
        client::PoWClient,
        types::{NextHash, PoWRequest},
    },
    signals::ExitFlag,
    solana::SolanaRpc,
};

use crate::ore::{MineArgs, OreInstruction, RegisterArgs, EPOCH_DURATION};

const COMMON_ITER_DURATION: Duration = Duration::from_millis(200);

// How many mining instructions we can stuff into one transaction.
const MINI_BATCH_LEN: usize = 5;

// TODO: Unhardcode?
const PROOF_ACCOUNT_RENT: u64 = 1_559_040;

const COMPUTE_BUDGET_MULTIPLIER: f64 = 1.2;

pub struct Signer {
    priority_fee: u64,
    max_transaction_age: Slot,

    fee_payer: Pubkey,
    keypairs: BTreeMap<Pubkey, Keypair>,
    pow: PoWClient,
    solana: SolanaRpc,
    solana_sim: SolanaRpc,
    solana_sending: SolanaRpc,
}

impl Signer {
    pub async fn new(
        url: String,
        solana_timeout: Duration,
        options: SignerOptions,
        exit_flag: ExitFlag,
    ) -> Result<Self> {
        let fee_payer = read_keypair(&options.keypair)?;
        let keypairs = generate_key_pairs_incl_base(&fee_payer, options.accounts);

        // Create PoW RPC client.
        let pow = PoWClient::new(options.pow_url, options.pow_timeout, exit_flag.clone());

        // Create Solana RPC clients.
        let simulation_url = options.solana_simulation_url.unwrap_or(url.clone());
        let sending_url = options.solana_sending_url.unwrap_or(url.clone());
        let solana = SolanaRpc::new(url, solana_timeout);
        let solana_sim = SolanaRpc::new(simulation_url, solana_timeout);
        let solana_sending = SolanaRpc::new(sending_url, solana_timeout);

        Ok(Self {
            priority_fee: options.priority_fee,
            max_transaction_age: options.max_transaction_age,

            fee_payer: fee_payer.pubkey(),
            keypairs,
            pow,
            solana,
            solana_sim,
            solana_sending,
        })
    }

    pub async fn run(&self) -> Result<()> {
        let (common_sender, common) =
            watch::channel(Arc::new(OnChainCommon::read(&self.solana).await?));

        // Split the list of all addresses into fixed-size batches.
        let mini_batches = {
            let mut mini_batches = Vec::new();
            let pubkeys = Vec::from_iter(self.keypairs.keys().copied());
            for batch_start in (0..pubkeys.len()).step_by(MINI_BATCH_LEN) {
                let mini_batch = pubkeys
                    [batch_start..(batch_start + MINI_BATCH_LEN).min(pubkeys.len())]
                    .to_vec();
                mini_batches.push(mini_batch);
            }
            mini_batches
        };
        info!("Generated mini batches: {}", mini_batches.len());

        // Run the state machine for each batch.
        let mini_batches =
            mini_batches
                .into_iter()
                .enumerate()
                .map(|(mini_batch_idx, mini_batch)| {
                    let common = common.clone();
                    async move {
                        self.handle_mini_batch_loop(common, mini_batch_idx, mini_batch)
                            .await
                    }
                });
        let mini_batches = try_join_all(mini_batches);

        try_join!(self.read_common_loop(common_sender), mini_batches,).map(|_| ())
    }

    async fn handle_mini_batch_loop(
        &self,
        mut common: watch::Receiver<Arc<OnChainCommon>>,
        mini_batch_idx: usize,
        mini_batch: Vec<Pubkey>,
    ) -> Result<()> {
        // Generate derived proof addresses.
        let proof_addresses = generate_proof_addresses(&mini_batch);

        let mut prev_state = State::Initial;
        loop {
            let prev_state_id = prev_state.id();
            match self
                .handle_mini_batch_iteration(
                    replace(&mut prev_state, State::Initial),
                    &mut common,
                    mini_batch_idx,
                    &mini_batch,
                    &proof_addresses,
                )
                .await
            {
                Err(error) => {
                    error!(
                        "State change from {} failed for mini batch [{}..{}]: {}",
                        prev_state_id,
                        mini_batch_idx * MINI_BATCH_LEN,
                        (mini_batch_idx + 1) * MINI_BATCH_LEN - 1,
                        error
                    );
                }

                Ok((new_state, reason)) => {
                    if let Some(reason) = reason {
                        error!(
                            "Mini batch [{}..{}] {} -> {}: {}",
                            mini_batch_idx * MINI_BATCH_LEN,
                            (mini_batch_idx + 1) * MINI_BATCH_LEN - 1,
                            prev_state_id,
                            new_state.id(),
                            reason,
                        );
                    }

                    prev_state = new_state;
                }
            };
        }
    }

    async fn handle_mini_batch_iteration(
        &self,
        prev_state: State,
        common: &mut watch::Receiver<Arc<OnChainCommon>>,
        mini_batch_idx: usize,
        mini_batch: &[Pubkey],
        proof_addresses: &[(Pubkey, u8)],
    ) -> Result<(State, Option<String>)> {
        let (proofs, common) = try_join!(
            read_maybe_proof_accounts(&self.solana, proof_addresses),
            Self::recv_common(common),
        )?;

        // Check for missing proof accounts.
        let proofs_missing: Vec<_> = proofs
            .iter()
            .enumerate()
            .filter_map(|(proof_idx, maybe_proof_account)| {
                if maybe_proof_account.is_none() {
                    Some((mini_batch[proof_idx], proof_addresses[proof_idx]))
                } else {
                    None
                }
            })
            .collect();

        let extract_signers = |proofs_missing: &Vec<(Pubkey, (Pubkey, u8))>| {
            proofs_missing
                .iter()
                .map(|(signer, _)| signer)
                .cloned()
                .collect::<Vec<_>>()
        };

        // Check that transaction has not expired.
        let transaction_valid = match prev_state {
            State::Initial => None,
            State::Registering(slot) => Some(slot),
            State::Sending(_, _, _, slot) => Some(slot),
        }
        .map_or(true, |slot| {
            common.is_slot_valid(slot, self.max_transaction_age)
        });

        // Check that difficulty has not changed.
        let difficulty_is_same = match prev_state {
            State::Initial | State::Registering(_) => None,
            State::Sending(difficulty, _, _, _) => Some(difficulty),
        }
        .map_or(true, |difficulty| {
            difficulty == common.ore.treasury.difficulty()
        });

        // Check that previous hashes has not changed.
        let prev_hashes: Vec<_> = proofs
            .into_iter()
            .flatten()
            .map(|proof| proof.hash())
            .collect();
        let prev_hashes_are_same = match prev_state {
            State::Initial | State::Registering(_) => None,
            State::Sending(_, ref prev_prev_hashes, _, _) => Some(prev_prev_hashes),
        }
        .map_or(true, |prev_prev_hashes| prev_prev_hashes == &prev_hashes);

        // State machine.
        #[rustfmt::skip]
        let (new_state, reason) = match (
                    prev_state,
                    transaction_valid,
                    proofs_missing.is_empty(),
                    difficulty_is_same,
                    prev_hashes_are_same) {
            (
                State::Registering(_),                      // Previous state
                false,                                      // Existing transaction is valid
                false,                                      // All proof acounts exist
                _,                                          // Difficulty is the same
                _,                                          // Previous hashes are same
            ) => {
                let signers = extract_signers(&proofs_missing);
                let (new_state, transaction) = self.state_register(&common, proofs_missing).await?;

                self.send(mini_batch_idx, transaction).await?;
                (
                    new_state,
                    Some(format!("proof accounts are still missing for {:?} and previous transaction expired", signers)),
                )
            }

            prev_state @ (
                State::Registering(_),                      // Previous state
                true,                                       // Existing transaction is valid
                false,                                      // All proof acounts exist
                _,                                          // Difficulty is the same
                _,                                          // Previous hashes are same
            ) => {
                // We are registering, transaction not expired, some proofs missing.
                (
                    prev_state.0,
                    None,
                )
            }

            (
                _,                                          // Previous state
                _,                                          // Existing transaction is valid
                false,                                      // All proof acounts exist
                _,                                          // Difficulty is the same
                _,                                          // Previous hashes are same
            ) => {
                let signers = extract_signers(&proofs_missing);
                let (new_state, transaction) = self.state_register(&common, proofs_missing).await?;

                self.send(mini_batch_idx, transaction).await?;
                (
                    new_state,
                    Some(format!("proof accounts are missing for {:?}", signers)),
                )
            }

            (
                State::Registering(_),                      // Previous state
                _,                                          // Existing transaction is valid
                true,                                       // All proof acounts exist
                _,                                          // Difficulty is the same
                _,                                          // Previous hashes are same
            ) => {
                let (new_state, transaction) = self.state_sending(
                    &common,
                    mini_batch,
                    proof_addresses,
                    prev_hashes,
                    None,
                ).await?;

                self.send(mini_batch_idx, transaction).await?;
                (
                    new_state,
                    Some("all proof accounts registered".to_string()),
                )
            }

            (
                State::Initial,                             // Previous state
                _,                                          // Existing transaction is valid
                true,                                       // All proof acounts exist
                _,                                          // Difficulty is the same
                _,                                          // Previous hashes are same
            ) => {
                let (new_state, transaction) = self.state_sending(
                    &common,
                    mini_batch,
                    proof_addresses,
                    prev_hashes,
                    None,
                ).await?;

                self.send(mini_batch_idx, transaction).await?;
                (
                    new_state,
                    Some("all proof accounts exist".to_string()),
                )
            }

            (
                State::Sending(_, _, _, _),                 // Previous state
                _,                                          // Existing transaction is valid
                true,                                       // All proof acounts exist
                false,                                      // Difficulty is the same
                _,                                          // Previous hashes are same
            ) => {
                let (new_state, transaction) = self.state_sending(
                    &common,
                    mini_batch,
                    proof_addresses,
                    prev_hashes,
                    None,
                ).await?;

                self.send(mini_batch_idx, transaction).await?;
                (
                    new_state,
                    Some("difficulty changed".to_string()),
                )
            }

            (
                State::Sending(_, prev_prev_hashes, _, _),  // Previous state
                _,                                          // Existing transaction is valid
                true,                                       // All proof acounts exist
                _,                                          // Difficulty is the same
                false,                                      // Previous hashes are same
            ) => {
                let reason = if prev_prev_hashes.iter().zip(&prev_hashes).all(|(a, b)| a == b) {
                    "transaction landed".to_string()
                } else {
                    // TODO: Do not query next hashes for unchanged entries.
                    // Currently this should be covered by PoW cache.
                    "some previous hashes changed, another instance running?".to_string()
                };

                let (new_state, transaction) = self.state_sending(
                    &common,
                    mini_batch,
                    proof_addresses,
                    prev_hashes,
                    None,
                ).await?;

                self.send(mini_batch_idx, transaction).await?;
                (
                    new_state,
                    Some(reason),
                )
            }

            prev_state @ (
                State::Sending(_, _, _, _),                 // Previous state
                true,                                       // Existing transaction is valid
                true,                                       // All proof acounts exist
                true,                                       // Difficulty is the same
                true,                                       // Previous hashes are same
            ) => {
                // We are sending, transaction not expired, proof accounts exist.
                // Difficulty is the same.
                (
                    prev_state.0,
                    None,
                )
            }

            (
                State::Sending(_, _, known_hashes, _),      // Previous state
                false,                                      // Existing transaction is valid
                true,                                       // All proof acounts exist
                true,                                       // Difficulty is the same
                true,                                       // Previous hashes are same
            ) => {
                let (new_state, transaction) = self.state_sending(
                    &common,
                    mini_batch,
                    proof_addresses,
                    prev_hashes,
                    Some(known_hashes),
                ).await?;

                self.send(mini_batch_idx, transaction).await?;
                (
                    new_state,
                    Some("transaction expired".to_string()),
                )
            }
        };

        Ok((new_state, reason))
    }

    async fn recv_common(
        common: &mut watch::Receiver<Arc<OnChainCommon>>,
    ) -> Result<Arc<OnChainCommon>> {
        common
            .changed()
            .await
            .map_err(|_| anyhow!("Unable to receive updated common value: sender closed"))?;
        Ok(common.borrow().clone())
    }

    async fn state_register(
        &self,
        common: &OnChainCommon,
        proofs_missing: Vec<(Pubkey, (Pubkey, u8))>,
    ) -> Result<(State, Option<Transaction>)> {
        let (signers, proofs_missing): (Vec<_>, Vec<_>) = proofs_missing.into_iter().unzip();
        let transaction = self
            .build_register_transaction(&signers, &proofs_missing, common)
            .await?;

        Ok((State::Registering(common.slot), Some(transaction)))
    }

    async fn state_sending(
        &self,
        common: &OnChainCommon,
        mini_batch: &[Pubkey],
        proof_addresses: &[(Pubkey, u8)],
        prev_hashes: Vec<keccak::Hash>,
        known_hashes: Option<Vec<NextHash>>,
    ) -> Result<(State, Option<Transaction>)> {
        let next_hashes = if let Some(known_hashes) = known_hashes {
            known_hashes
        } else {
            // Need to mine.
            // TODO: Extract into separate state.
            let requests: Vec<_> = prev_hashes
                .iter()
                .zip(mini_batch)
                .map(|(prev_hash, signer)| PoWRequest {
                    difficulty: common.ore.treasury.difficulty(),
                    prev_hash: *prev_hash,
                    signer: *signer,
                })
                .collect();
            self.pow.find_hashes(&requests).await?
        };

        let bus = common.ore.choose_bus(mini_batch.len() as u64)?;

        let transaction = self
            .build_mine_transaction(mini_batch, proof_addresses, &next_hashes, bus, common)
            .await?;

        Ok((
            State::Sending(
                common.ore.treasury.difficulty(),
                prev_hashes,
                next_hashes,
                common.slot,
            ),
            Some(transaction),
        ))
    }

    async fn build_mine_transaction(
        &self,
        signers: &[Pubkey],
        proof_addresses: &[(Pubkey, u8)],
        next_hashes: &[NextHash],
        bus: usize,
        common: &OnChainCommon,
    ) -> Result<Transaction> {
        let instructions: Vec<_> = signers
            .iter()
            .zip(proof_addresses)
            .zip(next_hashes)
            .map(|((signer, proof_address), next_hash)| {
                self.build_mine_instruction(signer, bus, proof_address, next_hash)
            })
            .collect();

        self.simulate_and_add_budget(&instructions, common)
            .await
            .map(|mut transaction| {
                self.sign(&mut transaction, signers, common);
                transaction
            })
    }

    fn build_mine_instruction(
        &self,
        signer: &Pubkey,
        bus: usize,
        proof_address: &(Pubkey, u8),
        next_hash: &NextHash,
    ) -> Instruction {
        Instruction {
            program_id: ore::id(),
            accounts: vec![
                AccountMeta::new(*signer, true),
                AccountMeta::new(BUS_ADDRESSES[bus], false),
                AccountMeta::new(proof_address.0, false),
                AccountMeta::new_readonly(TREASURY_ADDRESS, false),
                AccountMeta::new_readonly(sysvar::slot_hashes::id(), false),
            ],
            data: [
                OreInstruction::Mine.get_vec(),
                bytemuck::bytes_of(&MineArgs {
                    hash: next_hash.next_hash.to_bytes(),
                    nonce: next_hash.nonce.to_le_bytes(),
                })
                .to_vec(),
            ]
            .concat(),
        }
    }

    async fn build_register_transaction(
        &self,
        signers: &[Pubkey],
        proof_addresses: &[(Pubkey, u8)],
        common: &OnChainCommon,
    ) -> Result<Transaction> {
        let mut instructions = Vec::new();

        for (signer, proof_address) in signers.iter().zip(proof_addresses) {
            if signer != &self.fee_payer {
                instructions.push(system_instruction::transfer(
                    &self.fee_payer,
                    signer,
                    PROOF_ACCOUNT_RENT,
                ));
            }
            instructions.push(self.build_register_instruction(signer, proof_address));
        }

        self.simulate_and_add_budget(&instructions, common)
            .await
            .map(|mut transaction| {
                self.sign(&mut transaction, signers, common);
                transaction
            })
    }

    fn build_register_instruction(
        &self,
        signer: &Pubkey,
        proof_address: &(Pubkey, u8),
    ) -> Instruction {
        Instruction {
            program_id: ore::id(),
            accounts: vec![
                AccountMeta::new(*signer, true),
                AccountMeta::new(proof_address.0, false),
                AccountMeta::new_readonly(solana_program::system_program::id(), false),
            ],
            data: [
                OreInstruction::Register.get_vec(),
                bytemuck::bytes_of(&RegisterArgs {
                    bump: proof_address.1,
                })
                .to_vec(),
            ]
            .concat(),
        }
    }

    fn sign(&self, transaction: &mut Transaction, mini_batch: &[Pubkey], common: &OnChainCommon) {
        assert!(
            !transaction.is_signed(),
            "Logic error: transaction is already signed"
        );
        trace!("Signing transaction {:#?}", transaction);

        let mut already_have_fee_payer = false;
        for signer in mini_batch {
            transaction.partial_sign(&[&self.keypairs[signer]], common.hash);

            if *signer == self.fee_payer {
                already_have_fee_payer = true;
            }
        }

        if !already_have_fee_payer {
            transaction.partial_sign(&[&self.keypairs[&self.fee_payer]], common.hash);
        }
    }

    async fn send(&self, _mini_batch_idx: usize, transaction: Option<Transaction>) -> Result<()> {
        // TODO: Separate "sender" service.
        if let Some(transaction) = transaction {
            assert!(
                transaction.is_signed(),
                "Logic error: transaction is not fully signed"
            );

            self.solana_sending.send_fast(&transaction).await
        } else {
            Ok(())
        }
    }

    async fn simulate_and_add_budget(
        &self,
        instructions: &[Instruction],
        common: &OnChainCommon,
    ) -> Result<Transaction> {
        let transaction = Transaction::new_with_payer(instructions, Some(&self.fee_payer));
        self.solana_sim
            .simulate(&transaction, common.slot, false)
            .await
            .map(|units_consumed| {
                let mut new_instructions = vec![
                    ComputeBudgetInstruction::set_compute_unit_limit(
                        (units_consumed as f64 * COMPUTE_BUDGET_MULTIPLIER) as u32,
                    ),
                    ComputeBudgetInstruction::set_compute_unit_price(self.priority_fee),
                ];
                new_instructions.extend(instructions.iter().cloned());
                Transaction::new_with_payer(&new_instructions, Some(&self.fee_payer))
            })
    }

    async fn read_common_loop(&self, sender: watch::Sender<Arc<OnChainCommon>>) -> Result<()> {
        loop {
            let start = Instant::now();
            debug!("Tick");

            if let Err(error) = self.read_common_iteration(&sender).await {
                error!("On-chain common data read failed: {}", error);
            }

            let elapsed = start.elapsed();
            if elapsed < COMMON_ITER_DURATION {
                sleep(COMMON_ITER_DURATION - elapsed).await;
            }
        }
    }

    async fn read_common_iteration(
        &self,
        sender: &watch::Sender<Arc<OnChainCommon>>,
    ) -> Result<()> {
        let common = OnChainCommon::read(&self.solana).await?;

        if common.is_epoch_ended() {
            // TODO: Do not send transactions when epoch ended.
            // TODO: Implement reset.
            warn!("ORE epoch ended, needs reset");
        }

        if **sender.borrow() != common {
            sender
                .send(Arc::new(common))
                .map_err(|_| anyhow!("Unable to send common on-chain data: receiver closed"))?;
        }

        Ok(())
    }
}

enum State {
    Initial,
    Registering(Slot),
    Sending(keccak::Hash, Vec<keccak::Hash>, Vec<NextHash>, Slot),
}

impl State {
    const fn id(&self) -> StateId {
        match self {
            Self::Initial => StateId::Initial,
            Self::Registering(_) => StateId::Registering,
            Self::Sending(_, _, _, _) => StateId::Sending,
        }
    }
}

enum StateId {
    Initial,
    Registering,
    Sending,
}

impl StateId {
    const fn name(&self) -> &'static str {
        match self {
            Self::Initial => "Initial",
            Self::Registering => "Registering",
            Self::Sending => "sending",
        }
    }
}

impl Display for StateId {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name())
    }
}

#[derive(Eq, PartialEq)]
struct OnChainCommon {
    // Solana.
    hash: Hash,
    slot: Slot,
    clock: Clock,

    // ORE
    ore: OreCommon,
}

impl OnChainCommon {
    async fn read(solana: &SolanaRpc) -> Result<Self> {
        debug!("Reading base state...");

        let (hash, slot) = solana.get_blockhash_and_slot().await?;
        debug!("Latest blockhash: {}, slot {}", hash, slot);

        let clock = solana
            .read_account::<Clock>(&sysvar::clock::ID)
            .await?
            .ok_or_else(|| anyhow!("Clock is missing"))?;
        debug!("Clock: {:?}", clock);

        let ore = OreCommon::read(solana).await?;

        Ok(Self {
            hash,
            slot,
            clock,
            ore,
        })
    }

    const fn is_epoch_ended(&self) -> bool {
        self.clock.unix_timestamp
            >= self
                .ore
                .treasury
                .last_reset_at
                .saturating_add(EPOCH_DURATION)
    }

    const fn is_slot_valid(&self, transaction_slot: Slot, max_transaction_age: u64) -> bool {
        self.slot <= transaction_slot + max_transaction_age
    }
}
