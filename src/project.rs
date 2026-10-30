//! Project scopes: which folders the AI may change or read, what it must not do,
//! a fresh git branch for every piece of work, and a secret scan afterwards.
//!
//! Enforcement is only as strong as the tool underneath. For Claude Code the
//! limits are passed as real tool rules (`--disallowedTools`, `--add-dir`), so
//! the model cannot override them. Hermes runs its own tools on its own terms,
//! so aiws refuses to use it as the executor while a project is active.
//! The branch step and the secret scan run in aiws itself, after the model is done.

use crate::config::ProjectCfg;
use anyhow::{Result, anyhow, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Always denied inside a project, on top of the project's own `deny` list.
pub const BASE_DENY: &[&str] = &[
    // no merging, no history rewriting, no pushing to the main branches
    "Bash(git merge:*)",
    "Bash(git rebase:*)",
    "Bash(git reset --hard:*)",
    "Bash(git clean:*)",
    "Bash(git push --force:*)",
    "Bash(git push -f:*)",
    "Bash(git push origin main:*)",
    "Bash(git push origin master:*)",
    "Bash(git branch -D:*)",
    "Bash(git branch -d:*)",
    // no deleting
    "Bash(git rm:*)",
    "Bash(rm:*)",
    "Bash(rmdir:*)",
    "Bash(del:*)",
    "Bash(erase:*)",
    "Bash(rd:*)",
    "Bash(Remove-Item:*)",
];

pub struct Scope {
    pub name: String,
    pub work: PathBuf,
    pub writable: Vec<PathBuf>,
    pub context: Vec<PathBuf>,
    pub deny: Vec<String>,
    pub allow: Vec<String>,
    /// Branch the work is happening on (primary folder), if it is a git repo.
    pub branch: Option<String>,
    pub block_on_secret: bool,
    protect: Vec<String>,
    /// `git status` of each read-only repo before the run, to spot changes made during it.
    ctx_before: Vec<(PathBuf, Option<String>)>,
    /// Lines to tell the user (branch created, folder skipped, ...).
    pub notes: Vec<String>,
}

#[derive(Default, Debug)]
pub struct Report {
    /// Secrets or protected files in the changes. Never contains the secret itself.
    pub findings: Vec<String>,
    /// Read-only folders that changed during the run.
    pub context_changed: Vec<String>,
    /// Files changed in the writable folders.
    pub changed: usize,
}

pub fn deny_rules(cfg: &ProjectCfg) -> Vec<String> {
    let mut v: Vec<String> = BASE_DENY.iter().map(|s| s.to_string()).collect();
    v.extend(cfg.deny.iter().cloned());
    for p in &cfg.protect {
        v.push(format!("Read(**/{p})"));
        v.push(format!("Edit(**/{p})"));
    }
    v
}

fn abs(p: &str) -> Result<PathBuf> {
    let p = p.trim();
    if p.is_empty() {
        bail!("empty folder path");
    }
    let expanded = match p.strip_prefix("~/") {
        Some(rest) => dirs::home_dir().ok_or_else(|| anyhow!("no home directory"))?.join(rest),
        None => PathBuf::from(p),
    };
    let a = std::path::absolute(&expanded)?;
    if !a.is_dir() {
        bail!("folder not found: {}", a.display());
    }
    Ok(a)
}

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).to_string())
}

fn is_repo(dir: &Path) -> bool {
    git(dir, &["rev-parse", "--is-inside-work-tree"]).map(|s| s.trim() == "true").unwrap_or(false)
}

fn current_branch(dir: &Path) -> Option<String> {
    git(dir, &["rev-parse", "--abbrev-ref", "HEAD"]).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn slug(text: &str) -> String {
    let words: Vec<String> = text
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| w.len() > 2)
        .take(3)
        .map(|w| w.to_ascii_lowercase())
        .collect();
    words.join("-")
}

