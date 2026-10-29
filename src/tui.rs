//! Terminal UI: Dashboard, Requests (inspect), Chat, Config (edit), Help.
//! Works against the local engine or a remote daemon through `Source`.

use crate::config::{self, Config, Item};
use crate::engine::ChatRequest;
use crate::stream::{self, StreamEvent};
use crate::provider::Msg;
use crate::source::Source;
use crate::store::{ModelRow, RequestRow, Snapshot, Totals};
use anyhow::Result;
use chrono::{DateTime, Local};
use crate::theme::{self, UiPrefs, bordered, theme};
use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind};
use std::cell::Cell;
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Cell as TCell, Clear, Gauge, Paragraph, Row, Scrollbar, ScrollbarOrientation, ScrollbarState, Sparkline, Table, TableState, Tabs, Wrap},
};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

pub async fn run(src: Source, start_tab: Option<String>) -> Result<()> {
    let handle = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        let mut app = App::new(src, handle);
        if let Some(t) = start_tab {
            if let Some((tab, _)) = TABS.iter().find(|(_, label)| label.to_lowercase().contains(&t.to_lowercase())) {
                app.tab = *tab;
            }
        }
        theme::set(&app.ui);
        let mut terminal = ratatui::init();
        if app.ui.mouse {
            let _ = crossterm::execute!(std::io::stdout(), EnableMouseCapture);
        }
        let res = event_loop(&mut terminal, &mut app);
        let _ = crossterm::execute!(std::io::stdout(), DisableMouseCapture);
        ratatui::restore();
        res
    })
    .await?
}

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Dashboard,
    Requests,
    Chat,
    Config,
    Settings,
    Help,
}
const TABS: [(Tab, &str); 6] = [
    (Tab::Dashboard, "1 Dashboard"),
    (Tab::Requests, "2 Requests"),
    (Tab::Chat, "3 Chat"),
    (Tab::Config, "4 Config"),
    (Tab::Settings, "5 Settings"),
    (Tab::Help, "6 Help"),
];

enum Event2 {
    Snapshot(Result<Snapshot, String>),
    Stream(StreamEvent),
    Config(Result<Config, String>),
    Saved(Result<Config, String>),
}

struct ChatLine {
    role: String, // user | assistant | error
    text: String,
    meta: Option<String>,
    tools: Vec<String>,
}

struct App {
    src: Source,
    handle: tokio::runtime::Handle,
    tx: Sender<Event2>,
    rx: Receiver<Event2>,
    quit: bool,
    tab: Tab,

    snap: Option<Snapshot>,
    snap_err: Option<String>,
    refreshing: bool,
    last_refresh: Instant,

    sel_id: Option<i64>,
    filter: String,
    filtering: bool,
    detail_scroll: u16,

    chat: Vec<ChatLine>,
    chat_input: String,
    chat_pending: bool,
    chat_off: Cell<usize>,
    chat_max: Cell<usize>,
    chat_anchor: Cell<Option<usize>>,
    chat_provider: Option<String>,
    live_text: String,
    live_tools: Vec<String>,
    streamed: bool,
    approval: Option<(String, String)>,
    chat_tasks: Vec<tokio::task::JoinHandle<()>>,

    roles: Vec<(String, String, bool)>,
    key_state: std::collections::HashMap<String, crate::secrets::KeySource>,
    key_edit: Option<String>,
    ui: UiPrefs,
    set_sel: usize,
    set_edit: Option<String>,
    set_msg: Option<(bool, String)>,

    cfg: Option<Config>,
    items: Vec<Item>,
    cfg_sel: usize,
    editing: Option<String>,
    cfg_msg: Option<(bool, String)>,
}

impl App {
    fn new(src: Source, handle: tokio::runtime::Handle) -> Self {
        let (tx, rx) = channel();
        Self {
            src, handle, tx, rx, quit: false, tab: Tab::Dashboard,
            snap: None, snap_err: None, refreshing: false, last_refresh: Instant::now() - Duration::from_secs(60),
            sel_id: None, filter: String::new(), filtering: false, detail_scroll: 0,
            chat: Vec::new(), chat_input: String::new(), chat_pending: false,
            chat_off: Cell::new(0), chat_max: Cell::new(0), chat_anchor: Cell::new(None), chat_provider: None,
            live_text: String::new(), live_tools: Vec::new(), streamed: false, approval: None, chat_tasks: Vec::new(),
            roles: Vec::new(), key_state: Default::default(), key_edit: None, ui: theme::load(), set_sel: 0, set_edit: None, set_msg: None,
            cfg: None, items: Vec::new(), cfg_sel: 0, editing: None, cfg_msg: None,
        }
    }

    fn refresh(&mut self) {
        self.refreshing = true;
        self.last_refresh = Instant::now();
        let (src, tx) = (self.src.clone(), self.tx.clone());
        self.handle.spawn(async move {
            let _ = tx.send(Event2::Snapshot(src.snapshot().await.map_err(|e| format!("{e:#}"))));
        });
    }

    fn load_config(&mut self) {
        let (src, tx) = (self.src.clone(), self.tx.clone());
        self.handle.spawn(async move {
            let _ = tx.send(Event2::Config(src.config().await.map_err(|e| format!("{e:#}"))));
        });
    }

    fn set_cfg(&mut self, c: Config) {
        self.items = config::flatten(&c);
        self.cfg_sel = self.cfg_sel.min(self.items.len().saturating_sub(1));
        self.roles = crate::check::roles(&c, self.src.is_local()).into_iter().map(|r| (r.label.to_string(), r.text, r.full)).collect();
        self.cfg = Some(c);
        self.refresh_key_state();
    }

    fn refresh_key_state(&mut self) {
        let mut m = std::collections::HashMap::new();
        for i in &self.items {
            if (i.path.ends_with("api_key_env") || i.path.ends_with("token_env")) && config::is_env_name(&i.value) {
                m.insert(i.value.clone(), crate::secrets::source(&i.value));
            }
        }
        self.key_state = m;
    }

    fn is_key_row(item: &Item) -> bool {
        item.path.ends_with("api_key_env") || item.path.ends_with("token_env")
    }

    fn key_entry(&mut self, k: KeyEvent) {
        match k.code {
            KeyCode::Esc => self.key_edit = None,
            KeyCode::Enter => {
                let value = self.key_edit.take().unwrap_or_default();
                let name = self.items.get(self.cfg_sel).map(|i| i.value.clone()).unwrap_or_default();
                self.cfg_msg = Some(match crate::secrets::set(&name, &value) {
                    Ok(()) => (true, format!("Key saved for {name} in aiws' private key file. It is used immediately; no shell setup needed.")),
                    Err(e) => (false, format!("Not saved: {e:#}")),
                });
                self.refresh_key_state();
            }
            KeyCode::Backspace => {
                if let Some(b) = &mut self.key_edit {
                    b.pop();
                }
            }
            KeyCode::Char(c) => {
                if let Some(b) = &mut self.key_edit {
                    b.push(c);
                }
            }
            _ => {}
        }
    }

    fn on_event(&mut self, ev: Event2) {
        match ev {
            Event2::Snapshot(r) => {
                self.refreshing = false;
                match r {
                    Ok(s) => {
                        self.snap = Some(s);
                        self.snap_err = None;
                        self.refresh_key_state();
                    }
                    Err(e) => self.snap_err = Some(e),
                }
            }
            Event2::Stream(ev) => self.on_stream(ev),
            Event2::Config(r) => match r {
                Ok(c) => self.set_cfg(c),
                Err(e) => self.cfg_msg = Some((false, format!("could not load config: {e}"))),
            },
            Event2::Saved(r) => match r {
                Ok(c) => {
                    self.set_cfg(c);
                    self.cfg_msg = Some((true, "Saved. Takes effect on the next request.".into()));
                }
                Err(e) => {
                    self.cfg_msg = Some((false, format!("Not saved: {e}")));
                    self.load_config();
                }
            },
        }
    }

