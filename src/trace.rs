use std::{
    io::stderr,
    sync::atomic::{AtomicBool, Ordering},
};

use anyhow::{anyhow, Context, Result};
use tracing::info;
use tracing_core::LevelFilter;
use tracing_subscriber::EnvFilter;

pub type TracingReloadFn =
    Box<dyn Fn(EnvFilter) -> std::result::Result<(), tracing_subscriber::reload::Error>>;

pub fn init_tracing(without_timestamp: bool) -> Result<TracingReloadFn> {
    let filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .from_env_lossy();

    let builder = tracing_subscriber::fmt()
        .with_thread_names(true)
        .with_target(true)
        .with_env_filter(filter)
        .with_writer(stderr);

    let reload_fn: TracingReloadFn = if without_timestamp {
        let builder = builder.without_time().with_filter_reloading();
        let reload_handle = builder.reload_handle();
        builder.try_init().map_err(|err| anyhow!(err))?;
        Box::new(move |new_filter| reload_handle.reload(new_filter))
    } else {
        let builder = builder.with_filter_reloading();
        let reload_handle = builder.reload_handle();
        builder.try_init().map_err(|err| anyhow!(err))?;
        Box::new(move |new_filter| reload_handle.reload(new_filter))
    };

    Ok(reload_fn)
}

pub fn toggle_tracing_override(reload_fn: &TracingReloadFn) -> Result<()> {
    static NEG_OVERRIDE: AtomicBool = AtomicBool::new(true);

    let new_filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .from_env_lossy();

    let new_filter = if NEG_OVERRIDE.fetch_xor(true, Ordering::Relaxed) {
        info!("Tracing level override set to TRACE");
        new_filter.add_directive(LevelFilter::TRACE.into())
    } else {
        info!("Tracing level override disabled");
        new_filter
    };

    reload_fn(new_filter).context("Unable to reload logging/tracing")
}
