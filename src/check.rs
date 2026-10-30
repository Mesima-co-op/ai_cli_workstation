//! Setup checks: which supported tools/models are installed or configured on this
//! machine, what each pipeline role would use right now, and how to install what is
//! missing. Nothing here installs anything.

use crate::config::{Config, ProviderCfg};
use crate::jev;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Find an executable the way the shell would (PATH + PATHEXT on Windows).
pub fn which(cmd: &str) -> Option<PathBuf> {
    let p = Path::new(cmd);
    if p.is_absolute() || p.components().count() > 1 {
        return p.is_file().then(|| p.to_path_buf());
    }
    let exts: Vec<String> = if cfg!(windows) {
        let pathext = std::env::var("PATHEXT").unwrap_or_else(|_| ".EXE;.CMD;.BAT;.COM".into());
        let mut v: Vec<String> = pathext.split(';').filter(|e| !e.is_empty()).map(|e| e.to_lowercase()).collect();
        v.push(String::new()); // an exact name like "claude.exe" is tried last
        v
    } else {
        vec![String::new()]
    };
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        for e in &exts {
            let cand = dir.join(format!("{cmd}{e}"));
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    None
}

/// First line of `<exe> --version`, with a timeout so a hung tool cannot block us.
pub fn version_of(exe: &Path) -> Option<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    let exe = exe.to_path_buf();
    std::thread::spawn(move || {
        let out = std::process::Command::new(exe).arg("--version").stdin(std::process::Stdio::null()).output();
        let _ = tx.send(out);
    });
    let out = rx.recv_timeout(Duration::from_secs(6)).ok()?.ok()?;
    let text = if out.stdout.is_empty() { out.stderr } else { out.stdout };
    String::from_utf8_lossy(&text).lines().next().map(|l| l.trim().to_string()).filter(|l| !l.is_empty())
}

/// Places Hermes keeps its `.env`: $HERMES_HOME, %LOCALAPPDATA%\hermes (Windows), ~/.hermes.
pub fn hermes_env_files() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Ok(h) = std::env::var("HERMES_HOME") {
        if !h.is_empty() {
            v.push(PathBuf::from(h).join(".env"));
        }
    }
    if let Some(d) = dirs::data_local_dir() {
        v.push(d.join("hermes").join(".env"));
    }
    if let Some(h) = dirs::home_dir() {
        v.push(h.join(".hermes").join(".env"));
    }
    v
}

/// Value of `name` from the first Hermes `.env` that defines it (quotes stripped).
pub fn hermes_env_value(name: &str) -> Option<(String, PathBuf)> {
    for f in hermes_env_files() {
        let Ok(text) = std::fs::read_to_string(&f) else { continue };
        for line in text.lines() {
            let line = line.trim().trim_start_matches("export ").trim();
            if let Some(rest) = line.strip_prefix(name) {
                if let Some(v) = rest.trim_start().strip_prefix('=') {
                    let v = v.trim().trim_matches('"').trim_matches('\'');
                    if !v.is_empty() {
                        return Some((v.to_string(), f.clone()));
                    }
                }
            }
        }
    }
    None
}

fn is_loopback_url(url: &str) -> bool {
    let u = if url.is_empty() { "http://127.0.0.1" } else { url };
    let host = u.split("://").nth(1).unwrap_or(u).split('/').next().unwrap_or("");
    let host = host.rsplit('@').next().unwrap_or(host);
    let host = if host.starts_with('[') { host.split(']').next().unwrap_or(host).trim_start_matches('[') } else { host.split(':').next().unwrap_or(host) };
    matches!(host, "localhost" | "127.0.0.1" | "::1")
}

/// The Hermes API key: the env var named in the provider, or - only when Hermes runs
/// on this machine (loopback URL) - `API_SERVER_KEY` from Hermes' own `.env`.
pub fn hermes_key(p: &ProviderCfg) -> Option<String> {
    if let Some(k) = crate::secrets::get(&p.api_key_env) {
        return Some(k);
    }
    if is_loopback_url(&p.base_url) {
        return hermes_env_value("API_SERVER_KEY").map(|(v, _)| v);
    }
    None
}

fn env_set(name: &str) -> bool {
    crate::secrets::get(name).is_some()
}