    fn on_stream(&mut self, ev: StreamEvent) {
        if !self.chat_pending {
            return; // late event from a cancelled request
        }
        match ev {
            StreamEvent::Text { text } => {
                self.live_text.push_str(&text);
                self.streamed = true;
            }
            StreamEvent::Tool { name, state, detail } => {
                let mark = match (state.as_str(), theme().ascii) {
                    ("started", false) => "\u{2192}",
                    ("started", true) => ">",
                    ("error", false) => "\u{2717}",
                    ("error", true) => "x",
                    (_, false) => "\u{2713}",
                    (_, true) => "ok",
                };
                self.live_tools.push(format!("{mark} {name} {detail}").trim_end().to_string());
                if self.live_tools.len() > 300 {
                    self.live_tools.remove(0);
                }
            }
            StreamEvent::Status { text } => self.live_tools.push(format!("- {text}")),
            StreamEvent::Approval { id, summary } => self.approval = Some((id, summary)),
            StreamEvent::Done { response } => {
                let cost = match (response.cost_usd, response.billing.as_str()) {
                    (Some(c), "subscription") => format!("${c:.4} (subscription)"),
                    (Some(c), _) => format!("${c:.4}"),
                    _ => "cost unknown".into(),
                };
                let meta = format!(
                    "{} / {} | {} in, {} out | {} | {} ms | {}",
                    response.provider, response.model, response.input_tokens, response.output_tokens, cost, response.latency_ms, response.route_reason
                );
                let text = if response.text.is_empty() { std::mem::take(&mut self.live_text) } else { response.text };
                self.finish_chat(ChatLine { role: "assistant".into(), text, meta: Some(meta), tools: vec![] });
            }
            StreamEvent::Error { message } => {
                if !self.live_text.is_empty() {
                    let partial = std::mem::take(&mut self.live_text);
                    let tools = std::mem::take(&mut self.live_tools);
                    self.chat.push(ChatLine { role: "assistant".into(), text: partial, meta: None, tools });
                }
                self.finish_chat(ChatLine { role: "error".into(), text: message, meta: None, tools: vec![] });
            }
        }
    }

    fn finish_chat(&mut self, mut line: ChatLine) {
        line.tools = std::mem::take(&mut self.live_tools);
        self.chat.push(line);
        // Text that streamed in is already on screen; otherwise open the answer at its first line.
        if !self.streamed {
            self.chat_anchor.set(Some(self.chat.len() - 1));
        }
        self.live_text.clear();
        self.streamed = false;
        self.approval = None;
        self.chat_pending = false;
        self.chat_tasks.clear();
        self.refresh();
    }

    fn cancel_chat(&mut self) {
        if !self.chat_pending {
            return;
        }
        for t in self.chat_tasks.drain(..) {
            t.abort();
        }
        let partial = std::mem::take(&mut self.live_text);
        let tools = std::mem::take(&mut self.live_tools);
        let text = if partial.is_empty() { "(cancelled)".to_string() } else { format!("{partial}\n\n(cancelled)") };
        self.chat.push(ChatLine { role: "assistant".into(), text, meta: None, tools });
        self.streamed = false;
        self.approval = None;
        self.chat_pending = false;
    }

    fn answer_approval(&mut self, choice: &str) {
        if let Some((id, _)) = self.approval.take() {
            let (src, c) = (self.src.clone(), choice.to_string());
            self.live_tools.push(format!("- you answered: {choice}"));
            self.handle.spawn(async move {
                let _ = src.approve(&id, &c).await;
            });
        }
    }

    // ------------------------------------------------------------ requests

    fn visible(&self) -> Vec<&RequestRow> {
        let Some(s) = &self.snap else { return vec![] };
        let f = self.filter.to_lowercase();
        s.recent
            .iter()
            .filter(|r| {
                f.is_empty()
                    || [&r.provider, &r.model, &r.status, &r.error, &r.prompt, &r.response, &r.source, &r.route_reason]
                        .iter()
                        .any(|x| x.to_lowercase().contains(&f))
            })
            .collect()
    }

    fn move_sel(&mut self, delta: isize) {
        let ids: Vec<i64> = self.visible().iter().map(|r| r.id).collect();
        if ids.is_empty() {
            self.sel_id = None;
            return;
        }
        let cur = self.sel_id.and_then(|id| ids.iter().position(|x| *x == id)).unwrap_or(0) as isize;
        let next = (cur + delta).clamp(0, ids.len() as isize - 1) as usize;
        self.sel_id = Some(ids[next]);
        self.detail_scroll = 0;
    }

    // ---------------------------------------------------------------- keys

