//! SQLite request log + aggregate queries (WAL mode, so the daemon, CLI and
//! TUI can all use the same file at once).

use anyhow::Result;
use chrono::{Datelike, Local, TimeZone};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct RequestRow {
    pub id: i64,
    pub ts: i64, // unix ms
    pub provider: String,
    pub model: String,
    pub status: String, // ok | error
    pub error: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: Option<f64>,
    pub billing: String, // api | subscription
    pub latency_ms: u64,
    pub route_reason: String,
    pub source: String, // cli | tui | api
    pub prompt: String,
    pub response: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Totals {
    pub requests: u64,
    pub errors: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Pay-per-token spend (counts against budgets).
    pub api_cost_usd: f64,
    /// Notional value of subscription usage as reported by Claude Code.
    pub subscription_cost_usd: f64,
    /// API requests that had no price configured (cost unknown).
    pub unpriced: u64,
    pub avg_latency_ms: f64,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct ModelRow {
    pub provider: String,
    pub model: String,
    pub requests: u64,
    pub errors: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
    pub avg_latency_ms: f64,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Snapshot {
    pub today: Totals,
    pub month: Totals,
    pub all_time: Totals,
    pub by_model: Vec<ModelRow>, // last 30 days
    pub hourly: Vec<u64>,        // requests per hour, last 24h, oldest first
    pub recent: Vec<RequestRow>,
    pub budget_daily_usd: f64,
    pub budget_monthly_usd: f64,
}

pub fn start_of_today_ms() -> i64 {
    let now = Local::now();
    Local
        .with_ymd_and_hms(now.year(), now.month(), now.day(), 0, 0, 0)
        .single()
        .map(|d| d.timestamp_millis())
        .unwrap_or(0)
}

pub fn start_of_month_ms() -> i64 {
    let now = Local::now();
    Local
        .with_ymd_and_hms(now.year(), now.month(), 1, 0, 0, 0)
        .single()
        .map(|d| d.timestamp_millis())
        .unwrap_or(0)
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS requests (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                ts INTEGER NOT NULL,
                provider TEXT NOT NULL,
                model TEXT NOT NULL,
                status TEXT NOT NULL,
                error TEXT NOT NULL DEFAULT '',
                input_tokens INTEGER NOT NULL DEFAULT 0,
                output_tokens INTEGER NOT NULL DEFAULT 0,
                cost_usd REAL,
                billing TEXT NOT NULL DEFAULT 'api',
                latency_ms INTEGER NOT NULL DEFAULT 0,
                route_reason TEXT NOT NULL DEFAULT '',
                source TEXT NOT NULL DEFAULT '',
                prompt TEXT NOT NULL DEFAULT '',
                response TEXT NOT NULL DEFAULT ''
            );
            CREATE INDEX IF NOT EXISTS idx_requests_ts ON requests(ts);",
        )?;
        Ok(Self { conn: Arc::new(Mutex::new(conn)) })
    }

    pub fn insert(&self, r: &RequestRow) -> Result<i64> {
        let c = self.conn.lock().unwrap();
        c.execute(
            "INSERT INTO requests (ts,provider,model,status,error,input_tokens,output_tokens,cost_usd,billing,latency_ms,route_reason,source,prompt,response)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
            params![
                r.ts, r.provider, r.model, r.status, r.error, r.input_tokens as i64, r.output_tokens as i64,
                r.cost_usd, r.billing, r.latency_ms as i64, r.route_reason, r.source, r.prompt, r.response
            ],
        )?;
        Ok(c.last_insert_rowid())
    }

    fn map_row(row: &rusqlite::Row) -> rusqlite::Result<RequestRow> {
        Ok(RequestRow {
            id: row.get(0)?,
            ts: row.get(1)?,
            provider: row.get(2)?,
            model: row.get(3)?,
            status: row.get(4)?,
            error: row.get(5)?,
            input_tokens: row.get::<_, i64>(6)? as u64,
            output_tokens: row.get::<_, i64>(7)? as u64,
            cost_usd: row.get(8)?,
            billing: row.get(9)?,
            latency_ms: row.get::<_, i64>(10)? as u64,
            route_reason: row.get(11)?,
            source: row.get(12)?,
            prompt: row.get(13)?,
            response: row.get(14)?,
        })
    }

    const COLS: &'static str =
        "id,ts,provider,model,status,error,input_tokens,output_tokens,cost_usd,billing,latency_ms,route_reason,source,prompt,response";

    pub fn recent(&self, limit: usize) -> Result<Vec<RequestRow>> {
        let c = self.conn.lock().unwrap();
        let mut st = c.prepare(&format!("SELECT {} FROM requests ORDER BY id DESC LIMIT ?1", Self::COLS))?;
        let rows = st.query_map([limit as i64], Self::map_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn get(&self, id: i64) -> Result<Option<RequestRow>> {
        let c = self.conn.lock().unwrap();
        let mut st = c.prepare(&format!("SELECT {} FROM requests WHERE id = ?1", Self::COLS))?;
        let mut it = st.query_map([id], Self::map_row)?;
        Ok(it.next().transpose()?)
    }

    pub fn totals_since(&self, since_ms: i64) -> Result<Totals> {
        let c = self.conn.lock().unwrap();
        let t = c.query_row(
            "SELECT COUNT(*),
                    COALESCE(SUM(status='error'),0),
                    COALESCE(SUM(input_tokens),0),
                    COALESCE(SUM(output_tokens),0),
                    COALESCE(SUM(CASE WHEN billing='api' THEN cost_usd END),0),
                    COALESCE(SUM(CASE WHEN billing='subscription' THEN cost_usd END),0),
                    COALESCE(SUM(billing='api' AND status='ok' AND cost_usd IS NULL),0),
                    COALESCE(AVG(CASE WHEN status='ok' THEN latency_ms END),0)
             FROM requests WHERE ts >= ?1",
            [since_ms],
            |r| {
                Ok(Totals {
                    requests: r.get::<_, i64>(0)? as u64,
                    errors: r.get::<_, i64>(1)? as u64,
                    input_tokens: r.get::<_, i64>(2)? as u64,
                    output_tokens: r.get::<_, i64>(3)? as u64,
                    api_cost_usd: r.get(4)?,
                    subscription_cost_usd: r.get(5)?,
                    unpriced: r.get::<_, i64>(6)? as u64,
                    avg_latency_ms: r.get(7)?,
                })
            },
        )?;
        Ok(t)
    }

    pub fn by_model_since(&self, since_ms: i64) -> Result<Vec<ModelRow>> {
        let c = self.conn.lock().unwrap();
        let mut st = c.prepare(
            "SELECT provider, model, COUNT(*), COALESCE(SUM(status='error'),0),
                    COALESCE(SUM(input_tokens),0), COALESCE(SUM(output_tokens),0),
                    COALESCE(SUM(cost_usd),0), COALESCE(AVG(CASE WHEN status='ok' THEN latency_ms END),0)
             FROM requests WHERE ts >= ?1 GROUP BY provider, model ORDER BY COUNT(*) DESC",
        )?;
        let rows = st
            .query_map([since_ms], |r| {
                Ok(ModelRow {
                    provider: r.get(0)?,
                    model: r.get(1)?,
                    requests: r.get::<_, i64>(2)? as u64,
                    errors: r.get::<_, i64>(3)? as u64,
                    input_tokens: r.get::<_, i64>(4)? as u64,
                    output_tokens: r.get::<_, i64>(5)? as u64,
                    cost_usd: r.get(6)?,
                    avg_latency_ms: r.get(7)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn hourly_24h(&self) -> Result<Vec<u64>> {
        let now = Local::now().timestamp_millis();
        let hour = 3_600_000i64;
        let start = now - 24 * hour;
        let c = self.conn.lock().unwrap();
        let mut st = c.prepare("SELECT ts FROM requests WHERE ts >= ?1")?;
        let mut buckets = vec![0u64; 24];
        for ts in st.query_map([start], |r| r.get::<_, i64>(0))? {
            let idx = (((ts? - start) / hour) as usize).min(23);
            buckets[idx] += 1;
        }
        Ok(buckets)
    }

    pub fn snapshot(&self, daily: f64, monthly: f64, recent: usize) -> Result<Snapshot> {
        let day = Local::now().timestamp_millis() - 30 * 86_400_000;
        let mut recent_rows = self.recent(recent)?;
        for r in &mut recent_rows {
            // Keep snapshots small; full text is available per request.
            r.prompt = clip(&r.prompt, 4000);
            r.response = clip(&r.response, 4000);
        }
        Ok(Snapshot {
            today: self.totals_since(start_of_today_ms())?,
            month: self.totals_since(start_of_month_ms())?,
            all_time: self.totals_since(0)?,
            by_model: self.by_model_since(day)?,
            hourly: self.hourly_24h()?,
            recent: recent_rows,
            budget_daily_usd: daily,
            budget_monthly_usd: monthly,
        })
    }
}

pub fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max).collect();
        t.push('…');
        t
    }
}
