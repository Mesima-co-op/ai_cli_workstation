//! Model providers. Each turns a list of messages into text plus token usage,
//! emitting live events (text deltas, tool progress, approvals) through `Ctx`.

use crate::config::ProviderCfg;
use crate::stream::{Ctx, Sse, StreamEvent};
use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::io::AsyncWriteExt;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Msg {
    pub role: String, // "user" | "assistant"
    pub content: String,
}

pub struct ProviderReq {
    pub model: String,
    pub system: Option<String>,
    pub messages: Vec<Msg>,
    pub max_tokens: u32,
}

pub struct ProviderOut {
    pub text: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// True when the provider sent no usage and the numbers are a rough estimate.
    pub estimated: bool,
    /// Cost reported by the provider itself (claude-cli), if any.
    pub reported_cost: Option<f64>,
    /// "api" = pay per token, "subscription" = flat-rate plan.
    pub billing: &'static str,
}

#[async_trait]
pub trait Provider: Send + Sync {
    async fn complete(&self, req: &ProviderReq, ctx: &Ctx) -> Result<ProviderOut>;
}

pub fn build(name: &str, cfg: &ProviderCfg, http: &reqwest::Client) -> Result<Box<dyn Provider>> {
    if !cfg.enabled {
        bail!("provider '{name}' is disabled (enable it in the Config tab or config.toml)");
    }
    let key = if cfg.api_key_env.is_empty() { None } else { crate::secrets::get(&cfg.api_key_env) };
    // Hermes: the env var, or API_SERVER_KEY from Hermes' own .env when it runs on this machine.
    let need_hermes_key = || {
        crate::check::hermes_key(cfg).ok_or_else(|| {
            anyhow!(
                "no Hermes key for provider '{name}': run `aiws key set {0}` with Hermes' API_SERVER_KEY, or make sure API_SERVER_KEY is in Hermes' .env (Windows: %LOCALAPPDATA%\\hermes\\.env)",
                crate::config::env_label(&cfg.api_key_env)
            )
        })
    };
    match cfg.kind.as_str() {
        "anthropic" => {
            let key = key.ok_or_else(|| anyhow!("no API key for provider '{name}': run `aiws key set {0}` (or set that environment variable)", crate::config::env_label(&cfg.api_key_env)))?;
            Ok(Box::new(Anthropic { http: http.clone(), base: trim(&cfg.base_url, "https://api.anthropic.com"), key }))
        }
        "openai" => {
            if !cfg.api_key_env.is_empty() && key.is_none() {
                bail!("no API key for provider '{name}': run `aiws key set {0}` (or set that environment variable)", crate::config::env_label(&cfg.api_key_env));
            }
            Ok(Box::new(OpenAi { http: http.clone(), base: trim(&cfg.base_url, "https://api.openai.com/v1"), key }))
        }
        "hermes-api" => Ok(Box::new(OpenAi {
            http: http.clone(),
            base: trim(&cfg.base_url, "http://127.0.0.1:8642/v1"),
            key: Some(need_hermes_key()?),
        })),
        "hermes-runs" => Ok(Box::new(HermesRuns {
            http: http.clone(),
            base: trim(&cfg.base_url, "http://127.0.0.1:8642/v1"),
            key: need_hermes_key()?,
        })),
        "hermes-cli" => Ok(Box::new(HermesCli {
            command: if cfg.command.is_empty() { "hermes".into() } else { cfg.command.clone() },
        })),
        "claude-cli" => Ok(Box::new(ClaudeCli {
            command: if cfg.command.is_empty() { "claude".into() } else { cfg.command.clone() },
        })),
        other => bail!("unknown provider kind '{other}'"),
    }
}

fn trim(url: &str, default: &str) -> String {
    let u = if url.is_empty() { default } else { url };
    u.trim_end_matches('/').to_string()
}