    fn on_key(&mut self, k: KeyEvent) {
        if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        if self.approval.is_some() {
            match k.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => self.answer_approval("once"),
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => self.answer_approval("deny"),
                _ => {}
            }
            return;
        }
        // Tab switching by function keys always works, even while typing.
        let fkey = match k.code {
            KeyCode::F(n @ 1..=6) => Some(TABS[(n - 1) as usize].0),
            _ => None,
        };
        if let Some(t) = fkey {
            self.go(t);
            return;
        }
        match self.tab {
            Tab::Chat => return self.key_chat(k),
            Tab::Config if self.editing.is_some() => return self.key_config_edit(k),
            Tab::Config if self.key_edit.is_some() => return self.key_entry(k),
            Tab::Settings if self.set_edit.is_some() => return self.key_settings_edit(k),
            Tab::Requests if self.filtering => return self.key_filter(k),
            _ => {}
        }
        match k.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char(c @ '1'..='6') => self.go(TABS[(c as u8 - b'1') as usize].0),
            KeyCode::Char('?') => self.go(Tab::Help),
            KeyCode::Tab => self.cycle(1),
            KeyCode::BackTab => self.cycle(-1),
            KeyCode::Char('r') => {
                self.refresh();
                self.load_config();
            }
            _ => match self.tab {
                Tab::Requests => self.key_requests(k),
                Tab::Config => self.key_config(k),
                Tab::Settings => self.key_settings(k),
                _ => {}
            },
        }
    }

    fn go(&mut self, t: Tab) {
        self.tab = t;
        if t == Tab::Config {
            self.load_config();
        }
    }

    fn cycle(&mut self, d: isize) {
        let i = TABS.iter().position(|(t, _)| *t == self.tab).unwrap_or(0) as isize;
        let n = (i + d).rem_euclid(TABS.len() as isize) as usize;
        self.go(TABS[n].0);
    }

    fn key_requests(&mut self, k: KeyEvent) {
        match k.code {
            KeyCode::Down | KeyCode::Char('j') => self.move_sel(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_sel(-1),
            KeyCode::Home | KeyCode::Char('g') => self.move_sel(-100000),
            KeyCode::End | KeyCode::Char('G') => self.move_sel(100000),
            KeyCode::PageDown => self.detail_scroll = self.detail_scroll.saturating_add(8),
            KeyCode::PageUp => self.detail_scroll = self.detail_scroll.saturating_sub(8),
            KeyCode::Char('/') => self.filtering = true,
            KeyCode::Esc => {
                self.filter.clear();
            }
            _ => {}
        }
    }

    fn key_filter(&mut self, k: KeyEvent) {
        match k.code {
            KeyCode::Enter => self.filtering = false,
            KeyCode::Esc => {
                self.filtering = false;
                self.filter.clear();
            }
            KeyCode::Backspace => {
                self.filter.pop();
            }
            KeyCode::Char(c) => self.filter.push(c),
            _ => {}
        }
    }

    fn chat_scroll(&self, up: bool, lines: usize) {
        let max = self.chat_max.get();
        let cur = self.chat_off.get().min(max);
        self.chat_off.set(if up { (cur + lines).min(max) } else { cur.saturating_sub(lines) });
    }

    fn cycle_chat_provider(&mut self) {
        let mut names: Vec<String> = vec![];
        if let Some(c) = &self.cfg {
            names = c.providers.iter().filter(|(_, p)| p.enabled).map(|(n, _)| n.clone()).collect();
        }
        let pos = self.chat_provider.as_ref().and_then(|p| names.iter().position(|n| n == p));
        self.chat_provider = match pos {
            None if self.chat_provider.is_none() => names.first().cloned(),
            Some(i) if i + 1 < names.len() => Some(names[i + 1].clone()),
            _ => None,
        };
    }

    fn key_chat(&mut self, k: KeyEvent) {
        match k.code {
            KeyCode::Esc => self.cancel_chat(),
            KeyCode::Char('x') if k.modifiers.contains(KeyModifiers::CONTROL) => self.cancel_chat(),
            KeyCode::Tab => self.cycle(1),
            KeyCode::BackTab => self.cycle(-1),
            KeyCode::Up => self.chat_scroll(true, 1),
            KeyCode::Down => self.chat_scroll(false, 1),
            KeyCode::PageUp => self.chat_scroll(true, 10),
            KeyCode::PageDown => self.chat_scroll(false, 10),
            KeyCode::Home => self.chat_off.set(self.chat_max.get()),
            KeyCode::End => self.chat_off.set(0),
            KeyCode::Char('p') if k.modifiers.contains(KeyModifiers::CONTROL) => self.cycle_chat_provider(),
            KeyCode::Char('l') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                self.chat.clear();
                self.chat_off.set(0);
            }
            KeyCode::Enter => {
                let text = self.chat_input.trim().to_string();
                if text.is_empty() || self.chat_pending {
                    return;
                }
                self.chat_input.clear();
                self.chat.push(ChatLine { role: "user".into(), text, meta: None, tools: vec![] });
                self.chat_off.set(0);
                self.chat_pending = true;
                let messages: Vec<Msg> = self
                    .chat
                    .iter()
                    .filter(|l| l.role != "error")
                    .map(|l| Msg { role: l.role.clone(), content: l.text.clone() })
                    .collect();
                let provider = self.chat_provider.clone();
                let (stx, mut srx) = tokio::sync::mpsc::unbounded_channel();
                let (src, tx) = (self.src.clone(), self.tx.clone());
                let fwd = self.handle.spawn(async move {
                    while let Some(ev) = srx.recv().await {
                        if tx.send(Event2::Stream(ev)).is_err() {
                            break;
                        }
                    }
                });
                let run = self.handle.spawn(async move {
                    let req = ChatRequest { messages, provider, source: Some("tui".into()), ..Default::default() };
                    stream::drive(src.chat_stream(req, stx.clone()), stx).await;
                });
                self.chat_tasks = vec![fwd, run];
            }
            KeyCode::Backspace => {
                self.chat_input.pop();
            }
            KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => self.chat_input.push(c),
            _ => {}
        }
    }

    // ------------------------------------------------------------ settings

    /// (label, current value, description) for each customization row.
    fn setting_rows(&self) -> Vec<(&'static str, String, &'static str)> {
        let u = &self.ui;
        vec![
            ("Theme preset", u.theme.clone(), "Starting color scheme: default, ocean, forest, sunset, mono, contrast. Sets the three colors below."),
            ("Accent color", u.accent.clone(), "Headers, selected tab, input box border. Enter or Right cycles, e types a name or #rrggbb."),
            ("Your messages", u.user_color.clone(), "Color of your lines in Chat."),
            ("AI messages", u.ai_color.clone(), "Color of the AI's lines in Chat."),
            ("Border style", u.border.clone(), "plain, rounded, double or thick box lines."),
            ("Symbols", u.symbols.clone(), "unicode or ascii, for terminals/fonts that draw symbols badly."),
            ("Refresh every (ms)", u.refresh_ms.to_string(), "How often the dashboard and request list update."),
            ("Show key hints", u.show_hints.to_string(), "The help line at the bottom of the screen."),
            ("Mouse wheel", u.mouse.to_string(), "Scroll chat and request details with the wheel. Turn off if you want to select text with the mouse."),
        ]
    }

    fn cycle_setting(&mut self, idx: usize, dir: isize) {
        fn step<T: Clone + PartialEq>(list: &[T], cur: &T, dir: isize) -> T {
            let i = list.iter().position(|x| x == cur).unwrap_or(0) as isize;
            list[(i + dir).rem_euclid(list.len() as isize) as usize].clone()
        }
        let u = &mut self.ui;
        match idx {
            0 => {
                let names: Vec<&str> = theme::PRESETS.iter().map(|p| p.0).collect();
                let next = step(&names, &u.theme.as_str(), dir).to_string();
                theme::apply_preset(u, &next);
            }
            1 | 2 | 3 => {
                let field = match idx {
                    1 => &mut u.accent,
                    2 => &mut u.user_color,
                    _ => &mut u.ai_color,
                };
                let list: Vec<&str> = theme::COLORS.to_vec();
                let next = step(&list, &field.as_str(), dir);
                *field = next.to_string();
                u.theme = "custom".into();
            }
            4 => u.border = step(&theme::BORDERS.to_vec(), &u.border.as_str(), dir).to_string(),
            5 => u.symbols = step(&theme::SYMBOLS.to_vec(), &u.symbols.as_str(), dir).to_string(),
            6 => u.refresh_ms = step(theme::REFRESH, &u.refresh_ms, dir),
            7 => u.show_hints = !u.show_hints,
            8 => {
                u.mouse = !u.mouse;
                let _ = if u.mouse {
                    crossterm::execute!(std::io::stdout(), EnableMouseCapture)
                } else {
                    crossterm::execute!(std::io::stdout(), DisableMouseCapture)
                };
            }
            _ => {}
        }
        self.persist_ui();
    }

    fn persist_ui(&mut self) {
        theme::set(&self.ui);
        self.set_msg = Some(match theme::save(&self.ui) {
            Ok(_) => (true, "Applied and saved (this machine only).".into()),
            Err(e) => (false, format!("Applied, but could not save: {e:#}")),
        });
    }

    fn key_settings(&mut self, k: KeyEvent) {
        let n = self.setting_rows().len();
        match k.code {
            KeyCode::Down | KeyCode::Char('j') => self.set_sel = (self.set_sel + 1).min(n - 1),
            KeyCode::Up | KeyCode::Char('k') => self.set_sel = self.set_sel.saturating_sub(1),
            KeyCode::Right | KeyCode::Enter | KeyCode::Char(' ') => self.cycle_setting(self.set_sel, 1),
            KeyCode::Left => self.cycle_setting(self.set_sel, -1),
            KeyCode::Char('e') if (1..=3).contains(&self.set_sel) => {
                self.set_edit = Some(String::new());
            }
            KeyCode::Char('d') => {
                self.ui = UiPrefs::default();
                self.persist_ui();
            }
            _ => {}
        }
    }

    fn key_settings_edit(&mut self, k: KeyEvent) {
        match k.code {
            KeyCode::Esc => self.set_edit = None,
            KeyCode::Enter => {
                let v = self.set_edit.take().unwrap_or_default();
                if !theme::valid_color(&v) {
                    self.set_msg = Some((false, format!("'{v}' is not a color. Use a name (cyan, lightred...) or #rrggbb.")));
                    return;
                }
                match self.set_sel {
                    1 => self.ui.accent = v,
                    2 => self.ui.user_color = v,
                    _ => self.ui.ai_color = v,
                }
                self.ui.theme = "custom".into();
                self.persist_ui();
            }
            KeyCode::Backspace => {
                if let Some(b) = &mut self.set_edit {
                    b.pop();
                }
            }
            KeyCode::Char(c) => {
                if let Some(b) = &mut self.set_edit {
                    b.push(c);
                }
            }
            _ => {}
        }
    }

    fn key_config(&mut self, k: KeyEvent) {
        let n = self.items.len();
        match k.code {
            KeyCode::Char('s') => {
                let Some(item) = self.items.get(self.cfg_sel).cloned() else { return };
                self.cfg_msg = None;
                if !self.src.is_local() {
                    self.cfg_msg = Some((false, "Keys are stored on the machine that runs aiws. Run `aiws key set` on that machine.".into()));
                } else if !Self::is_key_row(&item) {
                    self.cfg_msg = Some((false, "Select an api_key_env or token_env row, then press s to enter its key.".into()));
                } else if !config::is_env_name(&item.value) {
                    self.cfg_msg = Some((false, "First give this row a variable name (Enter), for example HERMES_API_KEY; then press s.".into()));
                } else {
                    self.key_edit = Some(String::new());
                }
            }
            KeyCode::Char('x') => {
                if let Some(item) = self.items.get(self.cfg_sel).cloned() {
                    if Self::is_key_row(&item) && self.src.is_local() {
                        self.cfg_msg = Some(match crate::secrets::remove(&item.value) {
                            Ok(true) => (true, format!("Removed the stored key for {}.", item.value)),
                            Ok(false) => (false, format!("No key stored by aiws for {} (a key set in your shell is not touched).", item.value)),
                            Err(e) => (false, format!("Could not remove: {e:#}")),
                        });
                        self.refresh_key_state();
                    }
                }
            }
            KeyCode::Down | KeyCode::Char('j') => self.cfg_sel = (self.cfg_sel + 1).min(n.saturating_sub(1)),
            KeyCode::Up | KeyCode::Char('k') => self.cfg_sel = self.cfg_sel.saturating_sub(1),
            KeyCode::Enter | KeyCode::Char('e') | KeyCode::Char(' ') => {
                let Some(item) = self.items.get(self.cfg_sel).cloned() else { return };
                self.cfg_msg = None;
                if item.value == "true" || item.value == "false" {
                    // Booleans toggle immediately.
                    let new = if item.value == "true" { "false" } else { "true" };
                    self.apply(&item.path, new);
                } else {
                    self.editing = Some(item.value);
                }
            }
            _ => {}
        }
    }

    fn key_config_edit(&mut self, k: KeyEvent) {
        match k.code {
            KeyCode::Esc => self.editing = None,
            KeyCode::Enter => {
                let buf = self.editing.take().unwrap_or_default();
                if let Some(item) = self.items.get(self.cfg_sel).cloned() {
                    self.apply(&item.path, &buf);
                }
            }
            KeyCode::Backspace => {
                if let Some(b) = &mut self.editing {
                    b.pop();
                }
            }
            KeyCode::Char(c) => {
                if let Some(b) = &mut self.editing {
                    b.push(c);
                }
            }
            _ => {}
        }
    }

    fn apply(&mut self, path: &str, raw: &str) {
        let Some(cfg) = self.cfg.clone() else { return };
        match config::set_path(&cfg, path, raw) {
            Ok(updated) => {
                let (src, tx) = (self.src.clone(), self.tx.clone());
                let u = updated.clone();
                self.handle.spawn(async move {
                    let r = src.save_config(u.clone()).await.map(|_| u).map_err(|e| format!("{e:#}"));
                    let _ = tx.send(Event2::Saved(r));
                });
            }
            Err(e) => self.cfg_msg = Some((false, format!("Not saved: {e:#}"))),
        }
    }
}

