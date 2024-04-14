use jsonrpsee::{core::SubscriptionResult, proc_macros::rpc, types::ErrorObjectOwned};
use thiserror::Error;

use crate::pow::types::PoWRequest;

use super::types::PoWResponse;

#[derive(Debug, Error)]
pub enum PoWRpcError {
    #[error("PoW error: {0}")]
    Any(String),
}

impl From<PoWRpcError> for ErrorObjectOwned {
    fn from(error: PoWRpcError) -> Self {
        match error {
            PoWRpcError::Any(message) => Self::owned(0, message, None::<()>),
        }
    }
}

#[rpc(server, client)]
pub trait PoWRpc {
    #[method(name = "schedule_work")]
    async fn schedule_work(&self, requests: Vec<PoWRequest>) -> Result<(), PoWRpcError>;

    #[subscription(name = "subscribe_results", item = Option<PoWResponse>)]
    async fn subscribe_results(&self) -> SubscriptionResult;
}
