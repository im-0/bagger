use std::{
    sync::{
        atomic::{self, AtomicBool},
        Arc,
    },
    time::Instant,
};

use anyhow::{anyhow, Context, Result};
use futures::future::try_join_all;
use solana_sdk::keccak;
use tokio::{select, sync::mpsc, task};
use tracing::{debug, info};

use crate::{
    pow::server::GetServerImplFnBoxed,
    pow::types::{NextHash, PoWRequest},
    signals::ExitFlag,
};

use super::types::PoWResponse;

static STOP: AtomicBool = AtomicBool::new(false);

pub fn get_cpu_pow_server_impl(threads: usize, exit_flag: ExitFlag) -> GetServerImplFnBoxed {
    Box::new(move |requests_receiver, results_sender| {
        Box::pin(run_cpu_pow(
            requests_receiver,
            results_sender,
            threads,
            exit_flag.clone(),
        ))
    })
}

async fn run_cpu_pow(
    requests_receiver: mpsc::Receiver<Vec<PoWRequest>>,
    results_sender: mpsc::Sender<PoWResponse>,
    threads: usize,
    mut exit_flag: ExitFlag,
) -> Result<()> {
    let wait_for_exit = {
        || async move {
            let result = exit_flag.wait().await;
            STOP.store(true, atomic::Ordering::Relaxed);
            result
        }
    };

    let run_and_wait_for_exit = || async move {
        select! {
            r = wait_for_exit() => r,
            r = cpu_pow_thread(requests_receiver, results_sender, threads) => r,
        }
    };

    debug!("Starting CPU compute thread");
    let result = tokio::spawn(run_and_wait_for_exit()).await;
    debug!("CPU compute thread stopped");
    result
        .context("CPU PoW thread failed")?
        .context("CPU PoW error")
}

async fn cpu_pow_thread(
    mut requests_receiver: mpsc::Receiver<Vec<PoWRequest>>,
    results_sender: mpsc::Sender<PoWResponse>,
    threads: usize,
) -> Result<()> {
    'main: loop {
        match requests_receiver.recv().await {
            Some(requests) => {
                info!("Got {} requests", requests.len());

                for request in &requests {
                    let start = Instant::now();

                    let next_hash = cpu_pow_iteration(request, threads)
                        .await
                        .context("CPU PoW iteration failed")?;

                    let elapsed_compute = start.elapsed();
                    info!("Compute time elapsed: {}s", elapsed_compute.as_secs_f64());

                    if results_sender
                        .send((request.clone(), next_hash, elapsed_compute))
                        .await
                        .is_err()
                    {
                        debug!("Results receiver closed");
                        break 'main;
                    }
                }
            }

            None => {
                debug!("Requests sender closed");
                break;
            }
        }
    }

    Ok(())
}

async fn cpu_pow_iteration(request: &PoWRequest, threads: usize) -> Result<NextHash> {
    let mut hasher = keccak::Hasher::default();
    hasher.hashv(&[request.prev_hash.as_ref(), request.signer.as_ref()]);

    let found_solution = Arc::new(AtomicBool::new(false));
    let handles: Vec<_> = (0..threads)
        .map(|thread_idx| {
            let request = request.clone();

            let found_solution = found_solution.clone();
            let hasher = hasher.clone();

            task::spawn_blocking(move || {
                let mut nonce: u64 = u64::MAX
                    .saturating_div(threads as u64)
                    .saturating_mul(thread_idx as u64);

                loop {
                    let mut next_hasher = hasher.clone();
                    next_hasher.hash(&nonce.to_le_bytes());
                    let next_hash = next_hasher.result();

                    if next_hash.le(&request.difficulty) {
                        found_solution.store(true, atomic::Ordering::Relaxed);
                        return Some(NextHash { next_hash, nonce });
                    }

                    if (nonce % 10_000 == 0)
                        && (found_solution.load(atomic::Ordering::Relaxed)
                            || STOP.load(atomic::Ordering::Relaxed))
                    {
                        return None;
                    }

                    nonce += 1;
                }
            })
        })
        .collect();

    try_join_all(handles)
        .await
        .context("CPU PoW compute thread error")?
        .into_iter()
        .flatten()
        .take(1)
        .next()
        .ok_or_else(|| anyhow!("All PoW compute threads finished, but there is no result"))
}
