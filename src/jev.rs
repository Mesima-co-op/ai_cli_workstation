//! Request "verdicts": is this simple or complex, does it need a plan, is it risky?
//!
//! Two sources:
//!   * Jev (TypeSafe AI), a typed-decision model. NOTE: Jev's API is not publicly
//!     documented, so the request/response shape below is an *assumption* based on
//!     the vendor's description ("state + typed questions in, choices/scores out").
//!     Everything is configurable under [jev], parsing is lenient, and any failure
//!     falls back to the heuristic, so a wrong guess never breaks a request.
//!   * A built-in heuristic (always available).

use crate::config::{Config, JevCfg};
use crate::provider::Msg;
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct Verdict {
    pub complex: bool,
    pub needs_plan: bool,
    pub risky: bool,
    /// "jev" or "heuristic"
    pub source: &'static str,
    /// Why Jev was not used (empty when it was, or when it is simply switched off).
    pub note: String,
}

const HARD_WORDS: &[&str] = &[
    "refactor", "architecture", "debug", "prove", "optimi", "design", "migrate", "security", "race condition",
    "step by step", "analy", "implement", "build ",
];
const PLAN_WORDS: &[&str] = &[
    "plan", "implement", "build ", "refactor", "design", "migrate", "architecture", "set up", "setup", "step by step",
    "create a", "write a", "deploy", "fix the", "all the", "across",
];
const RISK_WORDS: &[&str] = &[
    "delete", "remove", "rm ", "drop ", "truncate", "force", "production", "overwrite", "format ", "wipe", "reset --hard",
    "uninstall",
];

pub fn heuristic(cfg: &Config, system: Option<&str>, messages: &[Msg]) -> Verdict {
    let total: usize = messages.iter().map(|m| m.content.chars().count()).sum::<usize>() + system.map(|s| s.chars().count()).unwrap_or(0);
    let last = messages.last().map(|m| m.content.to_lowercase()).unwrap_or_default();
    let long = total as u64 > cfg.router.long_prompt_chars;
    let hard = HARD_WORDS.iter().any(|w| last.contains(w)) || last.contains("```");
    let complex = long || hard;
    let needs_plan = complex && (long || PLAN_WORDS.iter().any(|w| last.contains(w)));
    let risky = RISK_WORDS.iter().any(|w| last.contains(w));
    Verdict { complex, needs_plan, risky, source: "heuristic", note: String::new() }
}

/// None when Jev can be used, otherwise the reason it cannot.
pub fn inactive_reason(j: &JevCfg) -> Option<String> {
    if !j.enabled {
        return Some("jev.enabled is false".into());
    }
    if j.base_url.trim().is_empty() {
        return Some("jev.base_url is empty".into());
    }
    if crate::secrets::get(&j.api_key_env).is_none() {
        return Some(format!("no key: run `aiws key set {}`", crate::config::env_label(&j.api_key_env)));
    }
    None
}

pub async fn decide(cfg: &Config, http: &reqwest::Client, system: Option<&str>, messages: &[Msg]) -> Verdict {
    let h = heuristic(cfg, system, messages);
    if inactive_reason(&cfg.jev).is_some() {
        return h;
    }
    match ask(cfg, http, system, messages, &h).await {
        Ok(v) => v,
        Err(e) => Verdict { note: format!("Jev unavailable ({e:#}); heuristic used"), ..h },
    }
}

async fn ask(cfg: &Config, http: &reqwest::Client, system: Option<&str>, messages: &[Msg], fallback: &Verdict) -> Result<Verdict> {
    let j = &cfg.jev;
    let key = crate::secrets::get(&j.api_key_env).ok_or_else(|| anyhow!("no API key"))?;
    let last: String = messages.last().map(|m| m.content.chars().take(6000).collect()).unwrap_or_default();
    let body = json!({
        "input": last,
        "context": {
            "system": system.map(|s| s.chars().take(1000).collect::<String>()),
            "turns": messages.len(),
        },
        "questions": [
            {"id": "complexity", "type": "choice", "options": ["simple", "complex"]},
            {"id": "needs_plan", "type": "boolean"},
            {"id": "risk", "type": "choice", "options": ["low", "high"]},
        ],
    });
    let path = if j.path.starts_with('/') { j.path.clone() } else { format!("/{}", j.path) };
    let mut rb = http
        .post(format!("{}{}", j.base_url.trim_end_matches('/'), path))
        .timeout(Duration::from_millis(j.timeout_ms.max(100)))
        .json(&body);
    rb = if j.auth == "x-api-key" { rb.header("x-api-key", key) } else { rb.bearer_auth(key) };
    let resp = rb.send().await.context("request failed")?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        bail!("HTTP {status}");
    }
    let v: Value = serde_json::from_str(&text).context("invalid JSON")?;
    interpret(&v, j.min_confidence, fallback)
}