fn clip(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

async fn check(resp: reqwest::Response) -> Result<Value> {
    let status = resp.status();
    let body = resp.text().await?;
    if !status.is_success() {
        bail!("HTTP {status}: {}", clip(&body, 400));
    }
    serde_json::from_str(&body).context("provider returned invalid JSON")
}

async fn ok(resp: reqwest::Response) -> Result<reqwest::Response> {
    if resp.status().is_success() {
        return Ok(resp);
    }
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    bail!("HTTP {status}: {}", clip(&body, 400))
}

fn est_tokens(chars: usize) -> u64 {
    (chars as u64).div_ceil(4)
}

fn first_str<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|k| v.get(*k).and_then(|x| x.as_str()))
}

// ------------------------------------------------------------------ Anthropic

struct Anthropic {
    http: reqwest::Client,
    base: String,
    key: String,
}

#[async_trait]
impl Provider for Anthropic {
    async fn complete(&self, req: &ProviderReq, ctx: &Ctx) -> Result<ProviderOut> {
        let mut body = json!({
            "model": req.model,
            "max_tokens": req.max_tokens,
            "messages": req.messages,
            "stream": true,
        });
        if let Some(s) = &req.system {
            body["system"] = json!(s);
        }
        let resp = self
            .http
            .post(format!("{}/v1/messages", self.base))
            .header("x-api-key", &self.key)
            .header("anthropic-version", "2023-06-01")
            .json(&body)
            .send()
            .await?;
        let resp = ok(resp).await?;

        let (mut text, mut input, mut output) = (String::new(), 0u64, 0u64);
        let mut sse = Sse::new();
        let mut bytes = resp.bytes_stream();
        while let Some(chunk) = bytes.next().await {
            for (ev, data) in sse.push(&chunk?) {
                let Ok(v) = serde_json::from_str::<Value>(&data) else { continue };
                match v["type"].as_str().unwrap_or(&ev) {
                    "message_start" => {
                        let u = &v["message"]["usage"];
                        input = u["input_tokens"].as_u64().unwrap_or(0)
                            + u["cache_creation_input_tokens"].as_u64().unwrap_or(0)
                            + u["cache_read_input_tokens"].as_u64().unwrap_or(0);
                    }
                    "content_block_delta" => {
                        if let Some(t) = v["delta"]["text"].as_str() {
                            text.push_str(t);
                            ctx.text(t);
                        }
                    }
                    "message_delta" => output = v["usage"]["output_tokens"].as_u64().unwrap_or(output),
                    "error" => bail!("Anthropic stream error: {}", v["error"]["message"].as_str().unwrap_or("unknown")),
                    _ => {}
                }
            }
        }
        Ok(ProviderOut { text, input_tokens: input, output_tokens: output, estimated: false, reported_cost: None, billing: "api" })
    }
}

// ------------------------------------------------------- OpenAI-compatible

/// Also serves `hermes-api` (Hermes' OpenAI-compatible endpoint), whose
/// `hermes.tool.progress` events show tool activity.
struct OpenAi {
    http: reqwest::Client,
    base: String,
    key: Option<String>,
}

