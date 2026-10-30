mod check;
mod config;
mod engine;
mod jev;
mod project;
mod provider;
mod router;
mod secrets;
mod server;
mod source;
mod stream;
mod store;
mod theme;
mod tui;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use engine::{ChatRequest, Engine};
use provider::Msg;
use source::Source;
use std::io::{IsTerminal, Read};
use std::path::PathBuf;
use store::{Snapshot, Totals};

#[derive(Parser)]
#[command(name = "aiws", version, about = "AI CLI workstation: monitor, route and run AI models from the terminal")]
struct Cli {
    /// Config file (default: <AIWS_HOME>/config.toml)
    #[arg(long, global = true, env = "AIWS_CONFIG")]
    config: Option<PathBuf>,
    /// Talk to a remote `aiws serve` daemon instead of the local one, e.g. http://host:7878
    #[arg(long, global = true, env = "AIWS_REMOTE")]
    remote: Option<String>,
    /// Name of the env var holding the remote daemon's bearer token
    #[arg(long, global = true, default_value = "AIWS_TOKEN")]
    token_env: String,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create a default config file
    Init {
        #[arg(long)]
        force: bool,
    },
    /// Send a prompt (reads stdin if no prompt is given)
    Chat {
        prompt: Vec<String>,
        #[arg(short, long)]
        provider: Option<String>,
        #[arg(short, long)]
        model: Option<String>,
        #[arg(short, long)]
        system: Option<String>,
        /// Work inside a project scope (see `aiws project`); "none" ignores the default project
        #[arg(short = 'P', long)]
        project: Option<String>,
    },
    /// Folders the AI may work in or read, with safety rules (branches, no merge/delete, no secrets)
    Project {
        #[command(subcommand)]
        action: ProjectCmd,
    },
    /// Usage and spend summary
    Stats {
        #[arg(long)]
        json: bool,
    },
    /// Recent requests
    Log {
        #[arg(short, default_value_t = 20)]
        n: usize,
    },
    /// View or change configuration
    Config {
        #[command(subcommand)]
        action: ConfigCmd,
    },
    /// Run the daemon + HTTP API
    Serve {
        #[arg(long)]
        bind: Option<String>,
    },
    /// Interactive terminal UI (dashboard, requests, chat, config)
    Tui {
        /// Start on a tab: dashboard, requests, chat, config, settings, help
        #[arg(long)]
        tab: Option<String>,
    },
    /// Store or remove API keys privately (no shell setup needed)
    Key {
        #[command(subcommand)]
        action: KeyCmd,
    },
    /// Check what is installed/configured (Claude Code, Hermes, Jev, ...) and what to install
    #[command(alias = "doctor")]
    Check,
    /// How to connect Hermes Agent, plus a connection check
    Hermes,
}

#[derive(Subcommand)]
enum ProjectCmd {
    /// Create or update a project: `aiws project add myapp C:\code\myapp --context C:\code\shared-lib`
    Add {
        name: String,
        /// Folders the AI may change; the first is the working directory
        #[arg(required = true)]
        folders: Vec<PathBuf>,
        /// Read-only folders for extra context (repeatable)
        #[arg(short, long)]
        context: Vec<PathBuf>,
        /// Make this the project used when none is named
        #[arg(long)]
        default: bool,
    },
    /// List projects
    List,
    /// Show a project's folders, branch state and the rules that will be enforced
    Show { name: String },
    /// Choose the project used by default ("off" to clear)
    Use { name: String },
    /// Remove a project (never touches the folders)
    Rm { name: String },
}