/// Pull (value, confidence) for a question id out of several plausible shapes.
fn answer(v: &Value, id: &str) -> Option<(Value, f64)> {
    let root = v.get("answers").or_else(|| v.get("results")).or_else(|| v.get("decisions")).unwrap_or(v);
    let item = match root {
        Value::Object(m) => m.get(id)?.clone(),
        Value::Array(a) => a.iter().find(|x| x.get("id").and_then(|i| i.as_str()) == Some(id))?.clone(),
        _ => return None,
    };
    match &item {
        Value::Object(o) => {
            let val = ["choice", "value", "answer", "label"].iter().find_map(|k| o.get(*k)).cloned()?;
            let conf = ["confidence", "probability", "score"]
                .iter()
                .find_map(|k| o.get(*k).and_then(|x| x.as_f64()))
                .or_else(|| {
                    ["probabilities", "scores"].iter().find_map(|k| {
                        o.get(*k).and_then(|p| p.as_object()).map(|m| m.values().filter_map(|x| x.as_f64()).fold(0.0, f64::max))
                    })
                })
                .unwrap_or(1.0);
            Some((val, conf))
        }
        other => Some((other.clone(), 1.0)),
    }
}

fn as_bool(v: &Value) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        Value::String(s) => match s.to_lowercase().as_str() {
            "true" | "yes" => Some(true),
            "false" | "no" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn interpret(v: &Value, min_conf: f64, fb: &Verdict) -> Result<Verdict> {
    let mut used = 0;
    let mut out = Verdict { source: "jev", note: String::new(), ..fb.clone() };
    if let Some((val, c)) = answer(v, "complexity") {
        if c >= min_conf {
            if let Some(s) = val.as_str() {
                out.complex = s.eq_ignore_ascii_case("complex");
                used += 1;
            }
        }
    }
    if let Some((val, c)) = answer(v, "needs_plan") {
        if c >= min_conf {
            if let Some(b) = as_bool(&val) {
                out.needs_plan = b;
                used += 1;
            }
        }
    }
    if let Some((val, c)) = answer(v, "risk") {
        if c >= min_conf {
            if let Some(s) = val.as_str() {
                out.risky = s.eq_ignore_ascii_case("high");
                used += 1;
            }
        }
    }
    if used == 0 {
        bail!("no usable confident answers in the response");
    }
    if !out.complex {
        out.needs_plan = false; // never plan trivial requests
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fb() -> Verdict {
        Verdict { complex: false, needs_plan: false, risky: false, source: "heuristic", note: String::new() }
    }

    #[test]
    fn parses_object_shape_with_probabilities() {
        let v = json!({"answers": {
            "complexity": {"choice": "complex", "probabilities": {"simple": 0.1, "complex": 0.9}},
            "needs_plan": {"value": true, "probability": 0.8},
            "risk": "high"
        }});
        let r = interpret(&v, 0.6, &fb()).unwrap();
        assert!(r.complex && r.needs_plan && r.risky);
        assert_eq!(r.source, "jev");
    }

    #[test]
    fn parses_array_shape_and_ignores_low_confidence() {
        let v = json!({"results": [
            {"id": "complexity", "value": "complex", "confidence": 0.3},
            {"id": "risk", "value": "high", "confidence": 0.95}
        ]});
        let r = interpret(&v, 0.6, &fb()).unwrap();
        assert!(!r.complex, "low-confidence answer must fall back to the heuristic value");
        assert!(r.risky);
    }

    #[test]
    fn useless_response_is_an_error() {
        assert!(interpret(&json!({"hello": 1}), 0.6, &fb()).is_err());
    }

    #[test]
    fn heuristic_flags_plan_and_risk() {
        let cfg = Config::default();
        let m = |t: &str| vec![Msg { role: "user".into(), content: t.into() }];
        let v = heuristic(&cfg, None, &m("what is 2+2"));
        assert!(!v.complex && !v.needs_plan);
        let v = heuristic(&cfg, None, &m("refactor the auth module and delete the old files"));
        assert!(v.complex && v.needs_plan && v.risky);
    }
}
