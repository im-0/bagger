use std::{fs::canonicalize, net::SocketAddr, num::NonZeroUsize, time::Duration};

use anyhow::{anyhow, Context, Result};
use clap::{command, Parser, Subcommand};
use solana_sdk::clock::Slot;

use crate::pow::types::PoWRequest;

#[derive(Parser)]
#[command(about, version)]
pub struct Cli {
    /// Do not print timestamp with each line of log.
    #[arg(short = 'T', long)]
    pub log_without_timestamps: bool,

    /// URL for Solana's JSON RPC or moniker (or their first letter): mainnet-beta, testnet, devnet, localhost.
    #[arg(short = 'u', long, value_name = "URL", default_value = "localhost", value_parser = parse_solana_rpc_url)]
    pub url: String,

    /// Solana RPC timeout.
    #[arg(short = 't', long, value_name = "SECONDS", default_value = "4", value_parser = parse_duration)]
    pub solana_timeout: Duration,

    /// Command.
    #[command(subcommand)]
    pub subcommand: SubCommand,
}

#[derive(Subcommand)]
pub enum SubCommand {
    /// Show ORE treasury and bus info.
    #[command()]
    Info,

    /// Proof-of-Work miner on CPU.
    #[command(name = "pow-cpu")]
    PoWCPU(PoWCPUOptions),

    /// Proof-of-Work miner based on hashcat.
    #[command(name = "pow-hashcat")]
    PoWHashcat(PoWHashcatOptions),

    /// Query next hash from Proof-of-Work miner.
    #[command(name = "pow")]
    PoW(PoWOptions),

    /// Generate and sign transactions.
    #[command()]
    Signer(SignerOptions),

    // Show unclaimed ORE rewards.
    #[command()]
    Rewards(RewardsOptions),

    /// Claim ORE.
    #[command()]
    Claim(ClaimOptions),
}

#[derive(Parser)]
pub struct PoWCPUOptions {
    /// IP address and port to listen on.
    #[arg(
        short = 'a',
        long,
        value_name = "IP:PORT",
        default_value = "127.0.0.1:26368"
    )]
    pub address: SocketAddr,

    /// Number of CPU threads to use for Proof-of-Work.
    #[arg(short = 't', long, value_name = "NUMBER", default_value = get_num_cpus_str())]
    pub threads: NonZeroUsize,
}

#[derive(Clone, Parser)]
pub struct PoWHashcatOptions {
    /// IP address and port to listen on.
    #[arg(
        short = 'a',
        long,
        value_name = "IP:PORT",
        default_value = "127.0.0.1:26368"
    )]
    pub address: SocketAddr,

    /// Hashcat command. Full path may be specified here.
    #[arg(short = 'c', long, default_value = "hashcat")]
    pub command: String,

    /// Hashcat module to use.
    #[arg(short = 'm', long, value_name = "NUMBER", default_value = "54100")]
    pub module: String,

    /// Persistend data directory for hashcat.
    #[arg(short = 'd', long, value_name = "PATH", default_value = ".", value_parser = canonicalize_path)]
    pub data_directory: String,

    /// Additional command line arguments for `hashcat`.
    #[arg()]
    pub hashcat_args: Vec<String>,
}

#[derive(Parser)]
pub struct PoWOptions {
    /// Proof-of-Work RPC server.
    #[arg(
        short = 'u',
        long,
        value_name = "URL",
        default_value = "ws://127.0.0.1:26368"
    )]
    pub url: String,

    /// Proof-of-Work RPC timeout.
    #[arg(short = 't', long, value_name = "SECONDS", default_value = "4", value_parser = parse_duration)]
    pub timeout: Duration,

    /// Proof-of-Work request in format `$DIFFICULTY:$PREVIOUS_HASH:$PUBKEY`, all base58 encoded.
    #[arg(required = true)]
    pub request: PoWRequest,
}

#[derive(Parser)]
pub struct SignerOptions {
    /// Proof-of-Work RPC server.
    #[arg(
        short = 'u',
        long,
        value_name = "URL",
        default_value = "ws://127.0.0.1:26368"
    )]
    pub pow_url: String,

    /// Proof-of-Work RPC timeout.
    #[arg(short = 't', long, value_name = "SECONDS", default_value = "4", value_parser = parse_duration)]
    pub pow_timeout: Duration,

    /// Simulation RPC: URL for Solana's JSON RPC or moniker (or their first letter): mainnet-beta, testnet, devnet, localhost.
    /// Regular url (`--url`) is the default.
    #[arg(short = 's', long, value_name = "URL", value_parser = parse_solana_rpc_url)]
    pub solana_simulation_url: Option<String>,

    /// Sending RPC: URL for Solana's JSON RPC or moniker (or their first letter): mainnet-beta, testnet, devnet, localhost.
    /// Regular url (`--url`) is the default.
    #[arg(short = 'S', long, value_name = "URL", value_parser = parse_solana_rpc_url)]
    pub solana_sending_url: Option<String>,

    /// Number of accounts to use for mining.
    #[arg(short = 'a', long, value_name = "NUMBER", default_value = "1")]
    pub accounts: NonZeroUsize,

    /// Number of microlamports to pay as priority fee per Solana's compute unit (CU).
    #[arg(short = 'p', long, value_name = "MICROLAMPORTS", default_value = "0")]
    pub priority_fee: u64,

    /// Maximum transaction age.
    #[arg(short = 'M', long, value_name = "SLOTS", default_value = "100")]
    pub max_transaction_age: Slot,

    /// Path to the fee-payer keypair.
    pub keypair: String,
}

