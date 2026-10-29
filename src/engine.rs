//! The engine ties config, routing, budgets, providers and the metrics store
//! together. CLI, TUI and the HTTP daemon all go through `Engine::chat`.

use crate::config::{self, Config};
use crate::provider::{self, Msg, ProviderReq};
use crate::router;
use crate::stream::{ApprovalHub, Ctx, Sink, StreamEvent};
use crate::{check, jev};
use crate::store::{self, RequestRow, Snapshot, Store};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime};

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct ChatRequest {
    pub messages: Vec<Msg>,
    #[serde(default)]
    pub system: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub source: Option<String>,
    /// Daemon API only: reply as a Server-Sent-Events stream.
    #[serde(default)]
    pub stream: bool,
    /// Internal: explains why this provider was chosen (used by the pipeline).
    #[serde(skip)]
    pub note: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ChatResponse {
    pub id: i64,
    pub text: String,
    pub provider: String,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: Option<f64>,
    pub billing: String,
    pub latency_ms: u64,
    pub route_reason: String,
}

#[derive(Clone)]
pub struct Engine {
    pub config_path: PathBuf,
    cached: Arc<Mutex<(Config, Option<SystemTime>)>>,
    pub store: Store,
    http: reqwest::Client,
    hub: ApprovalHub,
}

impl Engine {
    pub fn new(config_path: PathBuf) -> Result<Self> {
        let cfg = config::load(&config_path)?;
        let mtime = mtime(&config_path);
        let store = Store::open(&config::db_path())?;
        let http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(15))
            .read_timeout(std::time::Duration::from_secs(300))
            .build()?;
        Ok(Self { config_path, cached: Arc::new(Mutex::new((cfg, mtime))), store, http, hub: ApprovalHub::default() })
    }

    /// Current config; transparently reloads when the file changed on disk, so a
    /// running daemon picks up edits made from the TUI or a text editor.
    pub fn config(&self) -> Config {
        let mut g = self.cached.lock().unwrap();
        let m = mtime(&self.config_path);
        if m != g.1 {
            match config::load(&self.config_path) {
                Ok(c) => *g = (c, m),
                Err(e) => eprintln!("aiws: ignoring invalid config change: {e:#}"),
            }
        }
        g.0.clone()
    }

    pub fn save_config(&self, cfg: &Config) -> Result<()> {
        config::save(&self.config_path, cfg)?;
        *self.cached.lock().unwrap() = (cfg.clone(), mtime(&self.config_path));
        Ok(())
    }

    pub fn snapshot(&self) -> Result<Snapshot> {
        let c = self.config();
        self.store.snapshot(c.budget.daily_usd, c.budget.monthly_usd, 200)
    }

    fn check_budget(&self, cfg: &Config) -> Result<()> {
        if cfg.budget.daily_usd > 0.0 {
            let t = self.store.totals_since(store::start_of_today_ms())?;
            if t.api_cost_usd >= cfg.budget.daily_usd {
                bail!("daily budget reached (${:.2} of ${:.2}); raise budget.daily_usd to continue", t.api_cost_usd, cfg.budget.daily_usd);
            }
        }
        if cfg.budget.monthly_usd > 0.0 {
            let t = self.store.totals_since(store::start_of_month_ms())?;
            if t.api_cost_usd >= cfg.budget.monthly_usd {
                bail!("monthly budget reached (${:.2} of ${:.2}); raise budget.monthly_usd to continue", t.api_cost_usd, cfg.budget.monthly_usd);
            }
        }
        Ok(())
    }

    /// Answer a pending agent approval ("once" allows it; anything else denies).
    pub fn resolve_approval(&self, id: &str, choice: &str) -> bool {
        self.hub.resolve(id, if choice == "once" { "once" } else { "deny" })
    }

    pub async fn chat(&self, req: ChatRequest) -> Result<ChatResponse> {
        self.run(req, None).await
    }

    /// Like `chat`, but live events (text, tool progress, approvals) go to `sink`.
    pub async fn chat_stream(&self, req: ChatRequest, sink: Sink) -> Result<ChatResponse> {
        self.run(req, Some(sink)).await
    }

    async fn run(&self, req: ChatRequest, sink: Option<Sink>) -> Result<ChatResponse> {
        let ctx = Ctx::new(sink, self.hub.clone());
        if req.messages.is_empty() {
            bail!("no messages");
        }
        let cfg = self.config();
        // An explicit provider always wins; otherwise the pipeline (if on) decides.
        if req.provider.is_none() && cfg.pipeline.enabled {
            return self.pipeline(req, ctx, &cfg).await;
        }
        self.single(req, ctx).await
    }

    /// decide (Jev or heuristic) -> plan (Hermes) -> execute (Claude), where every
    /// stage falls back to what is available and the request still gets answered.
    async fn pipeline(&self, req: ChatRequest, ctx: Ctx, cfg: &Config) -> Result<ChatResponse> {
        let v = jev::decide(cfg, &self.http, req.system.as_deref(), &req.messages).await;
        let kind = if v.complex { "complex" } else { "simple" };
        let mut flags = String::new();
        if v.needs_plan {
            flags.push_str(" + needs plan");
        }
        if v.risky {
            flags.push_str(" + risky");
        }
        let note = if v.note.is_empty() { String::new() } else { format!(" ({})", v.note) };
        ctx.emit(StreamEvent::Status { text: format!("decide [{}]: {kind}{flags}{note}", v.source) });

        // Executors: the configured chain, then (optionally) anything else that works.
        let (mut execs, skipped) = check::ready_chain(cfg, &cfg.pipeline.executor);
        for (n, why) in skipped.iter().filter(|(_, w)| w != "disabled") {
            ctx.emit(StreamEvent::Status { text: format!("executor {n} skipped: {why}") });
        }
        if cfg.pipeline.fallback_any {
            for n in check::any_ready(cfg, &cfg.pipeline.executor) {
                if !execs.contains(&n) {
                    execs.push(n);
                }
            }
        }
        if execs.is_empty() {
            bail!("nothing is set up to answer this: no executor is ready. Run `aiws check` to see what to install or configure.");
        }

        // Planning only for requests that need it, and only if someone can do it.
        let mut plan: Option<String> = None;
        let mut planner_used = String::from("-");
        if v.needs_plan {
            let (planners, pskipped) = check::ready_chain(cfg, &cfg.pipeline.planner);
            for (n, why) in pskipped.iter().filter(|(_, w)| w != "disabled") {
                ctx.emit(StreamEvent::Status { text: format!("planner {n} skipped: {why}") });
            }
            for name in planners {
                ctx.emit(StreamEvent::Status { text: format!("planning with {name}") });
                match self.plan_with(&name, &req, cfg, &ctx).await {
                    Ok(t) if !t.trim().is_empty() => {
                        for line in t.lines().filter(|l| !l.trim().is_empty()).take(10) {
                            ctx.emit(StreamEvent::Status { text: format!("plan| {line}") });
                        }
                        plan = Some(t);
                        planner_used = name;
                        break;
                    }
                    Ok(_) => ctx.emit(StreamEvent::Status { text: format!("planner {name} returned nothing; trying next") }),
                    Err(e) => ctx.emit(StreamEvent::Status { text: format!("planner {name} failed: {e:#}; trying next") }),
                }
            }
            if plan.is_none() {
                planner_used = "inline".into();
                ctx.emit(StreamEvent::Status { text: "no planner available: the executor will outline its approach itself".into() });
            }
        }

        let mut errors: Vec<String> = Vec::new();
        for name in execs {
            let Some(pcfg) = cfg.providers.get(&name) else { continue };
            let mut system = req.system.clone().unwrap_or_default();
            if let Some(p) = &plan {
                system.push_str(&format!("\n\nA planning step prepared this execution plan. Follow it unless it is clearly wrong, and say so if you deviate:\n{p}"));
            } else if v.needs_plan {
                system.push_str("\n\nThis is a multi-step task: briefly outline your approach, then carry it out.");
            }
            if v.risky {
                system.push_str("\n\nThis request may be destructive. Confirm before any irreversible action.");
            }
            // The simple/complex model aliases only make sense for Claude Code.
            let model = req.model.clone().or_else(|| {
                (pcfg.kind == "claude-cli").then(|| if v.complex { cfg.pipeline.complex_model.clone() } else { cfg.pipeline.simple_model.clone() })
            });
            let sreq = ChatRequest {
                provider: Some(name.clone()),
                model: model.filter(|m| !m.is_empty()),
                system: Some(system.trim().to_string()).filter(|s| !s.is_empty()),
                source: Some(format!("{}+pipeline", req.source.clone().unwrap_or_else(|| "api".into()))),
                note: Some(format!("pipeline: {}:{kind} > plan:{planner_used} > run:{name}", v.source)),
                ..req.clone()
            };
            ctx.emit(StreamEvent::Status { text: format!("executing with {name}") });
            match self.single(sreq, ctx.clone()).await {
                Ok(r) => return Ok(r),
                Err(e) => {
                    errors.push(format!("{name}: {e:#}"));
                    if ctx.emitted_text() {
                        break; // part of an answer is already on screen; do not mix in a second one
                    }
                    ctx.emit(StreamEvent::Status { text: format!("{name} failed: {e:#}; trying next") });
                }
            }
        }
        bail!("no provider could answer:\n  {}", errors.join("\n  "))
    }

    /// Ask one provider for an execution plan. Its answer text is not shown as the
    /// reply; tool progress and approvals still reach the user.
    async fn plan_with(&self, name: &str, req: &ChatRequest, cfg: &Config, parent: &Ctx) -> Result<String> {
        const PLANNER_SYS: &str = "You are the planning stage of a multi-model pipeline. Produce a concise numbered plan for how to carry out the user's request: the steps, which tools or files are involved, checks to run, and risks. Do NOT carry out the steps and do NOT modify anything. At most 12 lines.";
        let mut system = PLANNER_SYS.to_string();
        if let Some(s) = &req.system {
            system.push_str(&format!("\n\nContext from the caller's system prompt:\n{s}"));
        }
        let (pctx, fwd) = match &parent.sink {
            Some(outer) => {
                let (itx, mut irx) = tokio::sync::mpsc::unbounded_channel::<StreamEvent>();
                let outer = outer.clone();
                let fwd = tokio::spawn(async move {
                    while let Some(ev) = irx.recv().await {
                        if !matches!(ev, StreamEvent::Text { .. }) {
                            let _ = outer.send(ev);
                        }
                    }
                });
                (Ctx::new(Some(itx), self.hub.clone()), Some(fwd))
            }
            None => (Ctx::new(None, self.hub.clone()), None),
        };
        let sreq = ChatRequest {
            messages: req.messages.clone(),
            system: Some(system),
            provider: Some(name.to_string()),
            max_tokens: Some(1024),
            source: Some(format!("{}+pipeline", req.source.clone().unwrap_or_else(|| "api".into()))),
            note: Some(format!("pipeline: plan via {name}")),
            ..Default::default()
        };
        let _ = cfg;
        let r = self.single(sreq, pctx).await;
        if let Some(f) = fwd {
            let _ = f.await; // all planner events are delivered before the executor starts
        }
        r.map(|x| x.text)
    }

    async fn single(&self, req: ChatRequest, ctx: Ctx) -> Result<ChatResponse> {
        let cfg = self.config();

        // 1. pick provider/model
        let (provider_name, mut model, reason) = match (&req.provider, cfg.router.enabled) {
            (Some(p), _) => (p.clone(), req.model.clone().unwrap_or_default(), req.note.clone().unwrap_or_else(|| "explicit provider".to_string())),
            (None, true) => {
                let verdict = jev::decide(&cfg, &self.http, req.system.as_deref(), &req.messages).await;
                let d = router::pick(&cfg, &verdict);
                let model = req.model.clone().unwrap_or(d.model);
                (d.provider, model, d.reason)
            }
            (None, false) => (cfg.default_provider.clone(), req.model.clone().unwrap_or_default(), "default provider".to_string()),
        };
        let pcfg = cfg
            .providers
            .get(&provider_name)
            .ok_or_else(|| anyhow::anyhow!("unknown provider '{provider_name}'"))?;
        if model.is_empty() {
            model = pcfg.default_model.clone();
        }

        let mut row = RequestRow {
            ts: chrono::Local::now().timestamp_millis(),
            provider: provider_name.clone(),
            model: if model.is_empty() { "(provider default)".into() } else { model.clone() },
            status: "ok".into(),
            billing: "api".into(),
            route_reason: reason.clone(),
            source: req.source.clone().unwrap_or_else(|| "api".into()),
            ..Default::default()
        };
        if cfg.storage.store_content {
            row.prompt = req.messages.last().map(|m| m.content.clone()).unwrap_or_default();
        }

        // 2. budget + provider construction; failures are logged too
        let started = Instant::now();
        let result = async {
            self.check_budget(&cfg)?;
            let p = provider::build(&provider_name, pcfg, &self.http)?;
            p.complete(&ProviderReq {
                model: model.clone(),
                system: req.system.clone(),
                messages: req.messages.clone(),
                max_tokens: req.max_tokens.unwrap_or(4096),
            }, &ctx)
            .await
        }
        .await;
        row.latency_ms = started.elapsed().as_millis() as u64;

        match result {
            Ok(out) => {
                row.input_tokens = out.input_tokens;
                row.output_tokens = out.output_tokens;
                row.billing = out.billing.into();
                row.cost_usd = out.reported_cost.or_else(|| {
                    cfg.price_for(&model).map(|p| {
                        (out.input_tokens as f64 * p.input_per_mtok + out.output_tokens as f64 * p.output_per_mtok) / 1e6
                    })
                });
                if !ctx.emitted_text() {
                    ctx.text(&out.text); // providers that return the answer in one piece
                }
                if out.estimated {
                    row.route_reason = format!("{} (tokens estimated)", row.route_reason);
                }
                if cfg.storage.store_content {
                    row.response = out.text.clone();
                }
                let id = self.store.insert(&row)?;
                Ok(ChatResponse {
                    id,
                    text: out.text,
                    provider: row.provider,
                    model: row.model,
                    input_tokens: row.input_tokens,
                    output_tokens: row.output_tokens,
                    cost_usd: row.cost_usd,
                    billing: row.billing,
                    latency_ms: row.latency_ms,
                    route_reason: row.route_reason,
                })
            }
            Err(e) => {
                row.status = "error".into();
                row.error = format!("{e:#}");
                let _ = self.store.insert(&row);
                Err(e)
            }
        }
    }
}

fn mtime(p: &std::path::Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}
