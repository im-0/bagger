#![deny(unsafe_code)]
#![warn(unused_results)]
#![warn(private_interfaces)]
#![warn(private_bounds)]
#![warn(clippy::empty_line_after_outer_attr)]
#![warn(clippy::manual_filter_map)]
#![warn(clippy::if_not_else)]
#![warn(clippy::mut_mut)]
#![warn(clippy::non_ascii_literal)]
#![warn(clippy::map_unwrap_or)]
#![warn(clippy::use_self)]
#![warn(clippy::used_underscore_binding)]
#![warn(clippy::print_stdout)]
#![warn(clippy::print_stderr)]
#![warn(clippy::map_flatten)]
#![warn(clippy::wildcard_dependencies)]
#![warn(clippy::cargo_common_metadata)]
#![warn(clippy::wildcard_enum_match_arm)]
#![warn(clippy::missing_const_for_fn)]
#![warn(clippy::dbg_macro)]
#![warn(clippy::path_buf_push_overwrite)]
#![warn(clippy::manual_find_map)]
#![warn(clippy::filter_map_next)]
#![warn(clippy::checked_conversions)]
#![warn(clippy::type_repetition_in_bounds)]

use std::process::exit;

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use clap::Parser;

mod claim;
mod cli;
mod info;
mod keys;
mod ore;
mod pow;
mod rewards;
mod signals;
mod signer;
mod solana;
mod trace;

use info::get_ore_info;
use pow::client::PoWClient;
use rewards::get_ore_rewards;
use signals::{handle_signals, ExitFlag};
use signer::Signer;
use tokio::{select, try_join};
use tracing::error;

use crate::cli::{Cli, SubCommand};
use crate::ore::{conv_ore, BUS_ADDRESSES};
use crate::pow::server::PoWServer;
use crate::trace::{init_tracing, TracingReloadFn};

fn main() {
    let args = Cli::parse();

    let tracing_reload_fn = match init_tracing(args.log_without_timestamps)
        .context("Unable to initialize logging/tracing")
    {
        Ok(tracing_reload_fn) => tracing_reload_fn,
        Err(error) => panic!("{:?}", error),
    };

    let return_code = match start_async(args, tracing_reload_fn) {
        Ok(()) => 0,

        Err(error) => {
            error!("Fatal error: {:?}", error);
            2
        }
    };

    exit(return_code)
}

fn start_async(args: Cli, tracing_reload_fn: TracingReloadFn) -> Result<()> {
    let num_async_threads = match args.subcommand {
        SubCommand::Info | SubCommand::PoW(_) | SubCommand::Rewards(_) | SubCommand::Claim(_) => 1,
        SubCommand::PoWCPU(_) | SubCommand::PoWHashcat(_) | SubCommand::Signer(_) => {
            num_cpus::get()
        }
    };
    let num_blocking_threads = match &args.subcommand {
        SubCommand::Info
        | SubCommand::PoW(_)
        | SubCommand::PoWHashcat(_)
        | SubCommand::Signer(_)
        | SubCommand::Rewards(_)
        | SubCommand::Claim(_) => num_cpus::get(),
        SubCommand::PoWCPU(sub_args) => sub_args.threads.into(),
    };

    let mut builder = if num_async_threads == 1 {
        tokio::runtime::Builder::new_current_thread()
    } else {
        let mut builder = tokio::runtime::Builder::new_multi_thread();
        let _ = builder.worker_threads(num_async_threads);
        builder
    };

    let _ = builder.max_blocking_threads(num_blocking_threads);
    let _ = builder.enable_all();
    let _ = builder.thread_name("amain");

    let runtime = builder
        .build()
        .context("Unable to initialize Tokio runtime")?;
    let _runtime_guard = runtime.enter();

    let exit_flag = ExitFlag::new();

    let wait_for_exit = {
        let mut exit_flag = exit_flag.clone();
        || async move { exit_flag.wait().await }
    };

    let run_main_and_handle_signals = || async move {
        try_join!(
            handle_signals(tracing_reload_fn, &exit_flag),
            async_main(args, exit_flag.clone())
        )
    };

    runtime.block_on(async {
        select! {
            r = wait_for_exit() => r,
            r = run_main_and_handle_signals() => r.map(|_| ()),
        }
    })?;

    Ok(())
}

async fn async_main(args: Cli, exit_flag: ExitFlag) -> Result<()> {
    #![allow(clippy::print_stdout)]

    let result = match args.subcommand {
        SubCommand::Info => {
            let info = get_ore_info(args.url, args.solana_timeout).await?;

            // Treasury.
            println!("Treasury:");
            println!(
                "    Difficulty: {} / {}",
                info.treasury.difficulty(),
                hex::encode(info.treasury.difficulty)
            );

            let last_reset = DateTime::<Utc>::from_timestamp(info.treasury.last_reset_at, 0)
                .ok_or_else(|| {
                    anyhow!("Invalid last reset date: {}", info.treasury.last_reset_at)
                })?;
            println!(
                "    Last reset: {} UTC",
                last_reset.format("%Y-%m-%d %H:%M:%S")
            );

            println!(
                "    Reward rate: {} ORE",
                conv_ore(info.treasury.reward_rate)
            );
            println!(
                "    Total claimed: {} ORE",
                conv_ore(info.treasury.total_claimed_rewards)
            );

            for (bus_idx, bus) in info.buses.iter().enumerate() {
                println!(
                    "Bus #{} / {}: {} ORE",
                    bus.id,
                    BUS_ADDRESSES[bus_idx],
                    conv_ore(bus.rewards)
                );
            }

            Ok(())
        }

        SubCommand::PoWCPU(sub_args) => {
            let server = PoWServer::new(sub_args.address).await?;
            let server_impl =
                pow::cpu::get_cpu_pow_server_impl(sub_args.threads.into(), exit_flag.clone());
            server
                .run(server_impl, exit_flag.clone())
                .await
                .context("CPU PoW server error")
        }

        SubCommand::PoWHashcat(sub_args) => {
            let server = PoWServer::new(sub_args.address).await?;
            let server_impl =
                pow::hashcat::get_hashcat_pow_server_impl(sub_args, exit_flag.clone());
            server
                .run(server_impl, exit_flag.clone())
                .await
                .context("Hashcat PoW server error")
        }

        SubCommand::PoW(sub_args) => {
            let client = PoWClient::new(sub_args.url, sub_args.timeout, exit_flag.clone());
            let results = client.find_hashes(&[sub_args.request]).await?;
            assert!(
                results.len() == 1,
                "Logic error: one request should yield exactly one response"
            );
            let result = results
                .first()
                .expect("Logic error: no result despite previous check");
            println!("{}", result);
            Ok(())
        }

        SubCommand::Signer(sub_args) => {
            let signer =
                Signer::new(args.url, args.solana_timeout, sub_args, exit_flag.clone()).await?;
            signer.run().await
        }

        SubCommand::Rewards(sub_args) => {
            let rewards = get_ore_rewards(
                args.url,
                args.solana_timeout,
                sub_args.keypair,
                sub_args.accounts,
            )
            .await?;
            println!("Unclaimed rewards: {} ORE", conv_ore(rewards));
            Ok(())
        }

        SubCommand::Claim(sub_args) => {
            claim::claim(
                args.url,
                sub_args.keypair,
                sub_args.accounts,
                args.solana_timeout,
                sub_args.beneficiary,
            )
            .await
        }
    };

    exit_flag.stop().await;

    result
}
