//! Configuration: typed structs, defaults, validation and a generic
//! "flat key/value" view used by both `aiws config` and the TUI editor.

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Config {
    pub default_provider: String,
    pub server: ServerCfg,
    pub storage: StorageCfg,
    pub budget: Budget,
    pub router: RouterCfg,
    pub pipeline: PipelineCfg,
    pub jev: JevCfg,
    pub providers: BTreeMap<String, ProviderCfg>,
    /// Price per million tokens, keyed by model-name prefix (longest prefix wins).
    pub prices: BTreeMap<String, Price>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct ServerCfg {
    pub bind: String,
    /// Name of the env var holding the API bearer token. Empty = no auth (loopback only).
    pub token_env: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct StorageCfg {
    /// Keep prompt/response text in the local database (turn off for metrics only).
    pub store_content: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Budget {
    pub daily_usd: f64,
    pub monthly_usd: f64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct RouterCfg {
    pub enabled: bool,
    pub long_prompt_chars: u64,
    pub simple_provider: String,
    pub simple_model: String,
    pub complex_provider: String,
    pub complex_model: String,
}

/// Multi-model pipeline: decider -> planner -> executor, each with fallbacks.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct PipelineCfg {
    pub enabled: bool,
    /// Providers tried in order for the "how to do it" step.
    pub planner: Vec<String>,
    /// Providers tried in order to carry out the request.
    pub executor: Vec<String>,
    /// If nothing listed works, use any other provider that is set up.
    pub fallback_any: bool,
    pub simple_model: String,
    pub complex_model: String,
}

/// Jev (TypeSafe AI) typed-decision model. Off until a URL and key are provided.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct JevCfg {
    pub enabled: bool,
    pub base_url: String,
    pub path: String,
    pub api_key_env: String,
    /// "bearer" or "x-api-key"
    pub auth: String,
    pub timeout_ms: u64,
    pub min_confidence: f64,
}

impl Default for PipelineCfg {
    fn default() -> Self {
        Self {
            enabled: false,
            planner: vec!["hermes_runs".into(), "hermes".into(), "hermes_cli".into()],
            executor: vec!["claude_code".into(), "anthropic".into()],
            fallback_any: true,
            simple_model: "haiku".into(),
            complex_model: "sonnet".into(),
        }
    }
}

impl Default for JevCfg {
    fn default() -> Self {
        Self {
            enabled: false,
            base_url: String::new(),
            path: "/v1/decide".into(),
            api_key_env: "JEV_API_KEY".into(),
            auth: "bearer".into(),
            timeout_ms: 3000,
            min_confidence: 0.6,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct ProviderCfg {
    /// "anthropic", "openai" (any OpenAI-compatible API) or "claude-cli".
    pub kind: String,
    pub enabled: bool,
    pub base_url: String,
    /// Name of the env var holding the API key (the key itself is never stored).
    pub api_key_env: String,
    pub default_model: String,
    /// Executable for kind = "claude-cli".
    pub command: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct Price {
    pub input_per_mtok: f64,
    pub output_per_mtok: f64,
}

impl Default for ServerCfg {
    fn default() -> Self {
        Self { bind: "127.0.0.1:7878".into(), token_env: "AIWS_TOKEN".into() }
    }
}
impl Default for StorageCfg {
    fn default() -> Self {
        Self { store_content: true }
    }
}
impl Default for Budget {
    fn default() -> Self {
        Self { daily_usd: 0.0, monthly_usd: 0.0 }
    }
}
impl Default for RouterCfg {
    fn default() -> Self {
        Self {
            enabled: false,
            long_prompt_chars: 1500,
            simple_provider: "claude_code".into(),
            simple_model: "haiku".into(),
            complex_provider: "claude_code".into(),
            complex_model: "sonnet".into(),
        }
    }
}
impl Default for ProviderCfg {
    fn default() -> Self {
        Self {
            kind: "openai".into(),
            enabled: false,
            base_url: String::new(),
            api_key_env: String::new(),
            default_model: String::new(),
            command: String::new(),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        let mut providers = BTreeMap::new();
        providers.insert(
            "claude_code".into(),
            ProviderCfg {
                kind: "claude-cli".into(),
                enabled: true,
                command: "claude".into(),
                ..Default::default()
            },
        );
        providers.insert(
            "anthropic".into(),
            ProviderCfg {
                kind: "anthropic".into(),
                enabled: false,
                base_url: "https://api.anthropic.com".into(),
                api_key_env: "ANTHROPIC_API_KEY".into(),
                default_model: "claude-sonnet-5-5".into(),
                ..Default::default()
            },
        );
        providers.insert(
            "openai".into(),
            ProviderCfg {
                kind: "openai".into(),
                enabled: false,
                base_url: "https://api.openai.com/v1".into(),
                api_key_env: "OPENAI_API_KEY".into(),
                default_model: String::new(),
                ..Default::default()
            },
        );
        providers.insert(
            "ollama".into(),
            ProviderCfg {
                kind: "openai".into(),
                enabled: false,
                base_url: "http://localhost:11434/v1".into(),
                default_model: "llama3.2".into(),
                ..Default::default()
            },
        );
        providers.insert(
            "hermes".into(),
            ProviderCfg {
                kind: "hermes-api".into(),
                enabled: false,
                base_url: "http://127.0.0.1:8642/v1".into(),
                api_key_env: "HERMES_API_KEY".into(),
                default_model: "hermes-agent".into(),
                ..Default::default()
            },
        );
        providers.insert(
            "hermes_runs".into(),
            ProviderCfg {
                kind: "hermes-runs".into(),
                enabled: false,
                base_url: "http://127.0.0.1:8642/v1".into(),
                api_key_env: "HERMES_API_KEY".into(),
                default_model: "hermes-agent".into(),
                ..Default::default()
            },
        );
        providers.insert(
            "hermes_cli".into(),
            ProviderCfg {
                kind: "hermes-cli".into(),
                enabled: false,
                command: "hermes".into(),
                ..Default::default()
            },
        );
        Self {
            default_provider: "claude_code".into(),
            server: ServerCfg::default(),
            storage: StorageCfg::default(),
            budget: Budget::default(),
            router: RouterCfg::default(),
            pipeline: PipelineCfg::default(),
            jev: JevCfg::default(),
            providers,
            prices: BTreeMap::new(),
        }
    }
}

impl Config {
    pub fn validate(&self) -> Result<()> {
        if !self.providers.contains_key(&self.default_provider) {
            bail!("default_provider '{}' is not defined under [providers]", self.default_provider);
        }
        for (name, p) in &self.providers {
            if !matches!(p.kind.as_str(), "anthropic" | "openai" | "claude-cli" | "hermes-api" | "hermes-runs" | "hermes-cli") {
                bail!("providers.{name}.kind must be anthropic, openai, claude-cli, hermes-api, hermes-runs or hermes-cli (got '{}')", p.kind);
            }
        }
        if self.server.bind.parse::<std::net::SocketAddr>().is_err() {
            bail!("server.bind '{}' must look like 127.0.0.1:7878", self.server.bind);
        }
        if self.budget.daily_usd < 0.0 || self.budget.monthly_usd < 0.0 {
            bail!("budget values cannot be negative");
        }
        if !matches!(self.jev.auth.as_str(), "bearer" | "x-api-key") {
            bail!("jev.auth must be bearer or x-api-key");
        }
        if !(0.0..=1.0).contains(&self.jev.min_confidence) {
            bail!("jev.min_confidence must be between 0 and 1");
        }
        if self.router.enabled {
            for p in [&self.router.simple_provider, &self.router.complex_provider] {
                if !self.providers.contains_key(p) {
                    bail!("router refers to unknown provider '{p}'");
                }
            }
        }
        Ok(())
    }

    /// Settings that must hold an environment variable NAME but hold something else
    /// (typically a secret typed in by mistake).
    pub fn bad_env_fields(&self) -> Vec<String> {
        let mut v = Vec::new();
        for (n, p) in &self.providers {
            if !p.api_key_env.is_empty() && !is_env_name(&p.api_key_env) {
                v.push(format!("providers.{n}.api_key_env"));
            }
        }
        if !self.jev.api_key_env.is_empty() && !is_env_name(&self.jev.api_key_env) {
            v.push("jev.api_key_env".into());
        }
        if !self.server.token_env.is_empty() && !is_env_name(&self.server.token_env) {
            v.push("server.token_env".into());
        }
        v
    }

    pub fn price_for(&self, model: &str) -> Option<&Price> {
        self.prices
            .iter()
            .filter(|(k, _)| model.starts_with(k.as_str()))
            .max_by_key(|(k, _)| k.len())
            .map(|(_, v)| v)
    }
}

#[cfg(test)]
pub static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub fn is_env_name(s: &str) -> bool {
    let mut c = s.chars();
    matches!(c.next(), Some(ch) if ch.is_ascii_alphabetic() || ch == '_') && c.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

/// An env var name that is safe to show; anything else is hidden because it may be a secret.
pub fn env_label(s: &str) -> String {
    if is_env_name(s) { s.to_string() } else { "(invalid name, hidden)".to_string() }
}

// ---------------------------------------------------------------- paths / io

pub fn home_dir() -> PathBuf {
    if let Ok(h) = std::env::var("AIWS_HOME") {
        if !h.is_empty() {
            return PathBuf::from(h);
        }
    }
    dirs::data_local_dir().unwrap_or_else(|| PathBuf::from(".")).join("aiws")
}

pub fn default_config_path() -> PathBuf {
    home_dir().join("config.toml")
}

pub fn db_path() -> PathBuf {
    home_dir().join("aiws.db")
}

pub fn load(path: &Path) -> Result<Config> {
    if !path.exists() {
        return Ok(Config::default());
    }
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut cfg: Config = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    // Configs written by older versions lack newer built-in providers; add them (disabled).
    for (name, p) in Config::default().providers {
        cfg.providers.entry(name).or_insert(p);
    }
    cfg.validate()?;
    Ok(cfg)
}

pub fn save(path: &Path, cfg: &Config) -> Result<()> {
    cfg.validate()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let body = toml::to_string_pretty(cfg)?;
    let header = "# aiws configuration. API keys are NOT stored here: set the env var named by api_key_env.\n\n";
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, format!("{header}{body}"))?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

// ------------------------------------------------------- flat key/value view

#[derive(Clone, Debug)]
pub struct Item {
    pub path: String,
    pub value: String,
    pub help: &'static str,
}

pub fn flatten(cfg: &Config) -> Vec<Item> {
    let v = serde_json::to_value(cfg).unwrap_or(Value::Null);
    let mut out = Vec::new();
    walk(&v, String::new(), &mut out);
    out
}

fn walk(v: &Value, prefix: String, out: &mut Vec<Item>) {
    match v {
        Value::Object(m) => {
            for (k, child) in m {
                let p = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                walk(child, p, out);
            }
        }
        leaf => {
            let value = match leaf {
                Value::String(s) => s.clone(),
                Value::Array(a) => a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(", "),
                other => other.to_string(),
            };
            let help = help_for(&prefix);
            out.push(Item { path: prefix, value, help });
        }
    }
}

/// Apply `f` to the leaf at a dotted path; object keys may themselves contain dots.
fn with_leaf(v: &mut Value, path: &str, f: &mut dyn FnMut(&mut Value) -> Result<()>) -> Option<Result<()>> {
    let obj = v.as_object_mut()?;
    let keys: Vec<String> = obj.keys().cloned().collect();
    for k in keys {
        if path == k {
            return Some(f(obj.get_mut(&k)?));
        }
        if let Some(rest) = path.strip_prefix(&format!("{k}.")) {
            if let Some(r) = with_leaf(obj.get_mut(&k)?, rest, f) {
                return Some(r);
            }
        }
    }
    None
}

/// Return a new config with `path` set to `raw`, type-checked against the old value.
pub fn set_path(cfg: &Config, path: &str, raw: &str) -> Result<Config> {
    let mut v = serde_json::to_value(cfg)?;
    let mut apply = |slot: &mut Value| -> Result<()> {
        let new = match slot {
            Value::Bool(_) => match raw.trim().to_ascii_lowercase().as_str() {
                "true" | "yes" | "on" | "1" => Value::Bool(true),
                "false" | "no" | "off" | "0" => Value::Bool(false),
                _ => bail!("'{raw}' is not a boolean (use true/false)"),
            },
            Value::Number(n) if n.is_u64() || n.is_i64() => {
                let x: u64 = raw.trim().parse().map_err(|_| anyhow!("'{raw}' is not a whole number"))?;
                Value::from(x)
            }
            Value::Number(_) => {
                let x: f64 = raw.trim().parse().map_err(|_| anyhow!("'{raw}' is not a number"))?;
                serde_json::Number::from_f64(x).map(Value::Number).ok_or_else(|| anyhow!("invalid number"))?
            }
            Value::String(_) => Value::String(raw.to_string()),
            Value::Array(_) => Value::Array(
                raw.split(',').map(|x| x.trim()).filter(|x| !x.is_empty()).map(|x| Value::String(x.to_string())).collect(),
            ),
            _ => bail!("'{path}' is not an editable value"),
        };
        *slot = new;
        Ok(())
    };
    with_leaf(&mut v, path, &mut apply).ok_or_else(|| anyhow!("unknown setting '{path}'"))??;
    let updated: Config = serde_json::from_value(v)?;
    updated.validate()?;
    let before = cfg.bad_env_fields();
    if updated.bad_env_fields().iter().any(|f| !before.contains(f)) {
        bail!("'{path}' must be the NAME of an environment variable (like HERMES_API_KEY), not the key itself. Put the key in that environment variable instead.");
    }
    Ok(updated)
}

pub fn get_path(cfg: &Config, path: &str) -> Option<String> {
    flatten(cfg).into_iter().find(|i| i.path == path).map(|i| i.value)
}

fn help_for(path: &str) -> &'static str {
    let last = path.rsplit('.').next().unwrap_or(path);
    match last {
        "default_provider" => "Provider used when a request does not pick one (and the router is off).",
        "bind" => "Address the daemon/API listens on. Use 0.0.0.0:7878 for Docker/remote (needs a token).",
        "token_env" => "Name of the environment variable holding the API bearer token. Press s to store the token itself. Empty = no auth.",
        "store_content" => "Save prompt/response text locally so you can inspect it. Off = metrics only.",
        "daily_usd" => "Hard stop for pay-per-token (API) spend per local day, USD. 0 = unlimited.",
        "monthly_usd" => "Hard stop for pay-per-token (API) spend per calendar month, USD. 0 = unlimited.",
        "enabled" if path.starts_with("pipeline") => "Use the multi-model pipeline: a decider judges the request, a planner outlines how, an executor does it. Missing parts fall back automatically.",
        "enabled" if path.starts_with("jev") => "Use Jev (TypeSafe AI) for fast typed decisions. Needs base_url and an API key; otherwise the built-in heuristic decides.",
        "enabled" if path.starts_with("router") => "Pick the model per request automatically instead of always using the default.",
        "enabled" => "Whether this provider may be used.",
        "planner" => "Providers tried in order for the planning step (comma separated). Skipped automatically if not set up.",
        "executor" => "Providers tried in order to carry out the request (comma separated).",
        "fallback_any" => "If every listed provider is unavailable, use any other provider that is set up.",
        "simple_model" if path.starts_with("pipeline") => "Model for simple requests when the executor is Claude Code (alias such as haiku).",
        "complex_model" if path.starts_with("pipeline") => "Model for complex requests when the executor is Claude Code (alias such as sonnet).",
        "path" => "Jev endpoint path appended to base_url.",
        "auth" => "How the Jev key is sent: bearer (Authorization: Bearer) or x-api-key.",
        "timeout_ms" => "How long to wait for Jev before the built-in heuristic decides instead.",
        "min_confidence" => "Ignore Jev answers less confident than this (0 to 1); the heuristic decides instead.",
        "long_prompt_chars" => "Prompts longer than this count as 'complex' for the router.",
        "simple_provider" | "complex_provider" => "Provider the router picks for simple / complex requests.",
        "simple_model" | "complex_model" => "Model the router picks for simple / complex requests.",
        "kind" => "anthropic | openai (any OpenAI-compatible API) | claude-cli | hermes-api (Hermes chat endpoint) | hermes-runs (Hermes agent runs: live tool progress + approvals) | hermes-cli (runs `hermes -z`).",
        "base_url" => "API endpoint root.",
        "api_key_env" => "Name of the environment variable that holds the key. To save the key itself, press s here (TUI) or run `aiws key set`: it goes to aiws' private key file, no shell setup needed.",
        "default_model" => "Model used when none is requested. Empty = provider's own default.",
        "command" => "Executable to run for claude-cli / hermes-cli kinds (claude, hermes or a full path).",
        "input_per_mtok" => "USD per million input tokens for this model prefix.",
        "output_per_mtok" => "USD per million output tokens for this model prefix.",
        _ => "",
    }
}