/// Can this provider be used right now? (No network calls; failures at call time
/// still fall through to the next provider.)
pub fn provider_ready(p: &ProviderCfg) -> Result<(), String> {
    if !p.enabled {
        return Err("disabled".into());
    }
    match p.kind.as_str() {
        "claude-cli" | "hermes-cli" => {
            let default = if p.kind == "claude-cli" { "claude" } else { "hermes" };
            let cmd = if p.command.is_empty() { default } else { &p.command };
            which(cmd).map(|_| ()).ok_or_else(|| format!("'{cmd}' is not installed or not on PATH"))
        }
        "hermes-api" | "hermes-runs" => {
            if hermes_key(p).is_some() {
                Ok(())
            } else {
                Err(format!("no key: run `aiws key set {}` (or keep API_SERVER_KEY in Hermes' .env)", crate::config::env_label(&p.api_key_env)))
            }
        }
        "anthropic" => {
            if env_set(&p.api_key_env) { Ok(()) } else { Err(format!("no key: run `aiws key set {}`", crate::config::env_label(&p.api_key_env))) }
        }
        _ => {
            if p.api_key_env.is_empty() || env_set(&p.api_key_env) { Ok(()) } else { Err(format!("no key: run `aiws key set {}`", crate::config::env_label(&p.api_key_env))) }
        }
    }
}

/// Split a provider-name list into usable names and (name, reason) for the rest.
pub fn ready_chain(cfg: &Config, names: &[String]) -> (Vec<String>, Vec<(String, String)>) {
    let (mut ready, mut skipped) = (Vec::new(), Vec::new());
    for n in names {
        match cfg.providers.get(n) {
            None => skipped.push((n.clone(), "not defined in [providers]".into())),
            Some(p) => match provider_ready(p) {
                Ok(()) => ready.push(n.clone()),
                Err(why) => skipped.push((n.clone(), why)),
            },
        }
    }
    (ready, skipped)
}

/// Every ready provider not in `exclude`, default provider first.
pub fn any_ready(cfg: &Config, exclude: &[String]) -> Vec<String> {
    let mut names: Vec<&String> = cfg.providers.keys().collect();
    names.sort_by_key(|n| (**n != cfg.default_provider, (*n).clone()));
    names
        .into_iter()
        .filter(|n| !exclude.contains(n))
        // Jev only answers yes/no questions; never use it as a general fallback for chat.
        .filter(|n| cfg.providers.get(*n).map(|p| p.kind != "jev").unwrap_or(false))
        .filter(|n| cfg.providers.get(*n).map(|p| provider_ready(p).is_ok()).unwrap_or(false))
        .cloned()
        .collect()
}

pub struct Role {
    pub label: &'static str,
    pub text: String,
    /// false = running in a reduced / fallback mode
    pub full: bool,
}

/// What each pipeline role would use right now. `local` = the config describes
/// this machine (readiness can be checked); otherwise only the configured chains are shown.
pub fn roles(cfg: &Config, local: bool) -> Vec<Role> {
    let mut out = Vec::new();
    out.push(match jev::inactive_reason(&cfg.jev) {
        None => Role { label: "Decider", text: "Jev (typed decisions)".into(), full: true },
        Some(why) => Role { label: "Decider", text: format!("built-in heuristic (Jev off: {why})"), full: false },
    });
    let chain_text = |names: &[String]| names.join(" > ");
    if !local {
        out.push(Role { label: "Planner", text: chain_text(&cfg.pipeline.planner), full: true });
        out.push(Role { label: "Executor", text: chain_text(&cfg.pipeline.executor), full: true });
    } else {
        let (pl, _) = ready_chain(cfg, &cfg.pipeline.planner);
        out.push(match pl.first() {
            Some(p) => Role { label: "Planner", text: p.clone(), full: true },
            None => Role { label: "Planner", text: "none ready: the executor plans inline".into(), full: false },
        });
        let (ex, _) = ready_chain(cfg, &cfg.pipeline.executor);
        out.push(match ex.first() {
            Some(p) => Role { label: "Executor", text: p.clone(), full: true },
            None => match (cfg.pipeline.fallback_any, any_ready(cfg, &[]).first()) {
                (true, Some(p)) => Role { label: "Executor", text: format!("{p} (fallback)"), full: false },
                _ => Role { label: "Executor", text: "NOTHING READY: run `aiws check`".into(), full: false },
            },
        });
    }
    out.push(Role {
        label: "Pipeline",
        text: if cfg.pipeline.enabled { "ON".into() } else { "off (aiws config set pipeline.enabled true)".into() },
        full: cfg.pipeline.enabled,
    });
    out
}