fn event_loop(terminal: &mut DefaultTerminal, app: &mut App) -> Result<()> {
    app.refresh();
    app.load_config();
    while !app.quit {
        while let Ok(ev) = app.rx.try_recv() {
            app.on_event(ev);
        }
        if !app.refreshing && app.last_refresh.elapsed() > Duration::from_millis(app.ui.refresh_ms) {
            app.refresh();
        }
        terminal.draw(|f| draw(f, app))?;
        if event::poll(Duration::from_millis(100))? {
            match event::read()? {
                Event::Key(k) if k.kind == KeyEventKind::Press => app.on_key(k),
                Event::Mouse(m) => match (m.kind, app.tab) {
                    (MouseEventKind::ScrollUp, Tab::Chat) => app.chat_scroll(true, 3),
                    (MouseEventKind::ScrollDown, Tab::Chat) => app.chat_scroll(false, 3),
                    (MouseEventKind::ScrollUp, Tab::Requests) => app.detail_scroll = app.detail_scroll.saturating_sub(3),
                    (MouseEventKind::ScrollDown, Tab::Requests) => app.detail_scroll = app.detail_scroll.saturating_add(3),
                    _ => {}
                },
                _ => {}
            }
        }
    }
    Ok(())
}

// ------------------------------------------------------------------ drawing

fn accent() -> Color {
    theme().accent
}

fn dim() -> Style {
    Style::new().fg(Color::DarkGray)
}
fn bold() -> Style {
    Style::new().add_modifier(Modifier::BOLD)
}

fn tokens(n: u64) -> String {
    match n {
        0..=9_999 => n.to_string(),
        10_000..=999_999 => format!("{:.1}k", n as f64 / 1e3),
        _ => format!("{:.2}M", n as f64 / 1e6),
    }
}

fn usd(v: f64) -> String {
    if v < 1.0 { format!("${v:.4}") } else { format!("${v:.2}") }
}

fn when(ts: i64) -> String {
    DateTime::from_timestamp_millis(ts)
        .map(|d| d.with_timezone(&Local))
        .map(|d| {
            if d.date_naive() == Local::now().date_naive() { d.format("%H:%M:%S").to_string() } else { d.format("%m-%d %H:%M").to_string() }
        })
        .unwrap_or_default()
}

