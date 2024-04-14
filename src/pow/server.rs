use std::{
    net::SocketAddr,
    num::NonZeroUsize,
    sync::{Arc, Mutex},
};

use anyhow::{anyhow, Context, Result};
use futures::future::{join_all, BoxFuture};
use jsonrpsee::{
    core::{async_trait, SubscriptionResult},
    server::{Server, ServerHandle},
    PendingSubscriptionSink, SubscriptionMessage, SubscriptionSink,
};
use lru::LruCache;
use tokio::{select, sync::mpsc, try_join};
use tracing::{debug, info, trace};

use crate::{
    pow::rpc::{PoWRpcError, PoWRpcServer},
    signals::ExitFlag,
};

use crate::pow::types::{PoWRequest, PoWResponse};

// TODO: Do not duplicate PoWRequest in both key and value.
type Cache = LruCache<PoWRequest, PoWResponse>;

const CHANNEL_SIZE: usize = 16384;
const CACHE_SIZE: NonZeroUsize = nonzero_lit::usize!(16384);

pub type ServerImplFuture = BoxFuture<'static, Result<()>>;
pub type GetServerImplFnBoxed =
    Box<dyn Fn(mpsc::Receiver<Vec<PoWRequest>>, mpsc::Sender<PoWResponse>) -> ServerImplFuture>;

pub struct PoWServer {
    request_receiver: mpsc::Receiver<Vec<PoWRequest>>,
    response_sender: mpsc::Sender<PoWResponse>,
    response_receiver: mpsc::Receiver<PoWResponse>,
    sinks: Arc<Mutex<Vec<Arc<SubscriptionSink>>>>,
    cache: Arc<Mutex<Cache>>,
    handle: ServerHandle,
}

impl PoWServer {
    pub async fn new(address: SocketAddr) -> Result<Self> {
        debug!("Starting PoW RPC server on {}...", address);
        let server = Server::builder()
            .ws_only()
            .build(address)
            .await
            .context("Unable to create RPC server for PoW")?;
        info!(
            "PoW RPC server started on {}",
            server
                .local_addr()
                .context("Unable to get local address of PoW RPC server")?
        );

        let (request_sender, request_receiver) = mpsc::channel(CHANNEL_SIZE);
        let (response_sender, response_receiver) = mpsc::channel(CHANNEL_SIZE);

        let sinks = Arc::new(Mutex::new(Vec::new()));
        let cache = Arc::new(Mutex::new(Cache::new(CACHE_SIZE)));
        let server_impl = PoWServerImpl {
            request_sender,
            response_sender: response_sender.clone(),
            sinks: sinks.clone(),
            cache: cache.clone(),
        };

        Ok(Self {
            request_receiver,
            response_sender,
            response_receiver,
            sinks,
            cache,
            handle: server.start(server_impl.into_rpc()),
        })
    }

    pub async fn run(
        self,
        server_impl_fn: GetServerImplFnBoxed,
        exit_flag: ExitFlag,
    ) -> Result<()> {
        let wait_for_exit = {
            let mut exit_flag = exit_flag.clone();
            let handle = self.handle.clone();

            || async move {
                exit_flag.wait().await?;
                let _ = handle
                    .stop()
                    .map_err(|_| debug!("PoW RPC server already stopped"));
                Ok(())
            }
        };

        let run_server = || async {
            self.handle.stopped().await;
            Result::<(), anyhow::Error>::Ok(())
        };

        let result = try_join!(
            wait_for_exit(),
            run_server(),
            Self::forward_responses(self.response_receiver, self.sinks, self.cache, exit_flag),
            server_impl_fn(self.request_receiver, self.response_sender),
        )
        .map(|_| ());
        info!("PoW RPC server stopped");
        result
    }

    async fn forward_responses(
        mut response_receiver: mpsc::Receiver<PoWResponse>,
        sinks: Arc<Mutex<Vec<Arc<SubscriptionSink>>>>,
        cache: Arc<Mutex<Cache>>,
        exit_flag: ExitFlag,
    ) -> Result<()> {
        loop {
            let response = response_receiver
                .recv()
                .await
                .ok_or_else(|| anyhow!("Unable to receive next hash from worker: sender closed"))?;
            trace!("Got next hash from worker or cache: {:?}", response);

            // Insert into cache.
            {
                let mut cache = cache
                    .lock()
                    .expect("Unable to lock cache for inserting new value");
                let _ = cache.put(response.0.clone(), response.clone());
            }

            // Send response to all subscribers.
            let sinks = sinks.clone();
            let mut exit_flag = exit_flag.clone();
            let forward_responses_and_wait_for_exit = || async move {
                select! {
                    r = exit_flag.wait() => r,
                    r = Self::forward_responses_thread(response, sinks) => r,
                }
            };

            // TODO: Join it somewhere.
            let _handle = tokio::spawn(forward_responses_and_wait_for_exit());
        }
    }