#[derive(Subcommand)]
enum KeyCmd {
    /// Save a key. NAME is an environment variable name (HERMES_API_KEY) or a provider name (hermes)
    Set {
        name: String,
        /// Read the key from standard input instead of prompting
        #[arg(long)]
        stdin: bool,
    },
    /// Delete a stored key
    Rm { name: String },
    /// Show which keys are available and where they come from (never the values)
    List,
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Print the config file location
    Path,
    /// Print every setting
    Show,
    /// Print one setting, e.g. budget.daily_usd
    Get { key: String },
    /// Change one setting, e.g. `aiws config set budget.daily_usd 5`
    Set { key: String, value: String },
    /// Set the price (USD per million tokens) for a model-name prefix
    Price { model: String, input: f64, output: f64 },
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn make_source(cli: &Cli) -> Result<Source> {
    if let Some(url) = &cli.remote {
        let token = secrets::get(&cli.token_env);
        return Source::remote(url, token);
    }
    let path = cli.config.clone().unwrap_or_else(config::default_config_path);
    Ok(Source::Local(Engine::new(path)?))
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    match &cli.cmd {
        Cmd::Init { force } => {
            let path = cli.config.clone().unwrap_or_else(config::default_config_path);
            if path.exists() && !force {
                bail!("{} already exists (use --force to overwrite)", path.display());
            }
            config::save(&path, &config::Config::default())?;
            println!("Wrote {}", path.display());
            println!("Next: `aiws doctor`, then `aiws tui` (or `aiws chat \"hello\"`).");
        }
        Cmd::Key { action } => {
            let path = cli.config.clone().unwrap_or_else(config::default_config_path);
            let cfg = config::load(&path)?;
            // A provider name maps to the variable it reads its key from.
            let resolve = |n: &str| -> String {
                cfg.providers.get(n).map(|p| p.api_key_env.clone()).filter(|e| !e.is_empty()).unwrap_or_else(|| n.to_string())
            };
            match action {
                KeyCmd::Set { name, stdin } => {
                    let var = resolve(name);
                    let value = if *stdin || !std::io::stdin().is_terminal() {
                        let mut l = String::new();
                        std::io::stdin().read_line(&mut l)?;
                        l
                    } else {
                        rpassword::prompt_password(format!("Key for {var} (input hidden): "))?
                    };
                    secrets::set(&var, &value)?;
                    println!("Saved {var} to {} (readable only by your user). aiws uses it immediately.", secrets::path().display());
                }
                KeyCmd::Rm { name } => {
                    let var = resolve(name);
                    println!("{}", if secrets::remove(&var)? { format!("Removed {var}.") } else { format!("No stored key named {var}.") });
                }
                KeyCmd::List => {
                    let mut vars: std::collections::BTreeSet<String> = secrets::stored_names().into_iter().collect();
                    for p in cfg.providers.values() {
                        vars.insert(p.api_key_env.clone());
                    }
                    vars.insert(cfg.jev.api_key_env.clone());
                    vars.insert(cfg.server.token_env.clone());
                    for v in vars.into_iter().filter(|v| config::is_env_name(v)) {
                        let src = match secrets::source(&v) {
                            secrets::KeySource::Shell => "set in your shell",
                            secrets::KeySource::Stored => "stored by aiws",
                            secrets::KeySource::None => "not set",
                        };
                        println!("  {v:<22} {src}");
                    }
                }
            }
        }
        Cmd::Check => {
            let path = cli.config.clone().unwrap_or_else(config::default_config_path);
            let cfg = config::load(&path)?;
            check::print_report(&cfg, &path).await;
        }
        Cmd::Hermes => hermes(&cli).await?,
        Cmd::Serve { bind } => {
            let path = cli.config.clone().unwrap_or_else(config::default_config_path);
            server::serve(Engine::new(path)?, bind.clone()).await?;
        }
        Cmd::Tui { tab } => tui::run(make_source(&cli)?, tab.clone()).await?,
        Cmd::Project { action } => {
            let path = cli.config.clone().unwrap_or_else(config::default_config_path);
            let mut cfg = config::load(&path)?;
            match action {
                ProjectCmd::Add { name, folders, context, default } => {
                    let abs = |p: &PathBuf| -> Result<String> {
                        let a = std::path::absolute(p)?;
                        if !a.is_dir() {
                            bail!("not a folder: {}", a.display());
                        }
                        Ok(a.display().to_string())
                    };
                    let entry = cfg.projects.entry(name.clone()).or_default();
                    entry.folders = folders.iter().map(abs).collect::<Result<_>>()?;
                    entry.context = context.iter().map(abs).collect::<Result<_>>()?;
                    if *default {
                        cfg.default_project = name.clone();
                    }
                    config::save(&path, &cfg)?;
                    println!("Saved project '{name}'. Check it with `aiws project show {name}`; use it with `aiws chat -P {name} \"...\"`.");
                    if !*default {
                        println!("Make it the default with `aiws project use {name}`.");
                    }
                }
                ProjectCmd::List => {
                    if cfg.projects.is_empty() {
                        println!("No projects yet. Add one: aiws project add myapp <folder> [--context <folder>]");
                    }
                    for (n, p) in &cfg.projects {
                        let mark = if *n == cfg.default_project { " (default)" } else { "" };
                        println!("{n}{mark}\n  work:    {}\n  context: {}", p.folders.join(", "), if p.context.is_empty() { "-".into() } else { p.context.join(", ") });
                    }
                }
                ProjectCmd::Show { name } => {
                    let p = cfg.projects.get(name).ok_or_else(|| anyhow::anyhow!("no project '{name}'"))?;
                    println!("Project {name}");
                    println!("  may change : {}", p.folders.join(", "));
                    println!("  read only  : {}", if p.context.is_empty() { "-".into() } else { p.context.join(", ") });
                    println!("  branch     : {}", if p.create_branch { format!("new '{}...' branch before changes", p.branch_prefix) } else { "current branch".into() });
                    println!("  on secret  : {}", p.on_secret);
                    println!("  never read/edit: {}", p.protect.join(", "));
                    println!("  denied tools:");
                    for r in project::deny_rules(p) {
                        println!("    {r}");
                    }
                    println!("  folder check:");
                    for f in p.folders.iter().chain(p.context.iter()) {
                        let ok = std::path::Path::new(f).is_dir();
                        println!("    [{}] {f}", if ok { "ok" } else { "--" });
                    }
                    println!("Limits are enforced for Claude Code only; Hermes is not used as executor while a project is active.");
                }
                ProjectCmd::Use { name } => {
                    cfg.default_project = if name == "off" { String::new() } else { name.clone() };
                    config::save(&path, &cfg)?;
                    println!("Default project: {}", if name == "off" { "(none)" } else { name });
                }
                ProjectCmd::Rm { name } => {
                    if cfg.projects.remove(name).is_none() {
                        bail!("no project '{name}'");
                    }
                    if cfg.default_project == *name {
                        cfg.default_project.clear();
                    }
                    config::save(&path, &cfg)?;
                    println!("Removed project '{name}' (folders untouched).");
                }
            }
        }
        Cmd::Chat { prompt, provider, model, system, project } => {
            let mut text = prompt.join(" ");
            if text.trim().is_empty() && !std::io::stdin().is_terminal() {
                std::io::stdin().read_to_string(&mut text)?;
            }
            if text.trim().is_empty() {
                bail!("give a prompt: aiws chat \"your question\"  (or pipe text in)");
            }
            let src = make_source(&cli)?;
            let req = ChatRequest {
                messages: vec![Msg { role: "user".into(), content: text }],
                system: system.clone(),
                provider: provider.clone(),
                model: model.clone(),
                max_tokens: None,
                source: Some("cli".into()),
                project: project.clone(),
                stream: false,
                note: None,
            };
            let r = chat_live(&src, req).await?;
            let cost = match (r.cost_usd, r.billing.as_str()) {
                (Some(c), "subscription") => format!("${c:.4} (subscription, not billed)"),
                (Some(c), _) => format!("${c:.4}"),
                (None, _) => "cost unknown (no price set)".into(),
            };
            eprintln!(
                "\n[{} / {} | {} in, {} out tokens | {} | {} ms | {}]",
                r.provider, r.model, r.input_tokens, r.output_tokens, cost, r.latency_ms, r.route_reason
            );
        }
        Cmd::Stats { json } => {
            let snap = make_source(&cli)?.snapshot().await?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&snap)?);
            } else {
                print_stats(&snap);
            }
        }
        Cmd::Log { n } => {
            let snap = make_source(&cli)?.snapshot().await?;
            println!("{:<5} {:<19} {:<12} {:<22} {:>7} {:>7} {:>9} {:>7}  {}", "ID", "TIME", "PROVIDER", "MODEL", "IN", "OUT", "COST", "MS", "STATUS");
            for r in snap.recent.iter().take(*n) {
                let t = chrono::DateTime::from_timestamp_millis(r.ts)
                    .map(|d| d.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S").to_string())
                    .unwrap_or_default();
                let cost = r.cost_usd.map(|c| format!("${c:.4}")).unwrap_or_else(|| "-".into());
                let status = if r.status == "ok" { "ok".to_string() } else { format!("ERR {}", store::clip(&r.error, 60)) };
                println!(
                    "{:<5} {:<19} {:<12} {:<22} {:>7} {:>7} {:>9} {:>7}  {}",
                    r.id, t, store::clip(&r.provider, 12), store::clip(&r.model, 22), r.input_tokens, r.output_tokens, cost, r.latency_ms, status
                );
            }
        }
        Cmd::Config { action } => match action {
            ConfigCmd::Path => match &cli.remote {
                Some(u) => println!("(remote) {u}/v1/config"),
                None => println!("{}", cli.config.clone().unwrap_or_else(config::default_config_path).display()),
            },
            ConfigCmd::Show => {
                let cfg = make_source(&cli)?.config().await?;
                for i in config::flatten(&cfg) {
                    println!("{} = {}", i.path, i.value);
                }
            }
            ConfigCmd::Get { key } => {
                let cfg = make_source(&cli)?.config().await?;
                match config::get_path(&cfg, key) {
                    Some(v) => println!("{v}"),
                    None => bail!("unknown setting '{key}' (see `aiws config show`)"),
                }
            }
            ConfigCmd::Price { model, input, output } => {
                let src = make_source(&cli)?;
                let mut cfg = src.config().await?;
                cfg.prices.insert(model.clone(), config::Price { input_per_mtok: *input, output_per_mtok: *output });
                src.save_config(cfg).await?;
                println!("prices.{model}: ${input}/M in, ${output}/M out");
            }
            ConfigCmd::Set { key, value } => {
                let src = make_source(&cli)?;
                let cfg = src.config().await?;
                let updated = config::set_path(&cfg, key, value)?;
                src.save_config(updated).await?;
                println!("{key} = {value}");
            }
        },
    }
    Ok(())
}

