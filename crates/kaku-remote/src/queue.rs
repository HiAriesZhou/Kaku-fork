//! Keep bulk transfers behind interactive replies, without starving transfers.

use tokio::sync::mpsc;

pub(crate) struct Sender<T> {
    pub(crate) interactive: mpsc::Sender<T>,
    pub(crate) bulk: mpsc::Sender<T>,
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        Self {
            interactive: self.interactive.clone(),
            bulk: self.bulk.clone(),
        }
    }
}

impl<T> Sender<T> {
    pub(crate) async fn send(&self, value: T) -> Result<(), mpsc::error::SendError<T>> {
        self.interactive.send(value).await
    }

    pub(crate) async fn send_bulk(&self, value: T) -> Result<(), mpsc::error::SendError<T>> {
        self.bulk.send(value).await
    }
}

pub(crate) struct Receiver<T> {
    pub(crate) interactive: mpsc::Receiver<T>,
    pub(crate) bulk: mpsc::Receiver<T>,
    served: usize,
}

pub(crate) fn channel<T>(capacity: usize) -> (Sender<T>, Receiver<T>) {
    channel_with_capacity(capacity, capacity)
}

pub(crate) fn channel_with_capacity<T>(
    interactive: usize,
    bulk: usize,
) -> (Sender<T>, Receiver<T>) {
    let (interactive, incoming) = mpsc::channel(interactive);
    let (bulk, transfers) = mpsc::channel(bulk);
    (
        Sender { interactive, bulk },
        Receiver {
            interactive: incoming,
            bulk: transfers,
            served: 0,
        },
    )
}

impl<T> Receiver<T> {
    pub(crate) async fn recv(&mut self) -> Option<T> {
        // A sustained terminal flood must still let downloads make progress.
        if self.served >= 8 {
            if let Ok(value) = self.bulk.try_recv() {
                self.served = 0;
                return Some(value);
            }
        }
        tokio::select! {
            biased;
            Some(value) = self.interactive.recv() => {
                self.served = self.served.saturating_add(1);
                Some(value)
            }
            Some(value) = self.bulk.recv() => {
                self.served = 0;
                Some(value)
            }
            else => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn input_reply_overtakes_bulk_but_each_stream_keeps_its_order() {
        let (tx, mut rx) = channel(16);
        tx.send_bulk("file start").await.unwrap();
        tx.send_bulk("file chunk").await.unwrap();
        tx.send("key echo").await.unwrap();
        assert_eq!(rx.recv().await, Some("key echo"));
        assert_eq!(rx.recv().await, Some("file start"));
        assert_eq!(rx.recv().await, Some("file chunk"));
        drop(tx);
        assert_eq!(rx.recv().await, None);
    }

    #[tokio::test]
    async fn transfers_progress_during_a_terminal_flood() {
        let (tx, mut rx) = channel(16);
        for n in 0..16 {
            tx.send(n).await.unwrap();
        }
        tx.send_bulk(99).await.unwrap();
        for n in 0..8 {
            assert_eq!(rx.recv().await, Some(n));
        }
        assert_eq!(rx.recv().await, Some(99));
        assert_eq!(rx.recv().await, Some(8));
    }
}