    async fn forward_responses_thread(
        response: PoWResponse,
        sinks: Arc<Mutex<Vec<Arc<SubscriptionSink>>>>,
    ) -> Result<()> {
        let sinks_clone = {
            sinks
                .lock()
                .expect("Unable to lock subscription sinks for iterating")
                .clone()
        };
        trace!("Got {} active subscription sinks", sinks_clone.len());

        let msg = SubscriptionMessage::from_json(&Some(response))
            .context("Unable to serialize PoW response")?;

        let handles: Vec<_> = sinks_clone
            .iter()
            .map(|sink| async {
                sink.send(msg.clone())
                    .await
                    .map_err(|error| {
                        debug!("Subscription {:?} error: {}", sink.subscription_id(), error);
                        error
                    })
                    .context("Unable to notify client that connection is active")
            })
            .collect();

        let send_results = join_all(handles).await;
        trace!("Sent next hash to all subscribers");

        let had_errors = send_results
            .iter()
            .map(|send_result| send_result.is_err())
            .any(|had_error| had_error);
        if had_errors {
            let mut sinks = sinks
                .lock()
                .expect("Unable to lock subscription sinks for removing dead sinks");

            let dead_sinks: Vec<_> = sinks
                .iter()
                .enumerate()
                .filter_map(|(sink_idx, sink)| {
                    if sink.is_closed() {
                        info!("Client {:?} closed subscription", sink.subscription_id());
                        Some(sink_idx)
                    } else {
                        None
                    }
                })
                .collect();

            for (offset, dead_sink_idx) in dead_sinks.iter().enumerate() {
                let _ = sinks.remove(dead_sink_idx - offset);
            }
            trace!("{} dead sinks removed", dead_sinks.len());
        }

        Ok(())
    }
}

struct PoWServerImpl {
    request_sender: mpsc::Sender<Vec<PoWRequest>>,
    response_sender: mpsc::Sender<PoWResponse>,
    sinks: Arc<Mutex<Vec<Arc<SubscriptionSink>>>>,
    cache: Arc<Mutex<Cache>>,
}

#[async_trait]
impl PoWRpcServer for PoWServerImpl {
    async fn schedule_work(&self, requests: Vec<PoWRequest>) -> Result<(), PoWRpcError> {
        trace!("Got more work: {:?})", requests);
        info!("Client requested {} hashes", requests.len());

        // Check cache.
        let (mut from_cache, remaining) = {
            let mut cache = self.cache.lock().expect("Unable to lock cache for reading");

            let mut from_cache = Vec::new();
            let mut remaining = Vec::new();
            for request in &requests {
                match cache.get(request) {
                    Some(response) => from_cache.push(response.clone()),
                    None => remaining.push(request.clone()),
                }
            }

            (from_cache, remaining)
        };
        let served_from_cache = from_cache.len();
        for response in from_cache.drain(..) {
            if self.response_sender.send(response).await.is_err() {
                return Err(PoWRpcError::Any(
                    "Failed to send cached response: receiver closed".to_string(),
                ));
            }
        }

        info!(
            "{} served from cache, {} remaining",
            served_from_cache,
            remaining.len()
        );

        if remaining.is_empty() {
            return Ok(());
        }

        // Process remaining requests normally.
        self.request_sender.send(remaining).await.map_err(|_| {
            PoWRpcError::Any("Unable to send request to worker: receiver closed".to_string())
        })
    }

    async fn subscribe_results(&self, pending: PendingSubscriptionSink) -> SubscriptionResult {
        trace!("Got PoW RPC sunscription request");
        let sink = Arc::new(
            pending
                .accept()
                .await
                .context("Unable to accept incoming PoW RPC subscribe request")?,
        );
        info!("Client {:?} subscribed for results", sink.subscription_id());

        {
            let mut sinks = self
                .sinks
                .lock()
                .expect("Unable to lock subscription sinks for adding");
            sinks.push(sink.clone());
        }

        // Notify client that connection is active.
        let msg = SubscriptionMessage::from_json(&Option::<PoWResponse>::None)?;
        sink.send(msg)
            .await
            .context("Unable to notify client that connection is active")?;

        Ok(())
    }
}