fn money(v: f64) -> String {
    format!("${v:.4}")
}

fn totals_line(label: &str, t: &Totals) {
    println!(
        "{:<10} {:>6} req  {:>5} err  {:>10} in  {:>10} out  api {:>10}  subscription(notional) {:>10}  avg {:>6.0} ms",
        label, t.requests, t.errors, t.input_tokens, t.output_tokens, money(t.api_cost_usd), money(t.subscription_cost_usd), t.avg_latency_ms
    );
}

fn print_stats(s: &Snapshot) {
    totals_line("today", &s.today);
    totals_line("month", &s.month);
    totals_line("all time", &s.all_time);
    if s.today.unpriced > 0 || s.month.unpriced > 0 {
        println!("note: some API requests have no price set, so their cost is unknown (add [prices.\"<model>\"] to the config).");
    }
    if s.budget_daily_usd > 0.0 {
        println!("daily budget: {} of ${:.2}", money(s.today.api_cost_usd), s.budget_daily_usd);
    }
    if s.budget_monthly_usd > 0.0 {
        println!("monthly budget: {} of ${:.2}", money(s.month.api_cost_usd), s.budget_monthly_usd);
    }
    println!("\nBy model (last 30 days):");
    println!("{:<12} {:<24} {:>6} {:>5} {:>10} {:>10} {:>10} {:>8}", "PROVIDER", "MODEL", "REQ", "ERR", "IN", "OUT", "COST", "AVG MS");
    for m in &s.by_model {
        println!(
            "{:<12} {:<24} {:>6} {:>5} {:>10} {:>10} {:>10} {:>8.0}",
            store::clip(&m.provider, 12), store::clip(&m.model, 24), m.requests, m.errors, m.input_tokens, m.output_tokens, money(m.cost_usd), m.avg_latency_ms
        );
    }
}

