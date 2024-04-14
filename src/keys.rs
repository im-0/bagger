use std::{collections::BTreeMap, num::NonZeroUsize, time::Instant};

use anyhow::{anyhow, Context, Result};
use rand_chacha::rand_core::SeedableRng;
use rand_chacha::ChaCha20Rng;
use solana_sdk::{
    pubkey::Pubkey,
    signature::{read_keypair_file, Keypair, Signer},
};
use tracing::{debug, info};

pub fn read_keypair(path: &str) -> Result<Keypair> {
    let keypair = read_keypair_file(path)
        .map_err(|error| anyhow!("{}", error))
        .with_context(|| format!("Unable to read key pair from {}", path))?;
    info!("Read key pair for {} from {}", keypair.pubkey(), path,);
    Ok(keypair)
}

pub fn generate_key_pairs_incl_base(
    base: &Keypair,
    num: NonZeroUsize,
) -> BTreeMap<Pubkey, Keypair> {
    let mut keypairs = generate_key_pairs(base, num.get() - 1);
    assert!(
        keypairs
            .insert(
                base.pubkey(),
                Keypair::from_bytes(&base.to_bytes())
                    .expect("Logic error: unable to copy a private key"),
            )
            .is_none(),
        "Logic error: map of key pairs already contains abase key pair",
    );

    keypairs
}

pub fn generate_key_pairs(base: &Keypair, num: usize) -> BTreeMap<Pubkey, Keypair> {
    let mut keypairs = BTreeMap::new();
    if num == 0 {
        return keypairs;
    }

    info!("Generating key pairs...");
    let start = Instant::now();

    let mut rng = ChaCha20Rng::from_seed(base.secret().to_bytes());
    while keypairs.len() != num {
        let keypair = Keypair::generate(&mut rng);
        debug!("Generated key pair for {}", keypair.pubkey());
        let _ = keypairs.insert(keypair.pubkey(), keypair);
    }

    info!(
        "{} key pairs generated in {}s",
        keypairs.len(),
        start.elapsed().as_secs_f64()
    );

    keypairs
}
