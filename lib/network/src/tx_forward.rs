use crate::protocol::ConnectionRegistry;
use crate::version::{ZksProtocolV5, ZksVersion};
use crate::wire::message::ZksMessage;
use crate::wire::transactions::{ForwardRawTransaction, ForwardRawTransactionResult};
use alloy::primitives::Bytes;
use dashmap::DashMap;
use reth_network_peers::PeerId;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::oneshot;

const TX_FORWARD_RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug)]
pub struct PeerForwardedRawTransaction {
    pub peer_id: PeerId,
    pub tx: Bytes,
    pub response_tx: oneshot::Sender<Result<(), String>>,
}

#[derive(Debug, thiserror::Error)]
pub enum TxForwardError {
    #[error("transaction forward target {0} is not connected")]
    NotConnected(PeerId),
    #[error("transaction forward target {peer_id} negotiated unsupported zks version {version:?}")]
    UnsupportedProtocol {
        peer_id: PeerId,
        version: ZksVersion,
    },
    #[error("failed to send transaction forward request to {0}")]
    SendFailed(PeerId),
    #[error("transaction forward response from {0} timed out")]
    Timeout(PeerId),
    #[error("transaction forward response channel closed")]
    ResponseDropped,
    #[error("transaction forward target rejected transaction: {0}")]
    RemoteRejected(String),
}

#[derive(Debug, Clone)]
pub struct TxForwardHandle {
    next_request_id: Arc<AtomicU64>,
    pending: Arc<DashMap<u64, oneshot::Sender<Result<(), String>>>>,
    connection_registry: ConnectionRegistry,
}

impl TxForwardHandle {
    pub(crate) fn new(connection_registry: ConnectionRegistry) -> Self {
        Self {
            next_request_id: Arc::new(AtomicU64::new(1)),
            pending: Arc::new(DashMap::new()),
            connection_registry,
        }
    }

    pub fn empty() -> Self {
        Self::new(Arc::new(RwLock::new(HashMap::new())))
    }

    pub async fn forward_raw_transaction(
        &self,
        peer_id: PeerId,
        tx: Bytes,
    ) -> Result<(), TxForwardError> {
        let connection = self
            .connection_registry
            .read()
            .expect("protocol connection registry lock poisoned")
            .get(&peer_id)
            .cloned()
            .ok_or(TxForwardError::NotConnected(peer_id))?;

        if connection.version < ZksVersion::Zks5 {
            return Err(TxForwardError::UnsupportedProtocol {
                peer_id,
                version: connection.version,
            });
        }

        let request_id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        let (response_tx, response_rx) = oneshot::channel();
        self.pending.insert(request_id, response_tx);

        let message = ZksMessage::<ZksProtocolV5>::ForwardRawTransaction(ForwardRawTransaction {
            request_id,
            tx,
        })
        .encoded();

        if connection.outbound_tx.send(message).await.is_err() {
            self.pending.remove(&request_id);
            return Err(TxForwardError::SendFailed(peer_id));
        }

        match tokio::time::timeout(TX_FORWARD_RESPONSE_TIMEOUT, response_rx).await {
            Ok(Ok(Ok(()))) => Ok(()),
            Ok(Ok(Err(error))) => Err(TxForwardError::RemoteRejected(error)),
            Ok(Err(_)) => Err(TxForwardError::ResponseDropped),
            Err(_) => {
                self.pending.remove(&request_id);
                Err(TxForwardError::Timeout(peer_id))
            }
        }
    }

    pub(crate) fn complete_response(&self, result: ForwardRawTransactionResult) {
        let response = match result.error {
            None => Ok(()),
            Some(error) => Err(error),
        };
        if let Some((_, pending)) = self.pending.remove(&result.request_id) {
            let _ = pending.send(response);
        }
    }
}