fn draw(f: &mut Frame, app: &mut App) {
    let [head, body, foot] = Layout::vertical([Constraint::Length(3), Constraint::Min(5), Constraint::Length(1)]).areas(f.area());

    let titles: Vec<Line> = TABS.iter().map(|(_, t)| Line::from(*t)).collect();
    let sel = TABS.iter().position(|(t, _)| *t == app.tab).unwrap_or(0);
    let status = match (&app.snap_err, &app.snap) {
        (Some(_), _) => Span::styled(format!("{} disconnected", theme().dot()), Style::new().fg(Color::Red)),
        (None, Some(_)) => Span::styled(format!("{} live", theme().dot()), Style::new().fg(Color::Green)),
        _ => Span::styled(format!("{} connecting…", theme().dot()), Style::new().fg(Color::Yellow)),
    };
    let title = Line::from(vec![
        Span::styled(" aiws ", Style::new().fg(Color::Black).bg(accent()).add_modifier(Modifier::BOLD)),
        Span::raw(format!(" {} ", app.src.label())),
        status,
        Span::raw(" "),
    ]);
    f.render_widget(
        Tabs::new(titles)
            .select(sel)
            .highlight_style(Style::new().fg(accent()).add_modifier(Modifier::BOLD | Modifier::UNDERLINED))
            .divider(if theme().ascii { "|" } else { "│" })
            .block(bordered().title(title)),
        head,
    );

    match app.tab {
        Tab::Dashboard => draw_dashboard(f, app, body),
        Tab::Requests => draw_requests(f, app, body),
        Tab::Chat => draw_chat(f, app, body),
        Tab::Config => draw_config(f, app, body),
        Tab::Settings => draw_settings(f, app, body),
        Tab::Help => draw_help(f, body),
    }

    let hints = match app.tab {
        _ if app.filtering => "type to filter · Enter keep · Esc clear",
        Tab::Dashboard => "Tab/1-6 switch · r refresh · ? help · q quit",
        Tab::Requests => "↑↓ select · PgUp/PgDn scroll detail · / filter · Esc clear filter · g/G first/last · q quit",
        Tab::Chat => "Enter send · Esc cancel · ↑↓/PgUp/PgDn/Home/End scroll · Ctrl+P provider · Ctrl+L clear · Tab switch tab · Ctrl+C quit",
        Tab::Config if app.key_edit.is_some() => "type or paste the key · Enter save · Esc cancel",
        Tab::Config if app.editing.is_some() => "type new value · Enter save · Esc cancel",
        Tab::Config => "↑↓ select · Enter edit (booleans toggle) · s store a key (on api_key_env rows) · x remove stored key · r reload · q quit",
        Tab::Settings if app.set_edit.is_some() => "type a color name or #rrggbb · Enter apply · Esc cancel",
        Tab::Settings => "↑↓ select · Enter/→ next value · ← previous · e type custom color · d reset all · q quit",
        Tab::Help => "Tab/1-6 switch · q quit",
    };
    if theme().hints {
        f.render_widget(Paragraph::new(hints).style(dim()), foot);
    }
    if let Some((_, summary)) = &app.approval {
        draw_approval(f, summary);
    }
}

fn totals_block<'a>(title: &'a str, t: &Totals) -> Paragraph<'a> {
    let mut lines = vec![
        Line::from(vec![Span::styled(format!("{} requests", t.requests), bold()), Span::styled(format!("  ({} errors)", t.errors), if t.errors > 0 { Style::new().fg(Color::Red) } else { dim() })]),
        Line::from(format!("tokens  {} in / {} out", tokens(t.input_tokens), tokens(t.output_tokens))),
        Line::from(vec![Span::raw("API spend  "), Span::styled(usd(t.api_cost_usd), Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD))]),
        Line::from(format!("subscription (notional)  {}", usd(t.subscription_cost_usd))),
        Line::from(format!("avg latency  {:.0} ms", t.avg_latency_ms)),
    ];
    if t.unpriced > 0 {
        lines.push(Line::from(Span::styled(format!("{} API calls have no price set", t.unpriced), Style::new().fg(Color::Yellow))));
    }
    Paragraph::new(lines).block(bordered().title(title))
}

fn draw_dashboard(f: &mut Frame, app: &App, area: Rect) {
    let Some(s) = &app.snap else {
        let msg = app.snap_err.clone().unwrap_or_else(|| "Loading…".into());
        f.render_widget(Paragraph::new(msg).block(bordered().title("Dashboard")), area);
        return;
    };
    let [top, budget, bottom] = Layout::vertical([Constraint::Length(8), Constraint::Length(4), Constraint::Min(5)]).areas(area);
    let [a, b, c] = Layout::horizontal([Constraint::Fill(1); 3]).areas(top);
    f.render_widget(totals_block(" Today ", &s.today), a);
    f.render_widget(totals_block(" This month ", &s.month), b);
    f.render_widget(totals_block(" All time ", &s.all_time), c);

    // budgets
    let [g1, g2] = Layout::horizontal([Constraint::Fill(1); 2]).areas(budget);
    for (area, label, spent, cap) in [(g1, " Daily budget ", s.today.api_cost_usd, s.budget_daily_usd), (g2, " Monthly budget ", s.month.api_cost_usd, s.budget_monthly_usd)] {
        let block = bordered().title(label);
        if cap <= 0.0 {
            f.render_widget(Paragraph::new(Span::styled("no limit set — see Config › budget", dim())).block(block), area);
        } else {
            let ratio = (spent / cap).clamp(0.0, 1.0);
            let color = if ratio >= 0.9 { Color::Red } else if ratio >= 0.7 { Color::Yellow } else { Color::Green };
            f.render_widget(
                Gauge::default().block(block).ratio(ratio).gauge_style(Style::new().fg(color)).label(format!("{} of {}", usd(spent), usd(cap))),
                area,
            );
        }
    }

    let [left, right] = Layout::horizontal([Constraint::Percentage(62), Constraint::Percentage(38)]).areas(bottom);
    let rows: Vec<Row> = s.by_model.iter().map(model_row).collect();
    let header = Row::new(["Provider", "Model", "Req", "Err", "In", "Out", "Cost", "Avg ms"]).style(Style::new().fg(accent()).add_modifier(Modifier::BOLD));
    f.render_widget(
        Table::new(rows, [Constraint::Length(12), Constraint::Min(14), Constraint::Length(6), Constraint::Length(4), Constraint::Length(7), Constraint::Length(7), Constraint::Length(9), Constraint::Length(7)])
            .header(header)
            .block(bordered().title(" By model (30 days) ")),
        left,
    );
    let [spark, roles] = Layout::vertical([Constraint::Min(4), Constraint::Length(7)]).areas(right);
    f.render_widget(
        Sparkline::default().data(s.hourly.iter().copied()).style(Style::new().fg(accent())).block(bordered().title(" Requests per hour (last 24h) ")),
        spark,
    );
    let mut rl: Vec<Line> = app
        .roles
        .iter()
        .map(|(label, text, full)| {
            Line::from(vec![
                Span::styled(format!("{label:<9}"), dim()),
                Span::styled(text.clone(), Style::new().fg(if *full { Color::Green } else { Color::Yellow })),
            ])
        })
        .collect();
    if rl.is_empty() {
        rl.push(Line::from(Span::styled("loading...", dim())));
    }
    f.render_widget(Paragraph::new(rl).wrap(Wrap { trim: false }).block(bordered().title(" Pipeline roles (yellow = fallback) ")), roles);
}

fn model_row(m: &ModelRow) -> Row<'static> {
    Row::new(vec![
        TCell::from(m.provider.clone()),
        TCell::from(m.model.clone()),
        TCell::from(m.requests.to_string()),
        TCell::from(m.errors.to_string()).style(if m.errors > 0 { Style::new().fg(Color::Red) } else { dim() }),
        TCell::from(tokens(m.input_tokens)),
        TCell::from(tokens(m.output_tokens)),
        TCell::from(usd(m.cost_usd)),
        TCell::from(format!("{:.0}", m.avg_latency_ms)),
    ])
}