#[async_trait]
impl Provider for OpenAi {
    async fn complete(&self, req: &ProviderReq, ctx: &Ctx) -> Result<ProviderOut> {
        let mut msgs: Vec<Value> = Vec::new();
        if let Some(s) = &req.system {
            msgs.push(json!({"role": "system", "content": s}));
        }
        for m in &req.messages {
            msgs.push(json!({"role": m.role, "content": m.content}));
        }
        let body = json!({
            "model": req.model,
            "messages": msgs,
            "max_tokens": req.max_tokens,
            "stream": true,
            "stream_options": {"include_usage": true},
        });
        let mut rb = self.http.post(format!("{}/chat/completions", self.base)).json(&body);
        if let Some(k) = &self.key {
            rb = rb.bearer_auth(k);
        }
        let resp = ok(rb.send().await?).await?;

        let (mut text, mut input, mut output) = (String::new(), 0u64, 0u64);
        let mut sse = Sse::new();
        let mut bytes = resp.bytes_stream();
        'outer: while let Some(chunk) = bytes.next().await {
            for (ev, data) in sse.push(&chunk?) {
                if data.trim() == "[DONE]" {
                    break 'outer;
                }
                let Ok(v) = serde_json::from_str::<Value>(&data) else { continue };
                if ev == "hermes.tool.progress" {
                    let name = first_str(&v, &["tool", "name", "tool_name"]).unwrap_or("tool").to_string();
                    let detail = first_str(&v, &["preview", "label", "message", "emoji"]).map(String::from).unwrap_or_else(|| clip(&v.to_string(), 120));
                    ctx.emit(StreamEvent::Tool { name, state: "started".into(), detail });
                    continue;
                }
                if let Some(msg) = v["error"]["message"].as_str() {
                    bail!("stream error: {msg}");
                }
                if let Some(t) = v["choices"][0]["delta"]["content"].as_str() {
                    text.push_str(t);
                    ctx.text(t);
                }
                if v["usage"].is_object() {
                    input = v["usage"]["prompt_tokens"].as_u64().unwrap_or(input);
                    output = v["usage"]["completion_tokens"].as_u64().unwrap_or(output);
                }
            }
        }
        let estimated = input == 0 && output == 0 && !text.is_empty();
        if estimated {
            input = est_tokens(req.messages.iter().map(|m| m.content.chars().count()).sum());
            output = est_tokens(text.chars().count());
        }
        Ok(ProviderOut { text, input_tokens: input, output_tokens: output, estimated, reported_cost: None, billing: "api" })
    }
}

// ------------------------------------------------- Hermes /v1/runs (agent runs)

/// Starts a run and follows its event stream: live text, tool progress and
/// approval requests (answered through `Ctx::hub`; denied when nobody can answer).
struct HermesRuns {
    http: reqwest::Client,
    base: String,
    key: String,
}

/// Asks Hermes to stop the run if we stop listening before it finished.
struct StopGuard {
    http: reqwest::Client,
    url: String,
    key: String,
    armed: bool,
}
impl Drop for StopGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Ok(h) = tokio::runtime::Handle::try_current() {
            let (http, url, key) = (self.http.clone(), self.url.clone(), self.key.clone());
            h.spawn(async move {
                let _ = http.post(url).bearer_auth(key).send().await;
            });
        }
    }
}

fn id_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Payloads may be a bare string or an object; pull out the visible text.
fn payload_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Object(_) => {
            for k in ["delta", "text", "content", "token"] {
                match v.get(k) {
                    Some(Value::String(s)) => return s.clone(),
                    Some(o @ Value::Object(_)) => {
                        let inner = payload_text(o);
                        if !inner.is_empty() {
                            return inner;
                        }
                    }
                    _ => {}
                }
            }
            String::new()
        }
        _ => String::new(),
    }
}

fn run_output(v: &Value) -> String {
    match &v["output"] {
        Value::String(s) => s.clone(),
        o @ Value::Object(_) => payload_text(o),
        Value::Array(a) => a.iter().map(payload_text).collect::<Vec<_>>().join(""),
        _ => String::new(),
    }
}

fn usage_from(v: &Value) -> (u64, u64) {
    let u = if v["usage"].is_object() { &v["usage"] } else { v };
    (
        u["input_tokens"].as_u64().or(u["prompt_tokens"].as_u64()).unwrap_or(0),
        u["output_tokens"].as_u64().or(u["completion_tokens"].as_u64()).unwrap_or(0),
    )
}

