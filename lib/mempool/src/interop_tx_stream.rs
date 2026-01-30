use std::{
    collections::VecDeque,
    pin::Pin,
    sync::{Arc, RwLock},
    task::{Context, Poll},
};

use futures::Stream;
use std::time::Instant;
use tokio::sync::broadcast::{self, error::TryRecvError};
use zksync_os_types::{
    IndexedInteropRoot, IndexedInteropRootsEnvelope, InteropRootsEnvelope, InteropRootsLogIndex,
};

pub struct InteropTxPool {
    inner: Arc<RwLock<InteropTxPoolInner>>,
}

impl InteropTxPool {
    pub fn new(buffer_size: usize) -> Self {
        Self {
            inner: Arc::new(RwLock::new(InteropTxPoolInner::new(buffer_size))),
        }
    }
}

impl Clone for InteropTxPool {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl InteropTxPool {
    pub fn delayed_transaction_stream(
        &self,
        interop_roots_per_tx: usize,
        next_tx_allowed_after: Instant,
    ) -> InteropTxStream {
        self.inner
            .write()
            .expect("Failed to read from interop tx pool")
            .delayed_transaction_stream(interop_roots_per_tx, next_tx_allowed_after)
    }

    pub fn add_root(&self, root: IndexedInteropRoot) {
        self.inner
            .write()
            .expect("Failed to write to interop tx pool")
            .add_root(root);
    }

    pub async fn on_canonical_state_change(
        &self,
        txs: Vec<InteropRootsEnvelope>,
    ) -> Option<InteropRootsLogIndex> {
        self.inner
            .write()
            .expect("Failed to write to interop tx pool")
            .on_canonical_state_change(txs)
            .await
    }
}

struct InteropTxPoolInner {
    sender: broadcast::Sender<IndexedInteropRoot>,
    pending_roots: VecDeque<IndexedInteropRoot>,
    sent_roots: VecDeque<IndexedInteropRoot>,
}

pub struct InteropTxStream {
    receiver: broadcast::Receiver<IndexedInteropRoot>,
    pending_roots: VecDeque<IndexedInteropRoot>,
    interop_roots_per_tx: usize,
    next_tx_allowed_after: Instant,
}

impl Stream for InteropTxStream {
    type Item = IndexedInteropRootsEnvelope;

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        if Instant::now() < this.next_tx_allowed_after {
            return Poll::Pending;
        }

        loop {
            match this.receiver.try_recv() {
                Ok(root) => {
                    if let Some(envelope) = this.add_root_and_try_take_tx(root) {
                        return Poll::Ready(Some(envelope));
                    }
                    continue;
                }
                Err(TryRecvError::Empty) | Err(TryRecvError::Lagged(_)) => {
                    if let Some(envelope) = this.take_tx() {
                        return Poll::Ready(Some(envelope));
                    }
                    return Poll::Pending;
                }
                Err(TryRecvError::Closed) => {
                    return Poll::Ready(None);
                }
            }
        }
    }
}

impl InteropTxStream {
    /// Add a new root to pending roots and return transaction if the limit of interop roots per import is reached
    fn add_root_and_try_take_tx(
        &mut self,
        root: IndexedInteropRoot,
    ) -> Option<IndexedInteropRootsEnvelope> {
        self.pending_roots.push_back(root);

        if self.pending_roots.len() >= self.interop_roots_per_tx {
            self.take_tx()
        } else {
            None
        }
    }

    /// Take a transaction from pending roots(not depending on the amount)
    fn take_tx(&mut self) -> Option<IndexedInteropRootsEnvelope> {
        if self.pending_roots.is_empty() {
            None
        } else {
            let amount_of_roots_to_take = self.pending_roots.len().min(self.interop_roots_per_tx);
            let roots_to_consume = self
                .pending_roots
                .drain(..amount_of_roots_to_take)
                .collect::<Vec<_>>();

            let tx = IndexedInteropRootsEnvelope {
                log_index: roots_to_consume.last().unwrap().log_index.clone(),
                envelope: InteropRootsEnvelope::from_interop_roots(
                    roots_to_consume.iter().map(|r| r.root.clone()).collect(),
                ),
            };

            Some(tx)
        }
    }
}

impl InteropTxPoolInner {
    pub fn new(buffer_size: usize) -> Self {
        Self {
            sender: broadcast::Sender::new(buffer_size),
            pending_roots: VecDeque::new(),
            sent_roots: VecDeque::new(),
        }
    }

    pub fn delayed_transaction_stream(
        &mut self,
        interop_roots_per_tx: usize,
        next_tx_allowed_after: Instant,
    ) -> InteropTxStream {
        self.is_stream_active = true;

        InteropTxStream {
            receiver: self.sender.subscribe(),
            pending_roots: self.pending_roots.clone(),
            interop_roots_per_tx,
            next_tx_allowed_after,
        }
    }

    pub fn add_root(&mut self, root: IndexedInteropRoot) {
        self.sender.send(root.clone()).expect("Failed to send root");

        if self.sender.receiver_count() > 0 {
            self.sent_roots.push_front(root);
        } else {
            self.pending_roots.push_front(root);
        }
    }

    /// Take next root in the following order:
    /// - used roots
    /// - pending roots
    /// - receiver
    async fn take_next_root(&mut self) -> Option<IndexedInteropRoot> {
        if let Some(root) = self.sent_roots.pop_back() {
            Some(root)
        } else {
            self.pending_roots.pop_back()
        }
    }

    /// Cleans up the stream and removes all roots that were sent in transactions
    /// Returns the last log index of executed interop root
    pub async fn on_canonical_state_change(
        &mut self,
        txs: Vec<InteropRootsEnvelope>,
    ) -> Option<InteropRootsLogIndex> {
        if txs.is_empty() {
            return None;
        }

        let mut log_index = InteropRootsLogIndex::default();

        for tx in txs {
            let mut roots = Vec::new();
            for _ in 0..tx.interop_roots_count() {
                roots.push(self.take_next_root().await.unwrap());
            }

            let envelope = InteropRootsEnvelope::from_interop_roots(
                roots.iter().map(|r| r.root.clone()).collect(),
            );
            log_index = roots.last().unwrap().log_index.clone();

            assert_eq!(&envelope, &tx);
        }

        self.pending_roots.extend(self.sent_roots.drain(..));

        Some(log_index)
    }
}
