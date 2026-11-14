//! Dedicated store-writer task: the single owner of a `SessionFile`
//! (ADR 0007's crash-tolerant JSONL) driven by `mpsc` commands.
//!
//! Why a task: prompt handling must never block on disk (the ACP pump runs
//! on the tokio runtime; `write_all` on a std `File` is synchronous), and
//! one owning task keeps appends strictly sequential without a mutex around
//! the file. Fire-and-forget `append` is the default for the turn loop;
//! `append_acked` exists for callers that must know the record hit the disk
//! (the doctor, future resume paths). The channel buffer absorbs bursts;
//! under sustained backpressure `append` degrades to queueing — the acked
//! variant is the honest seam for durability-critical callers.

use tokio::sync::{mpsc, oneshot};

use super::{SessionFile, StoreError};
use crate::session::TranscriptRecord;

/// One command to the writer task: append this record, optionally
/// reporting the write outcome back through `ack`.
#[derive(Debug)]
pub enum StoreCmd {
    Append {
        record: TranscriptRecord,
        ack: Option<oneshot::Sender<Result<(), StoreError>>>,
    },
}

/// Cloneable handle to a session's writer task.
#[derive(Debug, Clone)]
pub struct WriterHandle {
    tx: mpsc::Sender<StoreCmd>,
}

impl WriterHandle {
    /// Fire-and-forget append: queued in order, no completion signal.
    /// `Err` when the writer task is gone (closed) or its queue is full
    /// (backpressure — callers with durability needs use `append_acked`).
    pub fn append(&self, record: &TranscriptRecord) -> Result<(), StoreError> {
        self.tx
            .try_send(StoreCmd::Append {
                record: record.clone(),
                ack: None,
            })
            .map_err(|e| match e {
                mpsc::error::TrySendError::Closed(_) => StoreError::Io(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "store writer task is gone",
                )),
                mpsc::error::TrySendError::Full(_) => StoreError::Io(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "store writer queue is full",
                )),
            })
    }

    /// Append whose completion is reported: `Ok` once the record is on
    /// disk, `Err(StoreError)` on serialize/write failure, and an
    /// `Err(SendError)`-wrapped drop when the writer task is gone.
    pub async fn append_acked(&self, record: &TranscriptRecord) -> Result<(), StoreError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(StoreCmd::Append {
                record: record.clone(),
                ack: Some(tx),
            })
            .await
            .map_err(|_| {
                StoreError::Io(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "store writer task is gone",
                ))
            })?;
        rx.await.map_err(|_| {
            StoreError::Io(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "store writer task dropped the ack",
            ))
        })?
    }
}

impl SessionFile {
    /// Consumes the file into a dedicated writer task and returns a
    /// handle. The task owns the `SessionFile` for the rest of its life:
    /// appends stay strictly sequential (single owner), and the blocking
    /// `write_all` runs inside the task, never on the caller's thread.
    pub fn spawn_writer(self) -> WriterHandle {
        // Enough slack for a whole turn's records (stream deltas are not
        // persisted; messages, tool calls/results, usage are — tens per
        // turn, not thousands); `append_acked` awaits, so it cannot
        // overflow by construction.
        const CHANNEL_BUFFER: usize = 256;
        let (tx, mut rx) = mpsc::channel(CHANNEL_BUFFER);
        // Single owning task: appends stay strictly sequential without a
        // lock, and the blocking write runs off the caller's thread.
        // std::fs from an async task is bounded here (one JSON line per
        // command); see module comment.
        let mut file = self;
        tokio::task::spawn(async move {
            while let Some(cmd) = rx.recv().await {
                let StoreCmd::Append { record, ack } = cmd;
                let result = file.append(&record);
                if let Some(ack) = ack {
                    // The receiver may have hung up; that must not kill
                    // the writer.
                    let _ = ack.send(result);
                }
            }
            // `file` drops here, closing the transcript handle.
        });
        WriterHandle { tx }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A task that died (panic, or the file handle force-closed) must not
    /// hang an `append_acked` caller: the closed channel surfaces as an
    /// error immediately.
    #[tokio::test]
    async fn acked_append_errors_when_receiver_is_gone() {
        let (tx, rx) = mpsc::channel::<StoreCmd>(4);
        drop(rx);
        let writer = WriterHandle { tx };
        let record = TranscriptRecord::Message(crate::llm::types::ChatMessage::user("x"));
        let result = writer.append_acked(&record).await;
        assert!(result.is_err(), "send to dead task must error: {result:?}");
    }
}