pub struct Component {
    pub name: &'static str,
    pub role: &'static str,
    pub ok: bool,
    pub detail: String,
    pub install: String,
}

async fn ping(client: &reqwest::Client, url: &str) -> bool {
    client.get(url).send().await.map(|r| r.status().is_success()).unwrap_or(false)
}

pub async fn components(cfg: &Config) -> Vec<Component> {
    let client = reqwest::Client::builder().timeout(Duration::from_secs(3)).build().unwrap();
    let mut v = Vec::new();

    let cc = cfg.providers.get("claude_code").map(|p| p.command.clone()).filter(|c| !c.is_empty()).unwrap_or_else(|| "claude".into());
    let found = which(&cc);
    v.push(Component {
        name: "Claude Code (CLI)",
        role: "executor, uses your Claude subscription",
        ok: found.is_some(),
        detail: match &found {
            Some(p) => format!("{} {}", p.display(), version_of(p).unwrap_or_default()).trim().to_string(),
            None => "not found on PATH".into(),
        },
        install: if cfg!(windows) {
            "irm https://claude.ai/install.ps1 | iex   (or: npm install -g @anthropic-ai/claude-code), then run `claude` once to sign in".into()
        } else {
            "curl -fsSL https://claude.ai/install.sh | bash   (or: npm install -g @anthropic-ai/claude-code), then run `claude` once to sign in".into()
        },
    });

    let ak = cfg.providers.get("anthropic").map(|p| p.api_key_env.clone()).unwrap_or_else(|| "ANTHROPIC_API_KEY".into());
    let akl = crate::config::env_label(&ak);
    v.push(Component {
        name: "Anthropic API key",
        role: "executor (pay-per-token), optional if you use Claude Code",
        ok: env_set(&ak),
        detail: if env_set(&ak) { format!("{akl} is set") } else { format!("{akl} is not set") },
        install: format!("create a key at console.anthropic.com, run `aiws key set {akl}`, then: aiws config set providers.anthropic.enabled true"),
    });

    let hc = cfg.providers.get("hermes_cli").map(|p| p.command.clone()).filter(|c| !c.is_empty()).unwrap_or_else(|| "hermes".into());
    let hfound = which(&hc);
    v.push(Component {
        name: "Hermes Agent (CLI)",
        role: "planner / agent",
        ok: hfound.is_some(),
        detail: match &hfound {
            Some(p) => format!("{} {}", p.display(), version_of(p).unwrap_or_default()).trim().to_string(),
            None => "not found on PATH".into(),
        },
        install: "see the Installation section at https://hermes-agent.nousresearch.com/docs, then run `hermes setup`".into(),
    });

    let hp = cfg.providers.get("hermes").cloned().unwrap_or_else(|| ProviderCfg {
        base_url: "http://127.0.0.1:8642/v1".into(),
        api_key_env: "HERMES_API_KEY".into(),
        ..Default::default()
    });
    let (hbase, hkey) = (hp.base_url.clone(), crate::config::env_label(&hp.api_key_env));
    let health = format!("{}/health", hbase.trim_end_matches('/').trim_end_matches("/v1"));
    let up = ping(&client, &health).await;
    let key = hermes_key(&hp).is_some();
    let key_src = if env_set(&hp.api_key_env) {
        format!("{hkey} set")
    } else if key {
        format!("key found in {}", hermes_env_value("API_SERVER_KEY").map(|(_, f)| f.display().to_string()).unwrap_or_default())
    } else {
        format!("no key ({hkey} not set, no API_SERVER_KEY in Hermes' .env)")
    };
    v.push(Component {
        name: "Hermes API server",
        role: "planner / agent with live tool progress and approvals",
        ok: up && key,
        detail: format!("server {} at {health}; {key_src}", if up { "answering" } else { "not answering" }),
        install: "add API_SERVER_ENABLED=true and API_SERVER_KEY=<secret> to Hermes' .env (Windows: %LOCALAPPDATA%\\hermes\\.env, otherwise ~/.hermes/.env), run `hermes gateway`; aiws reads the key from that file automatically. Then: aiws config set providers.hermes.enabled true".to_string(),
    });

    let oll = which("ollama");
    let oll_up = ping(&client, "http://127.0.0.1:11434/api/tags").await;
    v.push(Component {
        name: "Ollama (local models)",
        role: "optional local executor",
        ok: oll.is_some() && oll_up,
        detail: match (&oll, oll_up) {
            (Some(_), true) => "installed and running".into(),
            (Some(_), false) => "installed, server not running (start it with `ollama serve`)".into(),
            _ => "not installed".into(),
        },
        install: "download from https://ollama.com/download, then: aiws config set providers.ollama.enabled true".into(),
    });

    let ok_key = cfg.providers.get("openai").map(|p| p.api_key_env.clone()).unwrap_or_else(|| "OPENAI_API_KEY".into());
    let okl = crate::config::env_label(&ok_key);
    v.push(Component {
        name: "OpenAI API key",
        role: "optional executor",
        ok: env_set(&ok_key),
        detail: if env_set(&ok_key) { format!("{okl} is set") } else { format!("{okl} is not set") },
        install: format!("run `aiws key set {okl}`, set providers.openai.default_model, then: aiws config set providers.openai.enabled true"),
    });

    let jr = jev::inactive_reason(&cfg.jev);
    v.push(Component {
        name: "Jev (TypeSafe AI)",
        role: "decider: fast typed decisions (heuristic used if off)",
        ok: jr.is_none(),
        detail: match &jr {
            None => format!("configured ({}{}); reachability is tested on first use", cfg.jev.base_url, cfg.jev.path),
            Some(why) => format!("not set up: {why}"),
        },
        install: format!("get access at https://typesafe.ai, then set: aiws config set jev.base_url <url from Jev docs>; run `aiws key set {}` with your key; aiws config set jev.enabled true", crate::config::env_label(&cfg.jev.api_key_env)),
    });

    let dk = which("docker");
    v.push(Component {
        name: "Docker",
        role: "optional, to run the aiws daemon in a container",
        ok: dk.is_some(),
        detail: dk.map(|p| p.display().to_string()).unwrap_or_else(|| "not found on PATH".into()),
        install: "install Docker Desktop (https://www.docker.com/products/docker-desktop/) or Docker Engine".into(),
    });
    v
}