/// Make sure the folder is on an aiws branch: reuse one, or create a fresh one.
fn ensure_branch(dir: &Path, prefix: &str, prompt: &str) -> Result<(String, bool)> {
    let cur = current_branch(dir).ok_or_else(|| anyhow!("{} has no commits yet; make a first commit so aiws can branch from it", dir.display()))?;
    if cur.starts_with(prefix) {
        return Ok((cur, false));
    }
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let s = slug(prompt);
    let name = if s.is_empty() { format!("{prefix}{stamp}") } else { format!("{prefix}{stamp}-{s}") };
    let out = Command::new("git").arg("-C").arg(dir).args(["checkout", "-b", &name]).output()?;
    if !out.status.success() {
        bail!("could not create branch {name} in {}: {}", dir.display(), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok((name, true))
}

/// Resolve the folders, switch to a new branch, and remember the state of read-only folders.
pub fn prepare(name: &str, cfg: &ProjectCfg, prompt: &str) -> Result<Scope> {
    let writable: Vec<PathBuf> = cfg.folders.iter().map(|f| abs(f)).collect::<Result<_>>()?;
    let mut notes = Vec::new();
    let mut context = Vec::new();
    for f in &cfg.context {
        match abs(f) {
            Ok(p) => context.push(p),
            Err(e) => notes.push(format!("context folder skipped: {e:#}")),
        }
    }
    let mut branch = None;
    for (i, dir) in writable.iter().enumerate() {
        if !is_repo(dir) {
            notes.push(format!("{} is not a git repository: no branch or diff checks there", dir.display()));
            continue;
        }
        if cfg.create_branch {
            let (b, created) = ensure_branch(dir, &cfg.branch_prefix, prompt)?;
            if created {
                notes.push(format!("created branch {b} in {}", dir.display()));
            }
            if i == 0 {
                branch = Some(b);
            }
        } else if i == 0 {
            branch = current_branch(dir);
        }
    }
    let ctx_before = context.iter().map(|d| (d.clone(), is_repo(d).then(|| status(d)).flatten())).collect();
    for d in &context {
        if !is_repo(d) {
            notes.push(format!("{} is not a git repository: read-only cannot be verified there", d.display()));
        }
    }
    Ok(Scope {
        name: name.into(),
        work: writable[0].clone(),
        writable,
        context,
        deny: deny_rules(cfg),
        allow: cfg.allow.clone(),
        branch,
        block_on_secret: cfg.on_secret == "block",
        protect: cfg.protect.clone(),
        ctx_before,
        notes,
    })
}

impl Scope {
    /// Told to the model so it knows where it is and what it may touch.
    pub fn prompt(&self) -> String {
        let list = |v: &[PathBuf]| v.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ");
        let mut s = format!("Project '{}'. You may change files only in: {}.", self.name, list(&self.writable));
        if !self.context.is_empty() {
            s.push_str(&format!(" These folders are read-only context, never edit them: {}.", list(&self.context)));
        }
        if let Some(b) = &self.branch {
            s.push_str(&format!(" You are on branch '{b}'. Commit here; never switch to or modify main, and never merge or delete anything."));
        }
        s.push_str(" Never read, print or write secrets (.env files, keys, tokens).");
        s
    }
}

fn status(dir: &Path) -> Option<String> {
    git(dir, &["status", "--porcelain", "-uall"])
}

/// Look at what changed in the writable folders and whether read-only ones stayed untouched.
pub fn finish(scope: &Scope) -> Report {
    let mut r = Report::default();
    for dir in scope.writable.iter().filter(|d| is_repo(d)) {
        let st = status(dir).unwrap_or_default();
        let files: Vec<(String, bool)> = st
            .lines()
            .filter(|l| l.len() > 3)
            .map(|l| (l[3..].trim().trim_matches('"').to_string(), l.starts_with("??")))
            .collect();
        r.changed += files.len();
        let label = |f: &str| if scope.writable.len() > 1 { format!("{}: {f}", dir.display()) } else { f.to_string() };
        for (f, _) in &files {
            if scope.protect.iter().any(|p| glob_match(p, f)) {
                r.findings.push(format!("protected file changed: {}", label(f)));
            }
        }
        // added lines in tracked files
        if let Some(diff) = git(dir, &["diff", "HEAD", "-U0", "--no-color"]) {
            let mut file = String::new();
            for line in diff.lines() {
                if let Some(f) = line.strip_prefix("+++ b/") {
                    file = f.to_string();
                } else if let Some(added) = line.strip_prefix('+') {
                    if !line.starts_with("+++") {
                        if let Some(kind) = scan_line(added) {
                            r.findings.push(format!("possible {kind} in {}", label(&file)));
                        }
                    }
                }
            }
        }
        // new files are not in the diff: read them
        for (f, untracked) in &files {
            if !untracked {
                continue;
            }
            let path = dir.join(f);
            if std::fs::metadata(&path).map(|m| m.len() > 512 * 1024).unwrap_or(true) {
                continue;
            }
            if let Ok(text) = std::fs::read_to_string(&path) {
                if let Some(kind) = text.lines().find_map(scan_line) {
                    r.findings.push(format!("possible {kind} in new file {}", label(f)));
                }
            }
        }
    }
    for (dir, before) in &scope.ctx_before {
        if let Some(b) = before {
            if status(dir).as_deref() != Some(b.as_str()) {
                r.context_changed.push(dir.display().to_string());
            }
        }
    }
    r.findings.sort();
    r.findings.dedup();
    r
}

/// Match `*` wildcards against a file name, or against the whole path when the pattern has a `/`.
pub fn glob_match(pat: &str, path: &str) -> bool {
    let path = path.replace('\\', "/");
    let pat = pat.trim_start_matches("**/").trim_start_matches("./");
    let target = if pat.contains('/') { path.as_str() } else { path.rsplit('/').next().unwrap_or("") };
    let (p, t): (Vec<char>, Vec<char>) = (pat.chars().collect(), target.chars().collect());
    let (mut pi, mut ti, mut star, mut mark) = (0, 0, None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == t[ti] || p[pi] == '?') {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

fn run_after(line: &str, prefix: &str, min: usize, ok: fn(char) -> bool) -> bool {
    line.match_indices(prefix).any(|(i, _)| line[i + prefix.len()..].chars().take_while(|c| ok(*c)).count() >= min)
}

/// A cheap check for credentials in one line of text. Returns what it looks like, never the value.
pub fn scan_line(line: &str) -> Option<&'static str> {
    if line.len() > 2000 {
        return None;
    }
    let alnum = |c: char| c.is_ascii_alphanumeric();
    let tokenish = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
    if line.contains("-----BEGIN") && line.contains("PRIVATE KEY-----") {
        return Some("private key");
    }
    if run_after(line, "AKIA", 16, |c| c.is_ascii_uppercase() || c.is_ascii_digit()) {
        return Some("AWS access key");
    }
    if run_after(line, "sk-ant-", 20, tokenish) || run_after(line, "sk-proj-", 20, tokenish) || run_after(line, "sk-", 32, alnum) {
        return Some("API key");
    }
    if run_after(line, "ghp_", 30, alnum) || run_after(line, "gho_", 30, alnum) || run_after(line, "github_pat_", 30, tokenish) {
        return Some("GitHub token");
    }
    if run_after(line, "xoxb-", 10, tokenish) || run_after(line, "xoxp-", 10, tokenish) || run_after(line, "xoxa-", 10, tokenish) {
        return Some("Slack token");
    }
    if run_after(line, "AIza", 35, tokenish) {
        return Some("Google API key");
    }
    // name = "long random-looking value"
    let lower = line.to_ascii_lowercase();
    if ["api_key", "apikey", "secret", "token", "password", "passwd", "private_key"].iter().any(|k| lower.contains(k)) {
        if let Some(pos) = line.find(['=', ':']) {
            let val = line[pos + 1..].trim().trim_end_matches([',', ';']).trim_matches(['"', '\'']);
            let placeholder = ["${", "$(", "<", "your", "example", "xxxx", "env(", "getenv", "changeme", "placeholder"].iter().any(|p| val.to_ascii_lowercase().contains(p));
            if val.len() >= 20
                && !val.contains(' ')
                && !placeholder
                && val.chars().any(|c| c.is_ascii_digit())
                && val.chars().any(|c| c.is_ascii_alphabetic())
            {
                return Some("secret value");
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(dir: &Path, args: &[&str]) {
        let ok = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap().status.success();
        assert!(ok, "git {args:?} failed");
    }

    fn temp_repo(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("aiws-proj-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        sh(&d, &["init", "-q", "-b", "main"]);
        sh(&d, &["config", "user.email", "t@example.com"]);
        sh(&d, &["config", "user.name", "t"]);
        std::fs::write(d.join("a.txt"), "hello\n").unwrap();
        sh(&d, &["add", "-A"]);
        sh(&d, &["commit", "-q", "-m", "first"]);
        d
    }

    #[test]
    fn detects_credentials_but_not_placeholders() {
        assert_eq!(scan_line("key = \"AKIAABCDEFGHIJKLMNOP\""), Some("AWS access key"));
        assert_eq!(scan_line("-----BEGIN RSA PRIVATE KEY-----"), Some("private key"));
        assert!(scan_line("api_key = \"a1b2c3d4e5f6g7h8i9j0k1l2\"").is_some());
        assert!(scan_line("api_key = std::env::var(\"API_KEY\")").is_none());
        assert!(scan_line("password: ${DB_PASSWORD_FROM_ENV_VAR_X}").is_none());
        assert!(scan_line("let token_count = 5;").is_none());
    }

    #[test]
    fn globs_match_names_and_paths() {
        assert!(glob_match(".env*", "src/.env.local"));
        assert!(glob_match("*.pem", "keys/server.pem"));
        assert!(glob_match("secrets.env", "secrets.env"));
        assert!(!glob_match("*.pem", "keys/server.txt"));
        assert!(glob_match("config/*.toml", "config/app.toml"));
    }

    #[test]
    fn deny_rules_cover_merge_delete_and_protected_files() {
        let r = deny_rules(&ProjectCfg::default());
        for want in ["Bash(git merge:*)", "Bash(rm:*)", "Read(**/.env*)", "Edit(**/*.pem)"] {
            assert!(r.iter().any(|x| x == want), "missing {want}");
        }
    }

    #[test]
    fn creates_a_branch_then_reuses_it_and_flags_a_secret() {
        let repo = temp_repo("work");
        let ctx = temp_repo("ctx");
        let cfg = ProjectCfg { folders: vec![repo.display().to_string()], context: vec![ctx.display().to_string()], ..Default::default() };

        let s = prepare("demo", &cfg, "add a login page").unwrap();
        let b = s.branch.clone().unwrap();
        assert!(b.starts_with("aiws/") && b.ends_with("add-login-page"), "{b}");
        assert_eq!(current_branch(&repo).unwrap(), b);
        let again = prepare("demo", &cfg, "something else").unwrap();
        assert_eq!(again.branch.unwrap(), b, "an existing aiws branch is reused");

        std::fs::write(repo.join("conf.rs"), "const K: &str = \"sk-ant-api03-abcdefghijklmnopqrstuvwxyz\";\n").unwrap();
        std::fs::write(repo.join(".env"), "A=1\n").unwrap();
        std::fs::write(ctx.join("oops.txt"), "edited\n").unwrap();
        let r = finish(&s);
        assert!(r.findings.iter().any(|f| f.contains("API key") && f.contains("conf.rs")), "{:?}", r.findings);
        assert!(r.findings.iter().any(|f| f.contains("protected file changed: .env")), "{:?}", r.findings);
        assert_eq!(r.context_changed.len(), 1);
        assert!(!r.findings.iter().any(|f| f.contains("abcdefghijkl")), "the secret itself must never be reported");

        let _ = std::fs::remove_dir_all(&repo);
        let _ = std::fs::remove_dir_all(&ctx);
    }
}