#[async_trait]
impl Provider for HermesRuns {
    async fn complete(&self, req: &ProviderReq, ctx: &Ctx) -> Result<ProviderOut> {
        let (last, earlier) = req.messages.split_last().ok_or_else(|| anyhow!("no messages"))?;
        let mut body = json!({ "input": last.content });
        if !earlier.is_empty() {
            body["conversation_history"] = json!(earlier);
        }
        if let Some(s) = &req.system {
            body["instructions"] = json!(s);
        }
        if !req.model.is_empty() && req.model != "hermes-agent" {
            body["model"] = json!(req.model);
        }
        let started = check(self.http.post(format!("{}/runs", self.base)).bearer_auth(&self.key).json(&body).send().await?).await?;
        let run_id = started["run_id"].as_str().ok_or_else(|| anyhow!("Hermes did not return a run_id: {}", clip(&started.to_string(), 200)))?.to_string();
        ctx.emit(StreamEvent::Status { text: format!("Hermes run {run_id} started") });

        let mut guard = StopGuard {
            http: self.http.clone(),
            url: format!("{}/runs/{run_id}/stop", self.base),
            key: self.key.clone(),
            armed: true,
        };
        let resp = self
            .http
            .get(format!("{}/runs/{run_id}/events", self.base))
            .bearer_auth(&self.key)
            .header("Accept", "text/event-stream")
            .send()
            .await?;
        let resp = ok(resp).await?;

        let (mut text, mut input, mut output) = (String::new(), 0u64, 0u64);
        let mut finished = false;
        let mut sse = Sse::new();
        let mut bytes = resp.bytes_stream();
        'outer: while let Some(chunk) = bytes.next().await {
            for (ev, data) in sse.push(&chunk?) {
                let v: Value = serde_json::from_str(&data).unwrap_or_else(|_| Value::String(data.clone()));
                let name = if ev.is_empty() { first_str(&v, &["type", "event"]).unwrap_or("").to_string() } else { ev };
                match name.as_str() {
                    "message.delta" => {
                        let t = payload_text(&v);
                        text.push_str(&t);
                        ctx.text(&t);
                    }
                    "message.interim" => {
                        if !v["already_streamed"].as_bool().unwrap_or(false) {
                            let t = payload_text(&v);
                            if !t.is_empty() {
                                let t = format!("{t}\n");
                                text.push_str(&t);
                                ctx.text(&t);
                            }
                        }
                    }
                    "tool.started" => ctx.emit(StreamEvent::Tool {
                        name: first_str(&v, &["tool", "name"]).unwrap_or("tool").into(),
                        state: "started".into(),
                        detail: first_str(&v, &["preview"]).unwrap_or("").into(),
                    }),
                    "tool.completed" => {
                        let failed = v["error"].as_bool().unwrap_or(false) || (v["error"].is_string() && !v["error"].as_str().unwrap_or("").is_empty());
                        let secs = v["duration"].as_f64().map(|d| format!("{d:.1}s ")).unwrap_or_default();
                        ctx.emit(StreamEvent::Tool {
                            name: first_str(&v, &["tool", "name"]).unwrap_or("tool").into(),
                            state: if failed { "error" } else { "completed" }.into(),
                            detail: format!("{secs}{}", first_str(&v, &["preview"]).unwrap_or("")),
                        });
                    }
                    "subagent.start" | "subagent.complete" => ctx.emit(StreamEvent::Status {
                        text: format!("{name}: {}", first_str(&v, &["summary", "status", "goal", "task"]).unwrap_or("")),
                    }),
                    "approval.request" | "approval.requested" => {
                        let req_id = ["request_id", "prompt_id", "id"].iter().find_map(|k| v.get(*k).and_then(id_string));
                        let summary = first_str(&v, &["description", "command", "preview", "message", "tool"])
                            .map(String::from)
                            .unwrap_or_else(|| clip(&v.to_string(), 300));
                        let Some(req_id) = req_id else {
                            ctx.emit(StreamEvent::Status { text: format!("approval requested but no request_id in payload: {}", clip(&v.to_string(), 200)) });
                            continue;
                        };
                        let key = format!("{run_id}:{req_id}");
                        let choice = if ctx.interactive() {
                            let rx = ctx.hub.register(key.clone());
                            ctx.emit(StreamEvent::Approval { id: key.clone(), summary });
                            match tokio::time::timeout(Duration::from_secs(300), rx).await {
                                Ok(Ok(c)) if c == "once" => "once",
                                _ => {
                                    ctx.hub.forget(&key);
                                    "deny"
                                }
                            }
                        } else {
                            ctx.emit(StreamEvent::Status { text: "approval needed but no one is watching: denied".into() });
                            "deny"
                        };
                        let mut ab = json!({ "request_id": v.get("request_id").or(v.get("prompt_id")).or(v.get("id")).cloned().unwrap_or(json!(req_id)), "choice": choice });
                        if let Some(g) = v.get("execution_generation") {
                            ab["execution_generation"] = g.clone();
                        }
                        let r = self.http.post(format!("{}/runs/{run_id}/approval", self.base)).bearer_auth(&self.key).json(&ab).send().await?;
                        if !r.status().is_success() {
                            ctx.emit(StreamEvent::Status { text: format!("approval reply rejected: HTTP {}", r.status()) });
                        } else {
                            ctx.emit(StreamEvent::Status { text: format!("approval answered: {choice}") });
                        }
                    }
                    "run.completed" => {
                        (input, output) = usage_from(&v);
                        if text.is_empty() {
                            text = run_output(&v);
                            ctx.text(&text);
                        }
                        finished = true;
                        guard.armed = false;
                        break 'outer;
                    }
                    "run.failed" => bail!("Hermes run failed: {}", clip(first_str(&v, &["error", "message"]).unwrap_or(&v.to_string()), 400)),
                    "run.cancelled" => bail!("Hermes run was cancelled"),
                    "run.interrupted" => bail!("Hermes run was interrupted (gateway shut down?)"),
                    _ => {}
                }
            }
        }

