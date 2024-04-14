use std::time::Duration;

use anyhow::Result;

use crate::{ore::OreCommon, solana::SolanaRpc};

pub async fn get_ore_info(url: String, timeout: Duration) -> Result<OreCommon> {
    let solana = SolanaRpc::new(url, timeout);

    OreCommon::read(&solana).await
}