async fn hermes_api_state(p: &config::ProviderCfg) -> String {
    let key_ok = check::hermes_key(p).is_some();
    let base = if p.base_url.is_empty() { "http://127.0.0.1:8642/v1" } else { &p.base_url };
    let health = format!("{}/health", base.trim_end_matches('/').trim_end_matches("/v1"));
    let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(3)).build().unwrap();
    let reachable = client.get(&health).send().await.map(|r| r.status().is_success()).unwrap_or(false);
    match (reachable, key_ok) {
        (true, true) => format!("ready (Hermes API server up at {base})"),
        (true, false) => format!("NOT READY: server is up, but no key: set env var {} to API_SERVER_KEY (or keep it in Hermes' .env)", config::env_label(&p.api_key_env)),
        (false, _) => format!("NOT READY: nothing answering at {health}; start `hermes gateway` and check API_SERVER_ENABLED=true in Hermes' .env"),
    }
}

async fn hermes(cli: &Cli) -> Result<()> {
    println!("Connect Hermes Agent to aiws");
    println!("============================");
    println!("Option A: API server (recommended: full agent with tools, works from Docker/remote)");
    println!("  1. Add to Hermes' .env (Windows: %LOCALAPPDATA%\\hermes\\.env, otherwise ~/.hermes/.env):");
    println!("       API_SERVER_ENABLED=true");
    println!("       API_SERVER_KEY=<a long random secret>");
    println!("  2. Start it:            hermes gateway          (listens on http://127.0.0.1:8642)");
    println!("  3. The key:             aiws reads API_SERVER_KEY from that .env automatically (Hermes on this PC).");
    println!("                          For a remote Hermes, set HERMES_API_KEY to the secret instead.");
    println!("  4. Enable provider:     aiws config set providers.hermes.enabled true");
    println!("  5. Try it:              aiws chat -p hermes \"hello\"        (plain chat, streams text)");
    println!("     Agent mode:          aiws config set providers.hermes_runs.enabled true");
    println!("                          aiws chat -p hermes_runs \"list the files in the current folder\"");
    println!("                          (shows tool progress live and asks before risky actions)");
    println!();
    println!("Option B: local CLI (no server; each request runs `hermes -z`)");
    println!("  aiws config set providers.hermes_cli.enabled true");
    println!("  aiws chat -p hermes_cli \"hello\"");
    println!();
    println!("Current state:");
    let path = cli.config.clone().unwrap_or_else(config::default_config_path);
    let cfg = config::load(&path)?;
    for (name, p) in cfg.providers.iter().filter(|(_, p)| p.kind.starts_with("hermes")) {
        let state = if !p.enabled {
            "disabled".to_string()
        } else if matches!(p.kind.as_str(), "hermes-api" | "hermes-runs") {
            hermes_api_state(p).await
        } else {
            match std::process::Command::new(if p.command.is_empty() { "hermes" } else { &p.command }).arg("--version").output() {
                Ok(o) if o.status.success() => format!("ready ({})", String::from_utf8_lossy(&o.stdout).trim()),
                _ => "NOT READY: hermes command not found".to_string(),
            }
        };
        println!("  {name:<12} {:<11} {state}", p.kind);
    }
    Ok(())
}

