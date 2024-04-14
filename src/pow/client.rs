use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{anyhow, bail, ensure, Context, Result};
use futures::future::try_join_all;
use jsonrpsee::{
    core::client::Subscription,
    ws_client::{WsClient, WsClientBuilder},
};
use tokio::{
    select,
    sync::{mpsc, oneshot},
    task::JoinHandle,
    time::sleep,
};
use tracing::{debug, info, trace, warn};
use tracing_log::log::error;

use crate::{pow::rpc::PoWRpcClient, signals::ExitFlag};

use super::types::{NextHash, PoWRequest, PoWResponse};

const RECONNECT_SLEEP: Duration = Duration::from_secs(4);
const REQUEST_CHANNEL_SIZE: usize = 16384;

pub struct PoWClient {
    thread: PoWClientThread,
}

impl PoWClient {
    pub fn new(url: String, timeout: Duration, exit_flag: ExitFlag) -> Self {
        let builder = WsClientBuilder::new()
            .connection_timeout(timeout)
            .request_timeout(timeout);

        Self {
            thread: PoWClientThread::new(url, builder, exit_flag),
        }
    }

    pub async fn find_hashes(&self, requests: &[PoWRequest]) -> Result<Vec<NextHash>> {
        debug!("Sending PoW requests: {:?}", requests);

        let start = Instant::now();

        let receivers = self
            .thread
            .schedule_work(requests)
            .await?
            .into_iter()
            .enumerate()
            .map(|(request_idx, receiver)| async move {
                receiver.await.map(|(next_hash, elapsed_compute)| {
                    let elapsed_real = start.elapsed();
                    info!(
                        "Hash found {} => {} ({}s compute, {}s real)",
                        requests[request_idx],
                        next_hash,
                        elapsed_compute.as_secs_f64(),
                        elapsed_real.as_secs_f64(),
                    );

                    next_hash
                })
            });

        let next_hashes = try_join_all(receivers)
            .await
            .expect("Logic error: next hash Sender dropped before find_hashes() future");

        Ok(next_hashes)
    }
}

type NextHashSender = oneshot::Sender<(NextHash, Duration)>;
type NextHashSenderMap = HashMap<PoWRequest, NextHashSender>;

struct PoWClientThread {
    scheduled: Arc<Mutex<NextHashSenderMap>>,
    sender: mpsc::Sender<Vec<PoWRequest>>,
    _handle: JoinHandle<()>, // TODO: Join somewhere.
}

impl PoWClientThread {
    fn new(url: String, builder: WsClientBuilder, exit_flag: ExitFlag) -> Self {
        let scheduled = Arc::new(Mutex::new(NextHashSenderMap::new()));
        let (sender, receiver) = mpsc::channel(REQUEST_CHANNEL_SIZE);

        // TODO: Multiple client "threads".
        let handle = tokio::spawn(Self::client_loop(
            url,
            builder,
            scheduled.clone(),
            receiver,
            exit_flag,
        ));

        Self {
            scheduled,
            sender,
            _handle: handle,
        }
    }

    async fn schedule_work(
        &self,
        requests: &[PoWRequest],
    ) -> Result<Vec<oneshot::Receiver<(NextHash, Duration)>>> {
        let receivers = {
            let mut scheduled = self
                .scheduled
                .lock()
                .expect("Unable to lock NextHashSenderMap for writing");

            requests
                .iter()
                .map(|request| {
                    let (sender, receiver) = oneshot::channel();
                    assert!(
                        scheduled.insert(request.clone(), sender).is_none(),
                        "Logic error: PoW request already scheduled"
                    );

                    receiver
                })
                .collect()
        };

        self.sender
            .send(requests.to_vec())
            .await
            .map_err(|error| {
                let mut scheduled = self
                    .scheduled
                    .lock()
                    .expect("Unable to lock NextHashSenderMap for writing (removing reqeust)");

                for request in &error.0 {
                    let _ = scheduled.remove(request);
                }

                error
            })
            .context("Unable to schedule PoW request: channel closed")?;

        Ok(receivers)
    }

