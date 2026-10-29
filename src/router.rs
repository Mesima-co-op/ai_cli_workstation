//! Simple/complex routing: turns a `Verdict` (from Jev or the heuristic) into a
//! provider + model choice using the [router] section.

use crate::config::Config;
use crate::jev::Verdict;

pub struct Decision {
    pub provider: String,
    pub model: String,
    pub reason: String,
}

pub fn pick(cfg: &Config, v: &Verdict) -> Decision {
    let r = &cfg.router;
    let (provider, model, kind) = if v.complex {
        (r.complex_provider.clone(), r.complex_model.clone(), "complex")
    } else {
        (r.simple_provider.clone(), r.simple_model.clone(), "simple")
    };
    let note = if v.note.is_empty() { String::new() } else { format!(" [{}]", v.note) };
    Decision { provider, model, reason: format!("{}: {kind}{note}", v.source) }
}
