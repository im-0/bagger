use anyhow::{Context, Result};
use tokio::select;
use tokio::signal::{
    self,
    unix::{self, Signal, SignalKind},
};
use tokio::sync::watch;
use tracing::{debug, info, trace};

use crate::trace::{toggle_tracing_override, TracingReloadFn};

#[derive(Clone)]
pub struct ExitFlag {
    sender: watch::Sender<bool>,
    receiver: watch::Receiver<bool>,
}

impl ExitFlag {
    pub fn new() -> Self {
        let (sender, receiver) = watch::channel(false);

        Self { sender, receiver }
    }

    pub async fn wait(&mut self) -> Result<()> {
        self.receiver
            .wait_for(|terminate| *terminate)
            .await
            .context("Unable to receive graceful shutdown flag")
            .map(|_| ())
    }

    pub async fn stop(&self) {
        self.sender
            .send(true)
            .map_err(|_| trace!("Unable to send graceful shutdown flag: channel closed"))
            .unwrap_or(())
    }
}

pub async fn handle_signals(
    tracing_reload_fn: TracingReloadFn,
    exit_flag: &ExitFlag,
) -> Result<()> {
    let mut sigterm =
        unix::signal(SignalKind::terminate()).context("Unable to configure SIGTERM listener")?;
    let mut sigusr2 = unix::signal(SignalKind::user_defined2())
        .context("Unable to configure SIGUSR2 listener")?;

    loop {
        handle_one_signal(&mut sigterm, &mut sigusr2, &tracing_reload_fn, exit_flag).await?
    }
}

async fn handle_one_signal(
    sigterm: &mut Signal,
    sigusr2: &mut Signal,
    tracing_reload_fn: &TracingReloadFn,
    exit_flag: &ExitFlag,
) -> Result<()> {
    select! {
        r = signal::ctrl_c() => {
            r.context("Unable to receive SIGINT/Ctrl+C")?;
            info!("Got SIGINT/Ctrl+C, exiting");

            exit_flag.stop().await
        }

        r = sigterm.recv() => {
            r.context("Unable to receive SIGTERM")?;
            info!("Got SIGTERM, exiting");

            exit_flag.stop().await
        }

        r = sigusr2.recv() => {
            r.context("Unable to receive SIGTERM")?;
            debug!("Got SIGUSR2");

            toggle_tracing_override(tracing_reload_fn)?;
        }
    }

    Ok(())
}
