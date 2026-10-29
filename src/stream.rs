//! Streaming plumbing shared by providers, the engine, the daemon (SSE) and the UIs.

use crate::engine::ChatResponse;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};

pub type Sink = mpsc::UnboundedSender<StreamEvent>;

/// One thing that happened while a request was running. This is also the wire
/// format of `POST /v1/chat` with `"stream": true` (one JSON object per SSE `data:`).
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    /// A piece of the answer.
    Text { text: String },
    /// An agent tool call: state is "started", "completed" or "error".
    Tool { name: String, state: String, detail: String },
    /// The agent wants permission. Answer with `ApprovalHub::resolve` / `POST /v1/approvals/{id}`.
    Approval { id: String, summary: String },
    /// Informational progress line.
    Status { text: String },
    /// Final result (sent by `drive`, never by providers).
    Done { response: ChatResponse },
    /// Final failure (sent by `drive`).
    Error { message: String },
}

/// Pending approval questions, keyed by an id the UI echoes back.
#[derive(Clone, Default)]
pub struct ApprovalHub {
    inner: Arc<Mutex<HashMap<String, oneshot::Sender<String>>>>,
}

impl ApprovalHub {
    pub fn register(&self, id: String) -> oneshot::Receiver<String> {
        let (tx, rx) = oneshot::channel();
        self.inner.lock().unwrap().insert(id, tx);
        rx
    }
    /// Returns false when nobody is waiting for that id (already answered / expired).
    pub fn resolve(&self, id: &str, choice: &str) -> bool {
        match self.inner.lock().unwrap().remove(id) {
            Some(tx) => tx.send(choice.to_string()).is_ok(),
            None => false,
        }
    }
    pub fn forget(&self, id: &str) {
        self.inner.lock().unwrap().remove(id);
    }
}

/// What a provider gets besides the request: where to send live events and
/// how to ask the user for approval. `sink == None` means nobody is watching.
#[derive(Clone)]
pub struct Ctx {
    pub sink: Option<Sink>,
    pub hub: ApprovalHub,
    emitted_text: Arc<AtomicBool>,
}

impl Ctx {
    pub fn new(sink: Option<Sink>, hub: ApprovalHub) -> Self {
        Self { sink, hub, emitted_text: Arc::new(AtomicBool::new(false)) }
    }
    pub fn emit(&self, ev: StreamEvent) {
        if matches!(ev, StreamEvent::Text { .. }) {
            self.emitted_text.store(true, Ordering::Relaxed);
        }
        if let Some(s) = &self.sink {
            let _ = s.send(ev);
        }
    }
    pub fn text(&self, t: &str) {
        if !t.is_empty() {
            self.emit(StreamEvent::Text { text: t.to_string() });
        }
    }
    pub fn interactive(&self) -> bool {
        self.sink.is_some()
    }
    pub fn emitted_text(&self) -> bool {
        self.emitted_text.load(Ordering::Relaxed)
    }
}

/// Run a request future and report its outcome as the final stream event.
/// Stops early (dropping the request, which cancels it) if the listener went away.
pub async fn drive<F: Future<Output = Result<ChatResponse>>>(fut: F, tx: Sink) {
    let watcher = tx.clone();
    tokio::select! {
        r = fut => {
            let ev = match r {
                Ok(response) => StreamEvent::Done { response },
                Err(e) => StreamEvent::Error { message: format!("{e:#}") },
            };
            let _ = tx.send(ev);
        }
        _ = watcher.closed() => {}
    }
}

/// Incremental Server-Sent-Events parser. Feed raw bytes, get `(event, data)` pairs.
pub struct Sse {
    buf: Vec<u8>,
}

impl Sse {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    pub fn push(&mut self, chunk: &[u8]) -> Vec<(String, String)> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some((pos, len)) = boundary(&self.buf) {
            let block: Vec<u8> = self.buf.drain(..pos + len).collect();
            let text = String::from_utf8_lossy(&block[..pos]).into_owned();
            let mut event = String::new();
            let mut data: Vec<String> = Vec::new();
            for line in text.lines() {
                if line.starts_with(':') {
                    continue; // comment / keepalive
                }
                if let Some(v) = line.strip_prefix("event:") {
                    event = v.trim().to_string();
                } else if let Some(v) = line.strip_prefix("data:") {
                    data.push(v.strip_prefix(' ').unwrap_or(v).to_string());
                }
            }
            if !event.is_empty() || !data.is_empty() {
                out.push((event, data.join("\n")));
            }
        }
        out
    }
}

fn boundary(buf: &[u8]) -> Option<(usize, usize)> {
    (0..buf.len()).find_map(|i| {
        if buf[i..].starts_with(b"\n\n") {
            Some((i, 2))
        } else if buf[i..].starts_with(b"\r\n\r\n") {
            Some((i, 4))
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_handles_split_chunks_crlf_and_comments() {
        let mut p = Sse::new();
        assert!(p.push(b"event: a\r\ndata: {\"x\"").is_empty());
        let got = p.push(b":1}\r\n\r\n: keepalive\n\ndata: second\ndata: line\n\n");
        assert_eq!(got.len(), 2);
        assert_eq!(got[0], ("a".to_string(), "{\"x\":1}".to_string()));
        assert_eq!(got[1], ("".to_string(), "second\nline".to_string()));
    }

    #[test]
    fn approval_hub_resolves_once() {
        let hub = ApprovalHub::default();
        let mut rx = hub.register("r:1".into());
        assert!(hub.resolve("r:1", "once"));
        assert!(!hub.resolve("r:1", "once"));
        assert_eq!(rx.try_recv().unwrap(), "once");
    }
}