fn draw_requests(f: &mut Frame, app: &mut App, area: Rect) {
    let [list, detail] = Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)]).areas(area);
    let ids: Vec<i64> = app.visible().iter().map(|r| r.id).collect();
    if app.sel_id.map(|id| !ids.contains(&id)).unwrap_or(true) {
        app.sel_id = ids.first().copied();
    }
    let visible = app.visible();
    let total = app.snap.as_ref().map(|s| s.recent.len()).unwrap_or(0);
    let sel_idx = app.sel_id.and_then(|id| visible.iter().position(|r| r.id == id));

    let rows: Vec<Row> = visible
        .iter()
        .map(|r| {
            let ok = r.status == "ok";
            Row::new(vec![
                TCell::from(when(r.ts)),
                TCell::from(r.provider.clone()),
                TCell::from(r.model.clone()),
                TCell::from(format!("{}/{}", tokens(r.input_tokens), tokens(r.output_tokens))),
                TCell::from(r.cost_usd.map(usd).unwrap_or_else(|| "-".into())),
                TCell::from(format!("{}", r.latency_ms)),
                TCell::from(if ok { "ok" } else { "ERR" }).style(Style::new().fg(if ok { Color::Green } else { Color::Red })),
            ])
        })
        .collect();
    let header = Row::new(["Time", "Provider", "Model", "Tok in/out", "Cost", "ms", "St"]).style(Style::new().fg(accent()).add_modifier(Modifier::BOLD));
    let title = if app.filtering || !app.filter.is_empty() {
        format!(" Requests {}/{} — filter: {}{} ", visible.len(), total, app.filter, if app.filtering { theme().cursor() } else { "" })
    } else {
        format!(" Requests ({total}) — press / to filter ")
    };
    let mut ts = TableState::default();
    ts.select(sel_idx);
    f.render_stateful_widget(
        Table::new(rows, [Constraint::Length(11), Constraint::Length(11), Constraint::Min(10), Constraint::Length(11), Constraint::Length(8), Constraint::Length(6), Constraint::Length(4)])
            .header(header)
            .row_highlight_style(Style::new().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
            .highlight_symbol(theme().pointer())
            .block(bordered().title(title)),
        list,
        &mut ts,
    );

    let text = match sel_idx.map(|i| visible[i]) {
        None => Text::from(Span::styled("No requests yet. Send one from the Chat tab or `aiws chat \"hi\"`.", dim())),
        Some(r) => request_detail(r),
    };
    f.render_widget(
        Paragraph::new(text).wrap(Wrap { trim: false }).scroll((app.detail_scroll, 0)).block(bordered().title(" Detail ")),
        detail,
    );
}

fn kv(k: &str, v: String) -> Line<'static> {
    Line::from(vec![Span::styled(format!("{k:<10}"), dim()), Span::raw(v)])
}

fn request_detail(r: &RequestRow) -> Text<'static> {
    let mut l = vec![
        kv("id", r.id.to_string()),
        kv("time", DateTime::from_timestamp_millis(r.ts).map(|d| d.with_timezone(&Local).format("%Y-%m-%d %H:%M:%S").to_string()).unwrap_or_default()),
        kv("provider", r.provider.clone()),
        kv("model", r.model.clone()),
        kv("status", r.status.clone()),
        kv("tokens", format!("{} in / {} out", r.input_tokens, r.output_tokens)),
        kv("cost", format!("{} ({})", r.cost_usd.map(usd).unwrap_or_else(|| "unknown".into()), r.billing)),
        kv("latency", format!("{} ms", r.latency_ms)),
        kv("routed", r.route_reason.clone()),
        kv("source", r.source.clone()),
    ];
    if !r.error.is_empty() {
        l.push(Line::from(""));
        l.push(Line::from(Span::styled("Error", Style::new().fg(Color::Red).add_modifier(Modifier::BOLD))));
        l.push(Line::from(r.error.clone()));
    }
    for (title, body) in [("Prompt", &r.prompt), ("Response", &r.response)] {
        l.push(Line::from(""));
        l.push(Line::from(Span::styled(title, Style::new().fg(accent()).add_modifier(Modifier::BOLD))));
        if body.is_empty() {
            l.push(Line::from(Span::styled("(not stored)", dim())));
        }
        for line in body.lines() {
            l.push(Line::from(line.to_string()));
        }
    }
    Text::from(l)
}

fn draw_chat(f: &mut Frame, app: &App, area: Rect) {
    let t = theme();
    let [log, input] = Layout::vertical([Constraint::Min(3), Constraint::Length(3)]).areas(area);

    // Build each message separately so we know where each one starts.
    let mut blocks: Vec<Vec<Line>> = Vec::new();
    if app.chat.is_empty() {
        blocks.push(vec![Line::from(Span::styled(
            "Type a message below and press Enter. Long answers scroll: arrows, PgUp, PgDn, Home, End or the mouse wheel. Ctrl+P switches provider (claude_code, hermes...).",
            dim(),
        ))]);
    }
    for m in &app.chat {
        let (label, style) = match m.role.as_str() {
            "user" => ("you", Style::new().fg(t.user).add_modifier(Modifier::BOLD)),
            "assistant" => ("ai", Style::new().fg(t.ai).add_modifier(Modifier::BOLD)),
            _ => ("error", Style::new().fg(Color::Red).add_modifier(Modifier::BOLD)),
        };
        let mut l = vec![Line::from(Span::styled(label, style))];
        for t in &m.tools {
            l.push(Line::from(Span::styled(format!("  {t}"), dim())));
        }
        for line in m.text.lines() {
            l.push(Line::from(line.to_string()));
        }
        if let Some(meta) = &m.meta {
            l.push(Line::from(Span::styled(format!("  {meta}"), dim())));
        }
        l.push(Line::from(""));
        blocks.push(l);
    }
    if app.chat_pending {
        let mut l = vec![Line::from(Span::styled("ai", Style::new().fg(t.ai).add_modifier(Modifier::BOLD)))];
        for tl in &app.live_tools {
            l.push(Line::from(Span::styled(format!("  {tl}"), dim())));
        }
        if app.live_text.is_empty() {
            let msg = if app.approval.is_some() { "... waiting for your approval" } else { "... working" };
            l.push(Line::from(Span::styled(msg, Style::new().fg(Color::Yellow))));
        } else {
            for line in app.live_text.lines() {
                l.push(Line::from(line.to_string()));
            }
        }
        blocks.push(l);
    }

    let inner_w = log.width.saturating_sub(3).max(1); // borders + scrollbar column
    let heights: Vec<usize> = blocks
        .iter()
        .map(|b| Paragraph::new(Text::from(b.clone())).wrap(Wrap { trim: false }).line_count(inner_w))
        .collect();
    let total: usize = heights.iter().sum();
    let visible_h = log.height.saturating_sub(2) as usize;
    let max = total.saturating_sub(visible_h);
    app.chat_max.set(max);

    // A new answer starts at the top of the view, so long text is read from its beginning.
    if let Some(idx) = app.chat_anchor.take() {
        let start: usize = heights.iter().take(idx).sum();
        app.chat_off.set(max.saturating_sub(start));
    }
    let off = app.chat_off.get().min(max);
    app.chat_off.set(off);
    let top = max - off;

    let lines: Vec<Line> = blocks.into_iter().flatten().collect();
    let auto = match &app.cfg {
        Some(c) if c.pipeline.enabled => "pipeline",
        Some(c) if c.router.enabled => "router",
        _ => "default",
    };
    let provider = app.chat_provider.clone().unwrap_or_else(|| auto.to_string());
    let mut title = format!(" Chat -> {provider} ");
    if app.chat_pending {
        title.push_str("| Esc = cancel ");
    }
    if off > 0 {
        title.push_str(&format!("| {off} lines below, End = latest "));
    }
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).scroll((top as u16, 0)).block(bordered().title(title)), log);
    if max > 0 {
        let mut sb = ScrollbarState::new(max).position(top);
        f.render_stateful_widget(Scrollbar::new(ScrollbarOrientation::VerticalRight), log, &mut sb);
    }

    let inp = Paragraph::new(app.chat_input.as_str()).block(bordered().title(" Message ").border_style(Style::new().fg(accent())));
    f.render_widget(inp, input);
    let x = input.x + 1 + app.chat_input.chars().count() as u16;
    f.set_cursor_position((x.min(input.x + input.width.saturating_sub(2)), input.y + 1));
}