        // The event stream may end (or omit usage); ask for the run's final state.
        let status = check(self.http.get(format!("{}/runs/{run_id}", self.base)).bearer_auth(&self.key).send().await?).await.ok();
        if let Some(st) = &status {
            if !finished {
                match st["status"].as_str().unwrap_or("") {
                    "completed" => guard.armed = false,
                    other => bail!("Hermes event stream ended while the run was '{other}'"),
                }
            }
            if input == 0 && output == 0 {
                (input, output) = usage_from(st);
            }
            if text.is_empty() {
                text = run_output(st);
                ctx.text(&text);
            }
        } else if !finished {
            bail!("Hermes event stream ended before the run finished");
        }
        guard.armed = false;
        let estimated = input == 0 && output == 0 && !text.is_empty();
        if estimated {
            input = est_tokens(req.messages.iter().map(|m| m.content.chars().count()).sum());
            output = est_tokens(text.chars().count());
        }
        Ok(ProviderOut { text, input_tokens: input, output_tokens: output, estimated, reported_cost: None, billing: "api" })
    }
}

// --------------------------------------------------- Claude Code (subscription)

/// Drives the local `claude` CLI in print mode, so usage is billed to the
/// user's Claude subscription instead of an API key. (Not token-streamed:
/// the answer arrives in one piece.)
struct ClaudeCli {
    command: String,
}

#[async_trait]
impl Provider for ClaudeCli {
    async fn complete(&self, req: &ProviderReq, _ctx: &Ctx) -> Result<ProviderOut> {
        let mut prompt = String::new();
        if req.messages.len() == 1 {
            prompt.push_str(&req.messages[0].content);
        } else {
            for m in &req.messages {
                prompt.push_str(&format!("{}: {}\n\n", m.role.to_uppercase(), m.content));
            }
        }
        let mut cmd = tokio::process::Command::new(crate::check::which(&self.command).unwrap_or_else(|| self.command.clone().into()));
        cmd.arg("-p").arg("--output-format").arg("json");
        if !req.model.is_empty() {
            cmd.arg("--model").arg(&req.model);
        }
        if let Some(s) = &req.system {
            cmd.arg("--append-system-prompt").arg(s);
        }
        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd
            .spawn()
            .with_context(|| format!("could not start '{}': is Claude Code installed and on PATH?", self.command))?;
        // Prompt goes over stdin so long prompts never hit command-line limits.
        let mut stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
        stdin.write_all(prompt.as_bytes()).await?;
        drop(stdin);
        let out = child.wait_with_output().await?;
        if !out.status.success() && out.stdout.is_empty() {
            let err = String::from_utf8_lossy(&out.stderr);
            bail!("claude exited with {}: {}", out.status, clip(&err, 400));
        }
        let v: Value = serde_json::from_slice(&out.stdout).context("claude returned non-JSON output")?;
        if v["is_error"].as_bool().unwrap_or(false) {
            bail!("claude error: {}", v["result"].as_str().unwrap_or("unknown"));
        }
        let usage = &v["usage"];
        let input = usage["input_tokens"].as_u64().unwrap_or(0)
            + usage["cache_creation_input_tokens"].as_u64().unwrap_or(0)
            + usage["cache_read_input_tokens"].as_u64().unwrap_or(0);
        Ok(ProviderOut {
            text: v["result"].as_str().unwrap_or_default().to_string(),
            input_tokens: input,
            output_tokens: usage["output_tokens"].as_u64().unwrap_or(0),
            estimated: false,
            reported_cost: v["total_cost_usd"].as_f64(),
            billing: "subscription",
        })
    }
}

