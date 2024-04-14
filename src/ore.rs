use std::fmt::{self, Display, Formatter};

use anyhow::{anyhow, ensure, Context, Result};
use bytemuck::{Pod, Zeroable};
use futures::future::try_join_all;
use jsonrpsee::core::async_trait;
use solana_program::pubkey;
use solana_sdk::{declare_id, keccak, pubkey::Pubkey};
use tracing::debug;

use crate::solana::SolanaRpc;

// See https://github.com/HardhatChad/ore/

const PROOF: &[u8] = b"proof";

declare_id!("mineRHF5r6S7HyD9SppBfVMXMavDkJsxwGesEvxZr2A");

pub const EPOCH_DURATION: i64 = 60;

pub const TREASURY_ADDRESS: Pubkey = pubkey!("FTap9fv2GPpWGqrLj3o4c9nHH7p36ih7NbSWHnrkQYqa");
pub const MINT_ADDRESS: Pubkey = pubkey!("oreoN2tQbHXVaZsr3pf66A48miqcBXCDJozganhEJgz");

pub const TOKEN_DECIMALS: u8 = 9;
pub const ONE_ORE: u64 = 10u64.pow(TOKEN_DECIMALS as u32);

pub const BUS_ADDRESSES: [Pubkey; 8] = [
    pubkey!("9ShaCzHhQNvH8PLfGyrJbB8MeKHrDnuPMLnUDLJ2yMvz"),
    pubkey!("4Cq8685h9GwsaD5ppPsrtfcsk3fum8f9UP4SPpKSbj2B"),
    pubkey!("8L1vdGdvU3cPj9tsjJrKVUoBeXYvAzJYhExjTYHZT7h7"),
    pubkey!("JBdVURCrUiHp4kr7srYtXbB7B4CwurUt1Bfxrxw6EoRY"),
    pubkey!("DkmVBWJ4CLKb3pPHoSwYC2wRZXKKXLD2Ued5cGNpkWmr"),
    pubkey!("9uLpj2ZCMqN6Yo1vV6yTkP6dDiTTXmeM5K3915q5CHyh"),
    pubkey!("EpcfjBs8eQ4unSMdowxyTE8K3vVJ3XUnEr5BEWvSX7RB"),
    pubkey!("Ay5N9vKS2Tyo2M9u9TFt59N1XbxdW93C7UrFZW3h8sMC"),
];

pub fn conv_ore(num: u64) -> f64 {
    num as f64 / ONE_ORE as f64
}

pub fn generate_proof_address(pubkey: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[PROOF, pubkey.as_ref()], &id())
}

pub fn generate_proof_addresses<'a, I>(addresses: I) -> Vec<(Pubkey, u8)>
where
    I: IntoIterator<Item = &'a Pubkey>,
{
    addresses
        .into_iter()
        .map(|address| {
            let proof_address = generate_proof_address(address);
            debug!(
                "ORE proof account address for {}: ({}, {})",
                address, proof_address.0, proof_address.1,
            );
            proof_address
        })
        .collect()
}

#[repr(u8)]
pub enum AccountDiscriminator {
    Bus = 100,
    Proof = 101,
    Treasury = 102,
}

impl Display for AccountDiscriminator {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bus => write!(f, "bus"),
            Self::Proof => write!(f, "proof"),
            Self::Treasury => write!(f, "treasury"),
        }
    }
}

pub trait Discriminator {
    fn discriminator() -> AccountDiscriminator;
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq, Pod, Zeroable)]
pub struct Treasury {
    pub admin: Pubkey,
    pub bump: u64,
    pub difficulty: [u8; keccak::HASH_BYTES],
    pub last_reset_at: i64,
    pub reward_rate: u64,
    pub total_claimed_rewards: u64,
}

impl Treasury {
    pub const fn difficulty(&self) -> keccak::Hash {
        keccak::Hash::new_from_array(self.difficulty)
    }
}

impl Discriminator for Treasury {
    fn discriminator() -> AccountDiscriminator {
        AccountDiscriminator::Treasury
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq, Pod, Zeroable)]
pub struct Bus {
    pub id: u64,
    pub rewards: u64,
}

