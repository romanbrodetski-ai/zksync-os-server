use std::{
    collections::VecDeque,
    pin::Pin,
    task::{Context, Poll},
};

use futures::Stream;
use tokio::sync::mpsc;
use zksync_os_types::{
    IndexedInteropRoot, IndexedInteropRootsEnvelope, InteropRootsEnvelope, InteropRootsLogIndex,
};

const INTEROP_ROOTS_PER_IMPORT: usize = 100;

/// Stream that accumulates interop roots and produces interop transactions
/// It also keeps track of sent roots to be able to return them back to stream
/// in case tx was excluded from the block
pub struct InteropTxStream {
    receiver: mpsc::Receiver<IndexedInteropRoot>,
    pending_roots: VecDeque<IndexedInteropRoot>,
    used_roots: VecDeque<IndexedInteropRoot>,
}

impl Stream for InteropTxStream {
    type Item = IndexedInteropRootsEnvelope;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            match this.receiver.poll_recv(cx) {
                Poll::Ready(Some(root)) => {
                    if let Some(envelope) = this.add_root_and_try_take_tx(root) {
                        return Poll::Ready(Some(envelope));
                    }
                    continue;
                }
                Poll::Pending => {
                    if let Some(envelope) = this.take_tx() {
                        return Poll::Ready(Some(envelope));
                    }
                    return Poll::Pending;
                }
                Poll::Ready(None) => {
                    return Poll::Ready(None);
                }
            }
        }
    }
}

impl InteropTxStream {
    pub fn new(receiver: mpsc::Receiver<IndexedInteropRoot>) -> Self {
        Self {
            receiver,
            pending_roots: VecDeque::new(),
            used_roots: VecDeque::new(),
        }
    }

    /// Add a new root to pending roots and return transaction if the limit of interop roots per import is reached
    fn add_root_and_try_take_tx(
        &mut self,
        root: IndexedInteropRoot,
    ) -> Option<IndexedInteropRootsEnvelope> {
        self.pending_roots.push_back(root);

        if self.pending_roots.len() == INTEROP_ROOTS_PER_IMPORT {
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
            let tx = IndexedInteropRootsEnvelope {
                log_index: self.pending_roots.back().unwrap().log_index.clone(),
                envelope: InteropRootsEnvelope::from_interop_roots(
                    self.pending_roots.iter().map(|r| r.root.clone()).collect(),
                ),
            };

            self.used_roots.extend(self.pending_roots.drain(..));

            Some(tx)
        }
    }

    /// Take next root in the following order:
    /// - used roots
    /// - pending roots
    /// - receiver
    async fn take_next_root(&mut self) -> Option<IndexedInteropRoot> {
        if let Some(root) = self.used_roots.pop_front() {
            Some(root)
        } else if let Some(root) = self.pending_roots.pop_front() {
            Some(root)
        } else {
            self.receiver.recv().await
        }
    }

    /// Cleans up the stream and removes all roots that were sent in transactions
    /// Returns the last log index of executed interop root
    pub async fn on_canonical_state_change(
        &mut self,
        txs: Vec<InteropRootsEnvelope>,
    ) -> Option<InteropRootsLogIndex> {
        let mut log_index = None;
        for tx in txs {
            let mut roots = Vec::new();
            for _ in 0..tx.interop_roots_count() {
                roots.push(self.take_next_root().await.unwrap());
            }

            let envelope = InteropRootsEnvelope::from_interop_roots(
                roots.iter().map(|r| r.root.clone()).collect(),
            );
            log_index = Some(roots.last().unwrap().log_index.clone());

            assert_eq!(&envelope, &tx);
        }

        assert!(self.pending_roots.is_empty());

        // Clear used roots that were left in the buffer and move them to pending
        self.pending_roots.extend(self.used_roots.drain(..));

        log_index
    }
}