fn draw_approval(f: &mut Frame, summary: &str) {
    let area = f.area();
    let w = (area.width * 3 / 4).max(40).min(area.width);
    let h = 10u16.min(area.height);
    let r = Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h };
    f.render_widget(Clear, r);
    let yellow = Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD);
    let text = Text::from(vec![
        Line::from("The agent wants to do this:"),
        Line::from(""),
        Line::from(Span::styled(summary.to_string(), bold())),
        Line::from(""),
        Line::from(vec![Span::styled("[y]", yellow), Span::raw(" allow once     "), Span::styled("[n]", yellow), Span::raw(" deny (Esc)")]),
    ]);
    f.render_widget(
        Paragraph::new(text).wrap(Wrap { trim: false }).block(bordered().title(" Approval needed ").border_style(Style::new().fg(Color::Yellow))),
        r,
    );
}

fn swatch_span(i: usize, value: &str, ascii: bool) -> Span<'static> {
    if (1..=3).contains(&i) {
        Span::styled(if ascii { "## " } else { "\u{2588}\u{2588}\u{2588} " }, Style::new().fg(theme::color(value, Color::White)))
    } else {
        Span::raw("")
    }
}

fn draw_settings(f: &mut Frame, app: &App, area: Rect) {
    let t = theme();
    let [left, right] = Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)]).areas(area);
    let rows_data = app.setting_rows();
    let rows: Vec<Row> = rows_data
        .iter()
        .enumerate()
        .map(|(i, (label, value, _))| {
            let shown = if app.set_sel == i {
                app.set_edit.clone().map(|b| format!("{b}{}", t.cursor())).unwrap_or_else(|| value.clone())
            } else {
                value.clone()
            };
            Row::new(vec![TCell::from(*label), TCell::from(Line::from(vec![swatch_span(i, value, t.ascii), Span::raw(shown)]))])
        })
        .collect();
    let mut ts = TableState::default();
    ts.select(Some(app.set_sel));
    f.render_stateful_widget(
        Table::new(rows, [Constraint::Percentage(45), Constraint::Percentage(55)])
            .header(Row::new(["Option", "Value"]).style(Style::new().fg(accent()).add_modifier(Modifier::BOLD)))
            .row_highlight_style(Style::new().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
            .highlight_symbol(t.pointer())
            .block(bordered().title(" Appearance (saved on this machine) ")),
        left,
        &mut ts,
    );

    let [prev, note] = Layout::vertical([Constraint::Min(9), Constraint::Length(6)]).areas(right);
    let desc = rows_data.get(app.set_sel).map(|r| r.2).unwrap_or("");
    let mut pl = vec![
        Line::from(Span::styled("you", Style::new().fg(t.user).add_modifier(Modifier::BOLD))),
        Line::from("How do I list files by size?"),
        Line::from(Span::styled("ai", Style::new().fg(t.ai).add_modifier(Modifier::BOLD))),
        Line::from("Use `ls -S` on Linux or `Get-ChildItem | Sort Length` on PowerShell."),
        Line::from(""),
        Line::from(Span::styled(desc, dim())),
    ];
    if let Some((ok, msg)) = &app.set_msg {
        pl.push(Line::from(""));
        pl.push(Line::from(Span::styled(msg.clone(), Style::new().fg(if *ok { Color::Green } else { Color::Red }))));
    }
    f.render_widget(Paragraph::new(pl).wrap(Wrap { trim: false }).block(bordered().title(" Preview ")), prev);
    f.render_widget(
        Paragraph::new(vec![
            Line::from("Font family and size are set by your terminal, not by aiws."),
            Line::from(Span::styled("Windows Terminal: Settings > Profiles > Appearance > Font face.", dim())),
            Line::from("If symbols look broken, set Symbols to ascii."),
        ])
        .wrap(Wrap { trim: false })
        .block(bordered().title(" About fonts ")),
        note,
    );
}

fn draw_config(f: &mut Frame, app: &App, area: Rect) {
    let [table_area, help_area] = Layout::vertical([Constraint::Min(5), Constraint::Length(7)]).areas(area);
    if app.cfg.is_none() {
        let msg = app.cfg_msg.as_ref().map(|m| m.1.clone()).unwrap_or_else(|| "Loading…".into());
        f.render_widget(Paragraph::new(msg).block(bordered().title(" Config ")), area);
        return;
    }
    let local = app.src.is_local();
    let rows: Vec<Row> = app
        .items
        .iter()
        .map(|i| {
            let mut value = i.value.clone();
            let mut style = Style::new();
            if App::is_key_row(i) && local && config::is_env_name(&value) {
                let st = app.key_state.get(&value).copied().unwrap_or(crate::secrets::KeySource::None);
                let (txt, ok) = match st {
                    crate::secrets::KeySource::Shell => ("✔ key set in your shell", true),
                    crate::secrets::KeySource::Stored => ("✔ key saved in aiws", true),
                    crate::secrets::KeySource::None => ("✘ no key: select + press s", false),
                };
                value = format!("{value}  {txt}");
                style = Style::new().fg(if ok { Color::Green } else { Color::Yellow });
            } else if i.value == "true" {
                style = Style::new().fg(Color::Green);
            } else if i.value == "false" {
                style = dim();
            }
            Row::new(vec![TCell::from(i.path.clone()), TCell::from(if value.is_empty() { "(empty)".to_string() } else { value }).style(style)])
        })
        .collect();
    let mut ts = TableState::default();
    ts.select(Some(app.cfg_sel));
    f.render_stateful_widget(
        Table::new(rows, [Constraint::Percentage(45), Constraint::Percentage(55)])
            .header(Row::new(["Setting", "Value"]).style(Style::new().fg(accent()).add_modifier(Modifier::BOLD)))
            .row_highlight_style(Style::new().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
            .highlight_symbol(theme().pointer())
            .block(bordered().title(format!(" Config — {} ", if local { "local file" } else { "remote daemon" }))),
        table_area,
        &mut ts,
    );

    // The active line (key entry / edit box / status message) goes right under the name so it is never clipped.
    let extra: Option<Line> = if let Some(buf) = &app.key_edit {
        Some(Line::from(vec![
            Span::styled("key ▸ ", Style::new().fg(Color::Yellow)),
            Span::raw("*".repeat(buf.chars().count().min(60))),
            Span::styled(theme().cursor(), Style::new().fg(Color::Yellow)),
            Span::styled(format!("  ({} characters; Enter saves, Esc cancels; never shown)", buf.chars().count()), dim()),
        ]))
    } else if let Some(buf) = &app.editing {
        Some(Line::from(vec![Span::styled("edit ▸ ", Style::new().fg(Color::Yellow)), Span::raw(buf.clone()), Span::styled(theme().cursor(), Style::new().fg(Color::Yellow))]))
    } else {
        app.cfg_msg.as_ref().map(|(ok, msg)| Line::from(Span::styled(msg.clone(), Style::new().fg(if *ok { Color::Green } else { Color::Red }))))
    };
    let mut lines = vec![];
    if let Some(item) = app.items.get(app.cfg_sel) {
        lines.push(Line::from(vec![Span::styled(item.path.clone(), bold())]));
    }
    lines.extend(extra);
    if let Some(item) = app.items.get(app.cfg_sel) {
        lines.push(Line::from(Span::styled(if item.help.is_empty() { "Custom setting." } else { item.help }, dim())));
    }
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(bordered().title(" About this setting ")), help_area);
}