// ------------------------------------------------------------ Hermes (local CLI)

/// Runs `hermes -z <prompt>` (one-shot, final answer only on stdout) and reads
/// token/cost figures from `--usage-file` when Hermes writes them.
struct HermesCli {
    command: String,
}

fn find_num(v: &Value, keys: &[&str]) -> Option<f64> {
    match v {
        Value::Object(m) => {
            for k in keys {
                if let Some(n) = m.get(*k).and_then(|x| x.as_f64()) {
                    return Some(n);
                }
            }
            m.values().find_map(|c| find_num(c, keys))
        }
        Value::Array(a) => a.iter().find_map(|c| find_num(c, keys)),
        _ => None,
    }
}

#[async_trait]
impl Provider for HermesCli {
    async fn complete(&self, req: &ProviderReq, _ctx: &Ctx) -> Result<ProviderOut> {
        let mut prompt = String::new();
        if let Some(s) = &req.system {
            prompt.push_str(&format!("SYSTEM: {s}\n\n"));
        }
        if req.messages.len() == 1 {
            prompt.push_str(&req.messages[0].content);
        } else {
            for m in &req.messages {
                prompt.push_str(&format!("{}: {}\n\n", m.role.to_uppercase(), m.content));
            }
        }
        if prompt.len() > 28_000 {
            bail!("prompt too long for the hermes CLI argument limit; use a hermes-api or hermes-runs provider for long inputs");
        }
        let usage_path = std::env::temp_dir().join(format!(
            "aiws-hermes-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
        ));
        let mut cmd = tokio::process::Command::new(crate::check::which(&self.command).unwrap_or_else(|| self.command.clone().into()));
        cmd.arg("-z").arg(&prompt);
        if !req.model.is_empty() {
            cmd.arg("-m").arg(&req.model);
        }
        cmd.arg("--usage-file").arg(&usage_path);
        cmd.stdin(std::process::Stdio::null()).kill_on_drop(true);
        let out = cmd
            .output()
            .await
            .with_context(|| format!("could not start '{}': is Hermes Agent installed and on PATH?", self.command))?;
        let usage: Option<Value> = std::fs::read(&usage_path).ok().and_then(|b| serde_json::from_slice(&b).ok());
        let _ = std::fs::remove_file(&usage_path);
        let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            bail!("hermes exited with {}: {}", out.status, clip(&err, 400));
        }
        if text.is_empty() {
            bail!("hermes returned no text");
        }
        let (input, output, cost) = match &usage {
            Some(u) => (
                find_num(u, &["input_tokens", "prompt_tokens"]).unwrap_or(0.0) as u64,
                find_num(u, &["output_tokens", "completion_tokens"]).unwrap_or(0.0) as u64,
                find_num(u, &["cost_usd", "total_cost_usd", "estimated_cost_usd", "cost"]),
            ),
            None => (0, 0, None),
        };
        Ok(ProviderOut { text, input_tokens: input, output_tokens: output, estimated: false, reported_cost: cost, billing: "api" })
    }
}