/// Run a chat request, printing the answer as it streams and asking on the
/// terminal before an agent does something that needs approval.
async fn chat_live(src: &Source, req: ChatRequest) -> Result<engine::ChatResponse> {
    use std::io::Write;
    use stream::StreamEvent;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let worker = {
        let (src, tx) = (src.clone(), tx.clone());
        tokio::spawn(async move { stream::drive(src.chat_stream(req, tx.clone()), tx).await })
    };
    drop(tx);
    let mut printed = false;
    let mut result = None;
    while let Some(ev) = rx.recv().await {
        match ev {
            StreamEvent::Text { text } => {
                print!("{text}");
                let _ = std::io::stdout().flush();
                printed = true;
            }
            StreamEvent::Tool { name, state, detail } => eprintln!("\n  [{state}] {name} {detail}"),
            StreamEvent::Status { text } => eprintln!("\n  [{text}]"),
            StreamEvent::Approval { id, summary } => {
                eprintln!("\n  The agent asks permission:\n    {summary}");
                let choice = if std::io::stdin().is_terminal() {
                    eprint!("  Allow once? [y/N] ");
                    let _ = std::io::stderr().flush();
                    let line = tokio::task::spawn_blocking(|| {
                        let mut l = String::new();
                        let _ = std::io::stdin().read_line(&mut l);
                        l
                    })
                    .await
                    .unwrap_or_default();
                    if line.trim().eq_ignore_ascii_case("y") { "once" } else { "deny" }
                } else {
                    eprintln!("  (not a terminal: denied)");
                    "deny"
                };
                src.approve(&id, choice).await?;
            }
            StreamEvent::Done { response } => {
                result = Some(Ok(response));
                break;
            }
            StreamEvent::Error { message } => {
                result = Some(Err(anyhow::anyhow!(message)));
                break;
            }
        }
    }
    let _ = worker.await;
    if printed {
        println!();
    }
    result.unwrap_or_else(|| Err(anyhow::anyhow!("request ended without a result")))
}