impl Discriminator for Bus {
    fn discriminator() -> AccountDiscriminator {
        AccountDiscriminator::Bus
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub struct Proof {
    pub authority: Pubkey,
    pub claimable_rewards: u64,
    pub hash: [u8; keccak::HASH_BYTES],
    pub total_hashes: u64,
    pub total_rewards: u64,
}

impl Proof {
    pub const fn hash(&self) -> keccak::Hash {
        keccak::Hash::new_from_array(self.hash)
    }
}

impl Discriminator for Proof {
    fn discriminator() -> AccountDiscriminator {
        AccountDiscriminator::Proof
    }
}

#[async_trait]
pub trait AccountRead {
    async fn read_ore_account<Account>(&self, address: &Pubkey) -> Result<Option<Account>>
    where
        Account: Pod + Discriminator + Clone;
}

#[async_trait]
impl AccountRead for SolanaRpc {
    async fn read_ore_account<Account>(&self, address: &Pubkey) -> Result<Option<Account>>
    where
        Account: Pod + Discriminator + Clone,
    {
        let maybe_data = self
            .read_account_data(address)
            .await
            .with_context(|| format!("Unable to read data for ORE account {}", address))?;
        match maybe_data {
            Some(data) => {
                anyhow::ensure!(
                    (Account::discriminator() as u8) == data[0],
                    "Invalid discriminator for ORE {} account: {}",
                    Account::discriminator(),
                    data[0],
                );

                bytemuck::try_from_bytes::<Account>(&data[8..])
                    .map_err(|error| {
                        anyhow::anyhow!(
                            "Unable to deserialize ORE {} account: {}",
                            Account::discriminator(),
                            error,
                        )
                    })
                    .map(|account| Some(*account))
            }

            None => Ok(None),
        }
    }
}

#[allow(dead_code)]
#[derive(Clone, Copy)]
pub enum OreInstruction {
    Reset = 0,
    Register = 1,
    Mine = 2,
    Claim = 3,
    Initialize = 100,
    UpdateAdmin = 101,
    UpdateDifficulty = 102,
}

impl OreInstruction {
    pub fn get_vec(&self) -> Vec<u8> {
        vec![*self as u8]
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub struct RegisterArgs {
    pub bump: u8,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub struct MineArgs {
    pub hash: [u8; keccak::HASH_BYTES],
    pub nonce: [u8; 8],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub struct ClaimArgs {
    pub amount: [u8; 8],
}

#[derive(Eq, PartialEq)]
pub struct OreCommon {
    pub treasury: Treasury,
    pub buses: Vec<Bus>,
}

impl OreCommon {
    pub async fn read(solana: &SolanaRpc) -> Result<Self> {
        debug!("Reading ORE common state...");

        let treasury = solana
            .read_ore_account::<Treasury>(&TREASURY_ADDRESS)
            .await?
            .ok_or_else(|| anyhow!("ORE treasury is missing"))?;
        debug!("ORE treasury: {:?}", treasury);

        let buses: Vec<_> = BUS_ADDRESSES
            .iter()
            .map(|address| async move {
                solana
                    .read_ore_account::<Bus>(address)
                    .await
                    .with_context(|| format!("ORE bus address: {}", address))
                    .and_then(|maybe_bus| {
                        maybe_bus.ok_or_else(|| anyhow!("ORE bus {} is missing", address))
                    })
                    .map(|bus| {
                        debug!("ORE bus {}: {:?}", address, bus);
                        bus
                    })
            })
            .collect();
        let buses = try_join_all(buses.into_iter())
            .await
            .context("Unable to read ORE bus")?;

        Ok(Self { treasury, buses })
    }

    pub fn choose_bus(&self, num_signers: u64) -> Result<usize> {
        let buses: Vec<_> = self
            .buses
            .iter()
            .enumerate()
            .filter(|(_, bus)| bus.rewards >= self.treasury.reward_rate * num_signers)
            .collect();

        ensure!(
            !buses.is_empty(),
            "No valid ORE bus found, all rewards drained"
        );

        let weights: Vec<_> = buses
            .iter()
            .map(|(_, bus)| {
                usize::try_from(bus.rewards)
                    .expect("Logic error: reward is too big and we are running on 32 bit system")
            })
            .collect();

        let bus_idx = random_pick::pick_from_slice(&buses, &weights)
            .expect("Logic error: we do not have zero weights")
            .0;

        Ok(bus_idx)
    }
}

pub async fn read_maybe_proof_accounts<'a, I>(
    solana: &SolanaRpc,
    proof_addresses: I,
) -> Result<Vec<Option<Proof>>>
where
    I: IntoIterator<Item = &'a (Pubkey, u8)>,
{
    let proofs: Vec<_> = proof_addresses
        .into_iter()
        .map(|address| async { solana.read_ore_account::<Proof>(&address.0).await })
        .collect();
    try_join_all(proofs).await
}

pub async fn read_proof_accounts<'a, I>(
    solana: &SolanaRpc,
    proof_addresses: I,
) -> Result<Vec<Proof>>
where
    I: IntoIterator<Item = &'a (Pubkey, u8)>,
{
    let proofs = read_maybe_proof_accounts(solana, proof_addresses).await?;

    ensure!(
        proofs.iter().all(|maybe_proof| maybe_proof.is_some()),
        "Some proof accounts are missing",
    );

    Ok(proofs.into_iter().flatten().collect())
}