#[derive(Parser)]
pub struct RewardsOptions {
    /// Number of accounts to use for mining.
    #[arg(short = 'a', long, value_name = "NUMBER", default_value = "1")]
    pub accounts: NonZeroUsize,

    /// Path to the fee-payer keypair.
    pub keypair: String,
}

#[derive(Parser)]
pub struct ClaimOptions {
    /// Number of accounts to use for mining.
    #[arg(short = 'a', long, value_name = "NUMBER", default_value = "1")]
    pub accounts: NonZeroUsize,

    /// Path to the fee-payer keypair.
    pub keypair: String,

    /// Beneficiary address.
    pub beneficiary: String,
}

fn parse_solana_rpc_url(url_or_moniker: &str) -> Result<String> {
    Ok(match url_or_moniker {
        "m" | "mainnet-beta" => "https://api.mainnet-beta.solana.com",
        "t" | "testnet" => "https://api.testnet.solana.com",
        "d" | "devnet" => "https://api.devnet.solana.com",
        "l" | "localhost" => "http://localhost:8899",
        url => url,
    }
    .to_string())
}

fn parse_duration(duration: &str) -> Result<Duration> {
    Ok(Duration::from_secs_f64(
        duration
            .parse::<f64>()
            .with_context(|| format!("Unable to parse duration \"{}\"", duration))?,
    ))
}

fn get_num_cpus_str() -> &'static str {
    match num_cpus::get() {
        1 => "1",
        2 => "2",
        3 => "3",
        4 => "4",
        5 => "5",
        6 => "6",
        7 => "7",
        8 => "8",
        9 => "9",
        10 => "10",
        11 => "11",
        12 => "12",
        13 => "13",
        14 => "14",
        15 => "15",
        16 => "16",
        17 => "17",
        18 => "18",
        19 => "19",
        20 => "20",
        21 => "21",
        22 => "22",
        23 => "23",
        24 => "24",
        25 => "25",
        26 => "26",
        27 => "27",
        28 => "28",
        29 => "29",
        30 => "30",
        31 => "31",
        32 => "32",
        33 => "33",
        34 => "34",
        35 => "35",
        36 => "36",
        37 => "37",
        38 => "38",
        39 => "39",
        40 => "40",
        41 => "41",
        42 => "42",
        43 => "43",
        44 => "44",
        45 => "45",
        46 => "46",
        47 => "47",
        48 => "48",
        49 => "49",
        50 => "50",
        51 => "51",
        52 => "52",
        53 => "53",
        54 => "54",
        55 => "55",
        56 => "56",
        57 => "57",
        58 => "58",
        59 => "59",
        60 => "60",
        61 => "61",
        62 => "62",
        63 => "63",
        64 => "64",
        65 => "65",
        66 => "66",
        67 => "67",
        68 => "68",
        69 => "69",
        70 => "70",
        71 => "71",
        72 => "72",
        73 => "73",
        74 => "74",
        75 => "75",
        76 => "76",
        77 => "77",
        78 => "78",
        79 => "79",
        80 => "80",
        81 => "81",
        82 => "82",
        83 => "83",
        84 => "84",
        85 => "85",
        86 => "86",
        87 => "87",
        88 => "88",
        89 => "89",
        90 => "90",
        91 => "91",
        92 => "92",
        93 => "93",
        94 => "94",
        95 => "95",
        96 => "96",
        97 => "97",
        98 => "98",
        99 => "99",
        100 => "100",
        101 => "101",
        102 => "102",
        103 => "103",
        104 => "104",
        105 => "105",
        106 => "106",
        107 => "107",
        108 => "108",
        109 => "109",
        110 => "110",
        111 => "111",
        112 => "112",
        113 => "113",
        114 => "114",
        115 => "115",
        116 => "116",
        117 => "117",
        118 => "118",
        119 => "119",
        120 => "120",
        121 => "121",
        122 => "122",
        123 => "123",
        124 => "124",
        125 => "125",
        126 => "126",
        127 => "127",
        _ => "128",
    }
}

fn canonicalize_path(path: &str) -> Result<String> {
    canonicalize(path)
        .with_context(|| format!("Unable to canonicalize path {}", path))
        .and_then(|path| {
            path.as_os_str()
                .to_str()
                .ok_or_else(|| anyhow!("Unable to convert canonicalized path {:?} to string", path))
                .map(|path| path.to_string())
        })
}
