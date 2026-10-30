//! Request "verdicts": is this simple or complex, does it need a plan, is it risky?
//!
//! Two sources:
//!   * Jev (TypeSafe AI), a typed-decision model: POST /v1/systemone with
//!     {model, state, questions}. The request follows the vendor's documented example;
//!     the response shape is NOT documented in what we have, so parsing is lenient and
//!     any failure (with the raw response in the note) falls back to the heuristic.
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
    // Documented shape: POST /v1/systemone {model, state, questions: {id: {type, instructions}}}.
    let state = match system {
        Some(s) if !s.is_empty() => format!("System prompt:\n{}\n\nUser request:\n{last}", s.chars().take(1000).collect::<String>()),
        _ => last,
    };
    let body = json!({
        "model": j.model,
        "state": state,
        "questions": {
            "complex": {"type": "noul", "instructions": "Is the user request in `state` complex: multi-step, needing careful reasoning, design or code changes across several places? Answer false for simple questions and small tasks."},
            "needs_plan": {"type": "noul", "instructions": "Would the request in `state` benefit from writing a step-by-step plan before doing the work?"},
            "risky": {"type": "noul", "instructions": "Could carrying out the request in `state` delete data, overwrite files, or affect a production system?"},
        },
    });
    let path = if j.path == "/v1/decide" { "/v1/systemone".to_string() } else if j.path.starts_with('/') { j.path.clone() } else { format!("/{}", j.path) };
    let mut rb = http
        .post(format!("{}{}", j.base_url.trim_end_matches('/'), path))
        .timeout(Duration::from_millis(j.timeout_ms.max(100)))
        .json(&body);
    rb = if j.auth == "x-api-key" { rb.header("x-api-key", key) } else { rb.bearer_auth(key) };
    let resp = rb.send().await.context("request failed")?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        // The server's own explanation is the fastest way to fix a request-shape mismatch.
        let detail: String = text.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(300).collect();
        bail!("HTTP {status}: {detail}");
    }
    let v: Value = serde_json::from_str(&text).context("invalid JSON")?;
    // The response shape is not documented in what we have, so show it when it can't be read.
    interpret(&v, j.min_confidence, fallback).map_err(|e| {
        let raw: String = text.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(300).collect();
        anyhow!("{e}; response was: {raw}")
    })
}

/// Read a yes/no answer plus how sure it is. Accepts booleans, "yes"/"no", or a 0..1 probability.
fn truth(val: &Value, conf: f64) -> Option<(bool, f64)> {
    match val {
        Value::Number(n) => {
            let p = n.as_f64()?;
            Some((p >= 0.5, p.max(1.0 - p)))
        }
        other => as_bool(other).map(|b| (b, conf)),
    }
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
            let val = ["choice", "value", "answer", "label", "noul"].iter().find_map(|k| o.get(*k)).or_else(|| ["probability", "score"].iter().find_map(|k| o.get(*k).filter(|x| x.is_number()))).cloned()?;
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
    for (id, slot) in [("complex", &mut out.complex), ("needs_plan", &mut out.needs_plan), ("risky", &mut out.risky)] {
        if let Some((val, c)) = answer(v, id) {
            if let Some((b, c)) = truth(&val, c) {
                if c >= min_conf {
                    *slot = b;
                    used += 1;
                }
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
            "complex": {"value": true, "confidence": 0.9},
            "needs_plan": {"value": true, "probability": 0.8},
            "risky": "yes"
        }});
        let r = interpret(&v, 0.6, &fb()).unwrap();
        assert!(r.complex && r.needs_plan && r.risky);
        assert_eq!(r.source, "jev");
    }

    #[test]
    fn parses_real_systemone_response() {
        let v = json!({"model": "jev-1.14.0", "answers": {
            "complex": {"type": "noul", "noul": 0.04},
            "needs_plan": {"type": "noul", "noul": 0.2},
            "risky": {"type": "noul", "noul": 0.93}
        }, "usage": {"input_tokens": 361, "output_tokens": 54}});
        let r = interpret(&v, 0.6, &fb()).unwrap();
        assert_eq!(r.source, "jev");
        assert!(!r.complex && !r.needs_plan && r.risky);
    }

    #[test]
    fn parses_bare_probabilities() {
        let v = json!({"complex": 0.92, "needs_plan": {"probability": 0.1}, "risky": 0.55});
        let r = interpret(&v, 0.6, &fb()).unwrap();
        assert!(r.complex && !r.needs_plan);
        assert!(!r.risky, "0.55 is below the confidence floor, so the heuristic value stays");
    }

    #[test]
    fn parses_array_shape_and_ignores_low_confidence() {
        let v = json!({"results": [
            {"id": "complex", "value": true, "confidence": 0.3},
            {"id": "risky", "value": true, "confidence": 0.95}
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
