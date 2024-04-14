use std::{
    fmt::{self, Display},
    str::FromStr,
    time::Duration,
};

use anyhow::{anyhow, Context, Error};
use serde_derive::{Deserialize, Serialize};
use solana_sdk::{keccak, pubkey::Pubkey};

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct PoWRequest {
    pub difficulty: keccak::Hash,
    pub prev_hash: keccak::Hash,
    pub signer: Pubkey,
}

impl Display for PoWRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}:{}", self.difficulty, self.prev_hash, self.signer)
    }
}

impl FromStr for PoWRequest {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut parts = s.split(':');

        let difficulty = parts
            .next()
            .expect("Logic error: can not be less than one part");
        let prev_hash = parts
            .next()
            .ok_or_else(|| anyhow!("No previous hash (#2) in request: {}", s))?;
        let signer = parts
            .next()
            .ok_or_else(|| anyhow!("No signer (#3) in request: {}", s))?;

        Ok(Self {
            difficulty: difficulty
                .parse()
                .with_context(|| format!("Invalid difficulty (#1) in request: {}", difficulty))?,

            prev_hash: prev_hash
                .parse()
                .with_context(|| format!("Invalid previous hash (#2) in request: {}", prev_hash))?,

            signer: signer
                .parse()
                .with_context(|| format!("Invalid signer (#3) in request: {}", signer))?,
        })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct NextHash {
    pub next_hash: keccak::Hash,
    pub nonce: u64,
}

impl Display for NextHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.next_hash, self.nonce)
    }
}

pub type PoWResponse = (PoWRequest, NextHash, Duration);
