//! A `Source` is where the TUI/CLI get their data: the local engine, or a
//! remote `aiws serve` daemon (another instance, a container, a VPS).

use crate::config::Config;
use crate::engine::{ChatRequest, ChatResponse, Engine};
use crate::store::Snapshot;
use crate::stream::{Sink, Sse, StreamEvent};
use anyhow::{Result, anyhow};
use futures::StreamExt;
use serde::de::DeserializeOwned;

#[derive(Clone)]
pub enum Source {
    Local(Engine),
    Remote { http: reqwest::Client, base: String, token: Option<String> },
}

impl Source {
    pub fn remote(url: &str, token: Option<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(15))
            .read_timeout(std::time::Duration::from_secs(300))
            .build()?;
        Ok(Source::Remote { http, base: url.trim_end_matches('/').to_string(), token })
    }

    pub fn label(&self) -> String {
        match self {
            Source::Local(_) => "local".into(),
            Source::Remote { base, .. } => base.clone(),
        }
    }

    pub fn is_local(&self) -> bool {
        matches!(self, Source::Local(_))
    }

    async fn get<T: DeserializeOwned>(http: &reqwest::Client, base: &str, token: &Option<String>, path: &str) -> Result<T> {
        let mut rb = http.get(format!("{base}{path}"));
        if let Some(t) = token {
            rb = rb.bearer_auth(t);
        }
        decode(rb.send().await?).await
    }

    pub async fn snapshot(&self) -> Result<Snapshot> {
        match self {
            Source::Local(e) => e.snapshot(),
            Source::Remote { http, base, token } => Self::get(http, base, token, "/v1/snapshot").await,
        }
    }

    /// Run a chat request, sending live events to `tx`. The returned value is the
    /// final result; callers that want it as an event use `stream::drive`.
    pub async fn chat_stream(&self, mut req: ChatRequest, tx: Sink) -> Result<ChatResponse> {
        match self {
            Source::Local(e) => e.chat_stream(req, tx).await,
            Source::Remote { http, base, token } => {
                req.stream = true;
                let mut rb = http.post(format!("{base}/v1/chat")).json(&req);
                if let Some(t) = token {
                    rb = rb.bearer_auth(t);
                }
                let resp = rb.send().await?;
                if !resp.status().is_success() {
                    return decode::<ChatResponse>(resp).await;
                }
                let mut sse = Sse::new();
                let mut bytes = resp.bytes_stream();
                while let Some(chunk) = bytes.next().await {
                    for (_ev, data) in sse.push(&chunk?) {
                        match serde_json::from_str::<StreamEvent>(&data) {
                            Ok(StreamEvent::Done { response }) => return Ok(response),
                            Ok(StreamEvent::Error { message }) => return Err(anyhow!(message)),
                            Ok(ev) => {
                                let _ = tx.send(ev);
                            }
                            Err(_) => {}
                        }
                    }
                }
                Err(anyhow!("stream ended without a result"))
            }
        }
    }

    /// Answer an agent approval prompt ("once" or "deny").
    pub async fn approve(&self, id: &str, choice: &str) -> Result<()> {
        match self {
            Source::Local(e) => {
                e.resolve_approval(id, choice);
                Ok(())
            }
            Source::Remote { http, base, token } => {
                let mut rb = http.post(format!("{base}/v1/approvals/{id}")).json(&serde_json::json!({ "choice": choice }));
                if let Some(t) = token {
                    rb = rb.bearer_auth(t);
                }
                decode::<serde_json::Value>(rb.send().await?).await.map(|_| ())
            }
        }
    }

    pub async fn config(&self) -> Result<Config> {
        match self {
            Source::Local(e) => Ok(e.config()),
            Source::Remote { http, base, token } => Self::get(http, base, token, "/v1/config").await,
        }
    }

    pub async fn save_config(&self, cfg: Config) -> Result<()> {
        match self {
            Source::Local(e) => e.save_config(&cfg),
            Source::Remote { http, base, token } => {
                let mut rb = http.put(format!("{base}/v1/config")).json(&cfg);
                if let Some(t) = token {
                    rb = rb.bearer_auth(t);
                }
                decode::<Config>(rb.send().await?).await.map(|_| ())
            }
        }
    }
}

async fn decode<T: DeserializeOwned>(resp: reqwest::Response) -> Result<T> {
    let status = resp.status();
    let body = resp.text().await?;
    if !status.is_success() {
        let msg = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v["error"].as_str().map(String::from))
            .unwrap_or(body);
        return Err(anyhow!("{status}: {msg}"));
    }
    Ok(serde_json::from_str(&body)?)
}