pub async fn print_report(cfg: &Config, config_path: &Path) {
    println!("aiws setup check");
    println!("  config   : {} ({})", config_path.display(), if config_path.exists() { "found" } else { "not created yet: `aiws init`" });
    println!("  database : {}", crate::config::db_path().display());
    for f in cfg.bad_env_fields() {
        println!("\n  WARNING: {f} holds something that is not an environment variable name (a key typed in by mistake?).");
        println!("           Rotate that key if it is real, then reset it: aiws config set {f} <ENV_VAR_NAME>");
    }
    println!("\nInstalled / configured on this machine:");
    let comps = components(cfg).await;
    for c in &comps {
        println!("  [{}] {:<20} {}", if c.ok { "ok" } else { "--" }, c.name, c.detail);
        println!("       {}", c.role);
    }
    println!("\nWhat the pipeline would use right now:");
    for r in roles(cfg, true) {
        println!("  {:<9} {}{}", r.label, r.text, if r.full { "" } else { "   <- reduced mode" });
    }
    let missing: Vec<&Component> = comps.iter().filter(|c| !c.ok).collect();
    if !missing.is_empty() {
        println!("\nNot set up (all optional; aiws works with whatever is ready):");
        for c in missing {
            println!("  - {}: {}", c.name, c.install);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_api_server_key_from_hermes_env_and_only_for_loopback() {
        let _g = crate::config::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("aiws-hermes-env-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".env"), "# comment\nAPI_SERVER_ENABLED=true\nexport API_SERVER_KEY=\"s3cret value\"\n").unwrap();
        unsafe { std::env::set_var("HERMES_HOME", &dir) };
        unsafe { std::env::remove_var("TEST_HK_UNSET") };
        let mut p = ProviderCfg { api_key_env: "TEST_HK_UNSET".into(), base_url: "http://127.0.0.1:8642/v1".into(), ..Default::default() };
        assert_eq!(hermes_key(&p).as_deref(), Some("s3cret value"));
        p.base_url = "http://localhost:8642/v1".into();
        assert!(hermes_key(&p).is_some());
        // never hand a local secret to a remote host
        p.base_url = "https://hermes.example.com/v1".into();
        assert!(hermes_key(&p).is_none());
    }
}