    async fn client_loop(
        url: String,
        builder: WsClientBuilder,
        scheduled: Arc<Mutex<NextHashSenderMap>>,
        mut receiver: mpsc::Receiver<Vec<PoWRequest>>,
        mut exit_flag: ExitFlag,
    ) {
        let client_loop = || async move {
            let mut first = true;

            while let Err(error) =
                Self::client_loop_iter(&url, &builder, scheduled.clone(), &mut receiver, first)
                    .await
            {
                error!(
                    "PoW RPC client failed: {}, will reconnect in {}s",
                    error,
                    RECONNECT_SLEEP.as_secs()
                );
                sleep(RECONNECT_SLEEP).await;

                first = false;
            }
        };

        select! {
            _ = exit_flag.wait() => (),
            _ = client_loop() => ()
        }

        debug!("PoW RPC client stopped");
    }

    async fn client_loop_iter(
        url: &str,
        builder: &WsClientBuilder,
        scheduled: Arc<Mutex<NextHashSenderMap>>,
        receiver: &mut mpsc::Receiver<Vec<PoWRequest>>,
        is_first_iteration: bool,
    ) -> Result<()> {
        info!("Connecting PoW RPC client to {}", url);
        let client = Self::create_client(url, builder).await?;

        // First, subscribe for results.
        trace!("Subscribing for PoW results...");
        let mut subscription = PoWRpcClient::subscribe_results(&client)
            .await
            .map_err(Self::convert_error)
            .context("Unable to subscribe for PoW results")?;

        // Server should send us None to indicate that subscription is active.
        trace!("Receiving first packet from PoW RPC server...");
        let pong = Self::recv_result(&mut subscription).await?;
        trace!("First packet from PoW RPC server received");
        ensure!(
            pong.is_none(),
            "PoW server unexpectedly sent non-empty result as a first packet"
        );

        if !is_first_iteration {
            // Send all requests after reconnect.
            let requests = {
                let scheduled = scheduled
                    .lock()
                    .expect("Unable lock NextHashSenderMap inside client thread for reading");
                scheduled.keys().cloned().collect()
            };
            Self::schedule_work_rpc(&client, requests).await?;
        }

        loop {
            select! {
                result = Self::recv_result(&mut subscription) => {
                    let (request, next_hash, elapsed_compute) = match result? {
                        Some(result) => result,
                        None => bail!("PoW server unexpectedly sent empty result"),
                    };

                    let mut scheduled = scheduled
                        .lock()
                        .expect("Unable to lock NextHashSenderMap inside client thread for writing");
                    match scheduled.remove(&request) {
                        Some(sender) => {
                            if sender.send((next_hash, elapsed_compute)).is_err() {
                                debug!("Unable to send next hash to PoW client: channel closed");
                                break
                            }
                        }

                        None => {
                            warn!(
                                "PoW server sent result for nonexistent request {:?}, client restarted, multiple clients or a duplicate?",
                                request)
                        },
                    }
                }

                request = receiver.recv() => match request {
                    Some(requests) => Self::schedule_work_rpc(&client, requests).await?,

                    None => {
                        debug!("Unable to receive from PoW request channel: closed");
                        break
                    }
                }
            }
        }

        Ok(())
    }

    async fn recv_result(
        subscription: &mut Subscription<Option<PoWResponse>>,
    ) -> Result<Option<PoWResponse>> {
        subscription
            .next()
            .await
            .ok_or_else(|| anyhow!("PoW subscription closed unexpectedly"))?
            .map_err(Self::convert_error)
            .context("Unable to receive next PoW result from server")
    }

    async fn schedule_work_rpc(client: &WsClient, requests: Vec<PoWRequest>) -> Result<()> {
        PoWRpcClient::schedule_work(client, requests)
            .await
            .map_err(Self::convert_error)
            .context("Unable to schedule PoW requests")
    }
    async fn create_client(url: &str, builder: &WsClientBuilder) -> Result<WsClient> {
        builder
            .clone()
            .build(url)
            .await
            .map_err(Self::convert_error)
            .context("Unable to create WebSocket client for PoW RPC")
    }

    fn convert_error(error: jsonrpsee::core::Error) -> anyhow::Error {
        #[allow(clippy::wildcard_enum_match_arm)]
        match error {
            jsonrpsee::core::Error::Call(error) => anyhow!("PoW server error: {}", error),
            error => anyhow!("PoW RPC client error: {}", error),
        }
    }
}