fn draw_help(f: &mut Frame, area: Rect) {
    let h = |s: &str| Line::from(Span::styled(s.to_string(), Style::new().fg(accent()).add_modifier(Modifier::BOLD)));
    let text = Text::from(vec![
        h("What is this?"),
        Line::from("aiws logs every model call (tokens, cost, latency, routing) so you can see what your AI usage is doing."),
        Line::from(""),
        h("Tabs"),
        Line::from("  Dashboard  today / month / all-time totals, budget bars, usage per model"),
        Line::from("  Requests   every call; select one to read its prompt, response, cost and routing reason; / to search"),
        Line::from("  Chat       talk to your configured provider; each message is logged like any other call"),
        Line::from("  Config     every setting with a description; Enter edits it, booleans toggle, saved instantly"),
        Line::from(""),
        h("Keys"),
        Line::from("  1-6 / Tab / F1-F6  switch tab     r  refresh     q  quit (Ctrl+C anywhere)"),
        Line::from("  Chat: arrows, PgUp, PgDn, Home, End or the mouse wheel scroll long answers; Ctrl+P picks the provider (Claude, Hermes...)"),
        Line::from("  Agents (hermes_runs): tool steps appear live in Chat; a popup asks y/n before risky actions; Esc cancels."),
        Line::from("  Settings tab: colors, borders, symbols, refresh rate. Fonts come from your terminal."),
        Line::from(""),
        h("Costs"),
        Line::from("  API spend      pay-per-token usage; this is what budgets limit."),
        Line::from("  Subscription   calls made through Claude Code on your plan; shown as notional value, never billed."),
        Line::from("  Unknown cost   add a price in config:  [prices.\"model-prefix\"]  input_per_mtok / output_per_mtok"),
        Line::from(""),
        h("Remote"),
        Line::from("  Run `aiws serve` on a server/container, then locally:  aiws --remote http://host:7878 tui"),
    ]);
    f.render_widget(Clear, area);
    f.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }).block(Block::new().borders(Borders::ALL).title(" Help ")), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};
    use crate::config::ENV_LOCK;

    fn screen(term: &Terminal<TestBackend>) -> String {
        let buf = term.backend().buffer();
        let w = buf.area.width as usize;
        buf.content().chunks(w).map(|r| r.iter().map(|c| c.symbol()).collect::<String>()).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn long_answer_is_scrollable() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("aiws-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        unsafe { std::env::set_var("AIWS_HOME", &dir) };
        let rt = tokio::runtime::Runtime::new().unwrap();
        let engine = crate::engine::Engine::new(dir.join("config.toml")).unwrap();
        let mut app = App::new(Source::Local(engine), rt.handle().clone());
        app.tab = Tab::Chat;
        let long: String = (1..=120).map(|i| format!("line number {i}\n")).collect();
        app.chat.push(ChatLine { role: "user".into(), text: "question".into(), meta: None, tools: vec![] });
        app.chat.push(ChatLine { role: "assistant".into(), text: long, meta: None, tools: vec![] });
        app.chat_anchor.set(Some(1));

        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        term.draw(|f| draw(f, &mut app)).unwrap();
        let first = screen(&term);
        assert!(first.contains("line number 1\n") || first.contains("line number 1 "), "answer should start at its beginning:\n{first}");
        assert!(!first.contains("line number 120"), "end of answer should be off-screen at first");
        assert!(app.chat_max.get() > 50);

        app.on_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        term.draw(|f| draw(f, &mut app)).unwrap();
        assert!(screen(&term).contains("line number 120"), "End should jump to the latest line");

        app.on_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
        term.draw(|f| draw(f, &mut app)).unwrap();
        let s = screen(&term);
        assert!(s.contains("lines below"), "title should say how far from the bottom:\n{s}");

        app.on_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        assert_eq!(app.chat_off.get(), app.chat_max.get());
    }

    #[test]
    fn settings_tab_changes_theme_and_renders() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("aiws-test2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        unsafe { std::env::set_var("AIWS_HOME", &dir) };
        let rt = tokio::runtime::Runtime::new().unwrap();
        let engine = crate::engine::Engine::new(dir.join("config.toml")).unwrap();
        let mut app = App::new(Source::Local(engine), rt.handle().clone());
        app.tab = Tab::Settings;
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)); // theme preset -> ocean
        assert_eq!(app.ui.theme, "ocean");
        app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        app.on_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE));
        for c in "#ff8800".chars() {
            app.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.ui.accent, "#ff8800");
        let mut term = Terminal::new(TestBackend::new(100, 24)).unwrap();
        term.draw(|f| draw(f, &mut app)).unwrap();
        let s = screen(&term);
        assert!(s.contains("Appearance") && s.contains("About fonts") && s.contains("#ff8800"), "{s}");
        assert!(dir.join("ui.toml").exists());
    }

    #[test]
    fn live_stream_tools_and_approval_render() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("aiws-test3-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        unsafe { std::env::set_var("AIWS_HOME", &dir) };
        let rt = tokio::runtime::Runtime::new().unwrap();
        let engine = crate::engine::Engine::new(dir.join("config.toml")).unwrap();
        let mut app = App::new(Source::Local(engine), rt.handle().clone());
        app.tab = Tab::Chat;
        app.chat_pending = true;
        app.on_stream(StreamEvent::Tool { name: "terminal".into(), state: "started".into(), detail: "ls -la".into() });
        app.on_stream(StreamEvent::Text { text: "Here are ".into() });
        app.on_stream(StreamEvent::Text { text: "the files".into() });
        let mut term = Terminal::new(TestBackend::new(90, 24)).unwrap();
        term.draw(|f| draw(f, &mut app)).unwrap();
        let s = screen(&term);
        assert!(s.contains("terminal ls -la"), "{s}");
        assert!(s.contains("Here are the files"), "{s}");
        app.on_stream(StreamEvent::Approval { id: "run1:7".into(), summary: "rm -rf build".into() });
        term.draw(|f| draw(f, &mut app)).unwrap();
        let s = screen(&term);
        assert!(s.contains("Approval needed") && s.contains("rm -rf build"), "{s}");
        // 'n' denies and removes the popup; Esc then cancels the run
        app.on_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert!(app.approval.is_none());
        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!app.chat_pending);
        assert!(app.chat.last().unwrap().text.contains("(cancelled)"));
    }

    #[test]
    fn pressing_s_on_a_key_row_stores_the_key_privately() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("aiws-test4-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        unsafe { std::env::set_var("AIWS_HOME", &dir) };
        unsafe { std::env::remove_var("HERMES_API_KEY") };
        let rt = tokio::runtime::Runtime::new().unwrap();
        let engine = crate::engine::Engine::new(dir.join("config.toml")).unwrap();
        let mut app = App::new(Source::Local(engine), rt.handle().clone());
        app.tab = Tab::Config;
        app.set_cfg(crate::config::Config::default());
        app.cfg_sel = app.items.iter().position(|i| i.path == "providers.hermes.api_key_env").unwrap();
        app.on_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
        assert!(app.key_edit.is_some());
        for c in "my-secret-key-123".chars() {
            app.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        // the key is masked on screen
        let mut term = Terminal::new(TestBackend::new(110, 28)).unwrap();
        term.draw(|f| draw(f, &mut app)).unwrap();
        let shown = screen(&term);
        assert!(!shown.contains("my-secret-key-123"), "key must never be drawn");
        assert!(shown.contains("17 characters"), "{shown}");
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(crate::secrets::get("HERMES_API_KEY").as_deref(), Some("my-secret-key-123"));
        assert_eq!(app.key_state.get("HERMES_API_KEY"), Some(&crate::secrets::KeySource::Stored));
        // and it never reaches config.toml
        let cfg_text = std::fs::read_to_string(dir.join("config.toml")).unwrap_or_default();
        assert!(!cfg_text.contains("my-secret-key-123"));
    }
}
