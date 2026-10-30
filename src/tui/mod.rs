//! Full-screen terminal UI: transcript with Markdown/diff/cell rendering, composer with history,
//! paste placeholders and slash popup, pickers, approvals, transcript overlay and row-diff
//! rendering (unchanged rows are never rewritten, so native text selection survives streaming).

mod render;
pub mod text;

use crate::session::SessionGoal;
use crate::term::{terminal_text, InputCancelled};
use anyhow::{anyhow, Result};
use futures_util::future::BoxFuture;
use indexmap::IndexMap;
use serde_json::Value;
use std::io::Write;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use text::basename;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

pub use text::{format_inline_markdown, format_token_count, slice_visible, visible_length, wrap_terminal, Theme};

pub const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
pub const USER_MARK: char = '\u{E000}';
const TRANSCRIPT_MAX_CHARS: usize = 160_000;
const TRANSCRIPT_KEEP_CHARS: usize = 120_000;
const LARGE_PASTE_CHAR_THRESHOLD: usize = 150;

pub mod glyph {
    pub const BRAND: &str = "◆";
    pub const ANSWER: &str = "◆";
    pub const USER: &str = "›";
    pub const THINKING: &str = "∴";
    pub const EXPLORED: &str = "○";
    pub const RAN: &str = "●";
    pub const DONE: &str = "●";
    pub const FAILED: &str = "✗";
    pub const EDIT: &str = "±";
    pub const AGENT: &str = "◈";
    pub const TODO: &str = "≡";
    pub const ALLOW: &str = "▲";
    pub const COMPACT: &str = "◌";
    pub const RESUMED: &str = "↻";
    pub const REVERTED: &str = "↺";
    pub const PROMPT: &str = "◇";
}

pub const SLASH_COMMANDS: &[&str] = &[
    "/help",
    "/setup",
    "/key",
    "/model",
    "/effort",
    "/mode",
    "/auto",
    "/yolo",
    "/view",
    "/transcript",
    "/copy",
    "/btw",
    "/side",
    "/goal",
    "/recap",
    "/skills",
    "/prompts",
    "/memories",
    "/memory",
    "/presets",
    "/mention",
    "/pair",
    "/workflow",
    "/workflows",
    "/network",
    "/brainstorm",
    "/council",
    "/dag",
    "/cd",
    "/pwd",
    "/mcp",
    "/subagents",
    "/agent",
    "/plan",
    "/revert",
    "/undo",
    "/checkpoints",
    "/todo",
    "/resume",
    "/new",
    "/delete",
    "/compact",
    "/diff",
    "/review",
    "/fork",
    "/rename",
    "/export",
    "/init",
    "/status",
    "/cost",
    "/tools",
    "/sessions",
    "/debug-config",
    "/warnings",
    "/doctor",
    "/clear",
    "/exit",
];

pub fn slash_description(cmd: &str) -> &'static str {
    match cmd {
        "/help" => "show commands & shortcuts",
        "/model" => "switch model & auto-sync limits",
        "/effort" => "set reasoning effort (low..max)",
        "/auto" => "toggle auto-approve mode for writes & shell (or /mode auto)",
        "/yolo" => "enable auto-approve mode for all tools",
        "/view" => "switch view mode: normal | verbose | transcript",
        "/transcript" => "toggle full transcript mode (or Ctrl+T)",
        "/copy" => "copy last assistant response or code block to clipboard (or Ctrl+O)",
        "/btw" => "ask an ephemeral side question without modifying history",
        "/side" => "ask an ephemeral side question (/btw alias)",
        "/goal" => "set or inspect session objective & token tracking",
        "/recap" => "summarize current session progress without compacting",
        "/skills" => "list or inspect SKILL.md skills (use $skill-name in prompt)",
        "/memories" => "manage persistent user memories across sessions",
        "/memory" => "memory store: rag index/stats/search, budget, preferences",
        "/presets" => "save or run reusable prompt presets",
        "/mention" => "attach @file mention into the composer",
        "/pair" => "pair mobile/web device via free Cloudflared tunnel (QR + link)",
        "/workflow" => "list, create, inspect, or run multi-step custom workflows",
        "/workflows" => "list or manage custom workflows (/workflow alias)",
        "/network" => "convene multi-agent Brainstorm / Council room or inspect P2P bus",
        "/dag" => "decompose a goal into a role-based todo DAG and execute it with subagents",
        "/brainstorm" => "run multi-agent Agent Network ideation session (/network alias)",
        "/council" => "convene Agent Network council debate (/network alias)",
        "/cd" => "change active workspace directory",
        "/pwd" => "show active workspace directory",
        "/mcp" => "list, check, add, remove, reload, or import-codex MCP servers",
        "/subagents" => "list spawned subagents, inspect transcripts, or run a subagent task",
        "/agent" => "spawn a subagent task or inspect subagents (/subagents)",
        "/plan" => "toggle read-only Plan Mode",
        "/mode" => "cycle ask / auto / plan / readonly (or Shift+Tab)",
        "/revert" => "rewind turn(s) & restore prompt to composer (or Esc Esc)",
        "/undo" => "rewind last turn & restore prompt to composer (or Esc Esc)",
        "/checkpoints" => "pick a previous turn checkpoint to rewind to",
        "/compact" => "summarize history to free context",
        "/diff" => "show git diff in workspace (including untracked files)",
        "/review" => "run code review on current diff, branch, or commit",
        "/resume" => "resume a session: this folder or all projects (←→ to switch)",
        "/new" => "start a fresh session",
        "/delete" => "delete a saved session by id or current session",
        "/debug-config" => "show resolved config paths, model capabilities, and settings",
        "/warnings" => "show retained MCP or runtime warnings",
        "/status" => "show session & token status",
        "/cost" => "show session cost and caps",
        "/doctor" => "verify environment & gateway /models",
        "/setup" => "run the provider / model setup wizard",
        "/key" => "replace the stored API key (input hidden)",
        "/prompts" => "list custom prompt files (.goon/prompts/*.md); run with /prompts:<name>",
        "/todo" => "show or clear the todo checklist",
        "/fork" => "fork this session into a new one",
        "/rename" => "rename the current session",
        "/export" => "export the session to a Markdown file",
        "/init" => "create or update AGENTS.md for this project",
        "/tools" => "list available tools (built-in + MCP)",
        "/sessions" => "list saved sessions",
        "/clear" => "clear the screen (history is kept)",
        "/exit" => "quit goon cli",
        _ => "",
    }
}

/// One selectable row of a tabbed picker.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PickerItem {
    pub id: String,
    pub label: String,
    pub meta: Option<String>,
    pub detail: Option<String>,
    pub search: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct PickerTab {
    pub label: String,
    pub items: Vec<PickerItem>,
    pub empty: Option<String>,
}

struct PickerState {
    title: String,
    tabs: Vec<PickerTab>,
    tab: usize,
    index: usize,
    filter: String,
    action: String,
    tx: Option<oneshot::Sender<Option<(usize, PickerItem)>>>,
}

#[derive(Clone, Debug, Default)]
pub struct QuestionOpts {
    pub esc_cancels: bool,
    pub esc_hint: Option<String>,
    pub mask: bool,
}

enum QErr {
    Cancelled,
    Esc,
    Closed,
}

struct Pending {
    id: u64,
    tx: oneshot::Sender<std::result::Result<String, QErr>>,
}

/// A decoded key press (names follow Node's `readline` keypress events).
#[derive(Clone, Debug, Default)]
pub struct Key {
    pub name: String,
    pub ctrl: bool,
    pub meta: bool,
    pub shift: bool,
    pub sequence: String,
}

impl Key {
    pub fn named(name: &str) -> Key {
        Key { name: name.into(), ..Default::default() }
    }
    fn is(&self, name: &str) -> bool {
        self.name == name
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Block {
    Normal,
    Answer,
    Reasoning,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct ParseState {
    pub block: Block,
    pub in_code: bool,
    pub in_diff: bool,
    pub answer_first: bool,
    pub last_was_user: bool,
    pub in_banner: bool,
}

impl Default for ParseState {
    fn default() -> Self {
        ParseState { block: Block::Normal, in_code: false, in_diff: false, answer_first: false, last_was_user: false, in_banner: false }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum RowKind {
    Blank,
    User,
    Answer,
    Normal,
    Heading,
    Quote,
    Rule,
    Table,
    ReasoningHead,
    Reasoning,
    CodeHead,
    Code,
    Cell,
    Child,
    Diff,
    DiffMeta,
    Box,
    Hint,
    BannerTitle,
    Banner,
}

#[derive(Clone, Debug)]
pub(crate) struct Row {
    pub kind: RowKind,
    pub pre: String,
    pub text: String,
    pub glyph: Option<String>,
    pub sign: Option<char>,
    pub level: usize,
}

pub(crate) struct BodyCache {
    pub cols: usize,
    pub rich: bool,
    pub show_reasoning: bool,
    pub source: String,
    pub state: ParseState,
    pub rows: Vec<Row>,
    pub styled: Vec<String>,
}

#[derive(Clone)]
enum EscPrime {
    Backtrack,
    Clear,
}

pub(crate) struct State {
    pub transcript: String,
    pub input: Vec<char>,
    pub cursor: usize,
    pub scroll: usize,
    pub max_scroll: usize,
    pub label: String,
    pub status: String,
    pub mode_label: String,
    pub effort_label: String,
    pub todo_label: String,
    workspace_files: Vec<String>,
    pub tokens: f64,
    pub cost_usd: f64,
    pending: Option<Pending>,
    next_pending: u64,
    pub closed: bool,
    timer: bool,
    spin_gen: u64,
    spinning: bool,
    pub spin_tick: usize,
    history: Vec<String>,
    history_index: usize,
    remember: bool,
    pub tool_states: IndexMap<String, String>,
    output_channel: Option<&'static str>,
    reasoning_source: Option<String>,
    pub turn_started_at: Option<i64>,
    reasoning_buffer: String,
    reasoning_header: Option<String>,
    reasoning_detail: Option<String>,
    explore_group: Vec<(&'static str, String)>,
    pub last_frame_rows: Vec<String>,
    pub last_cols: usize,
    pub last_rows: usize,
    pub last_cursor_seq: String,
    pub last_body_total_lines: usize,
    pub needs_full_clear: bool,
    last_paste_token: Option<String>,
    last_paste_at: i64,
    pub attached_images: IndexMap<String, String>,
    image_archive: IndexMap<String, String>,
    next_image_idx: usize,
    pub pasted_texts: IndexMap<String, String>,
    paste_archive: IndexMap<String, String>,
    recent_expanded: Vec<(String, String)>,
    next_paste_idx: usize,
    pub queued_prompts: Vec<String>,
    pub popup_index: usize,
    popup_for: String,
    esc_cancels: bool,
    pub esc_hint: String,
    turn_snapshots: Vec<String>,
    pub masked: bool,
    picker: Option<PickerState>,
    input_restored_from_revert: bool,
    esc_primed: Option<EscPrime>,
    esc_gen: u64,
    paste_gen: u64,
    pub status_notice: Option<String>,
    pub view_mode: String,
    pub in_transcript_overlay: bool,
    pub workspace_name: String,
    pub branch: String,
    pub context_window: u64,
    pub context_used: Option<u64>,
    pub goal: Option<SessionGoal>,
    pub title: String,
    pub body_cache: Option<BodyCache>,
}

type Callback0 = Arc<dyn Fn() + Send + Sync>;
type AsyncCallback0 = Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>;

#[derive(Default, Clone)]
struct Callbacks {
    on_toggle_mode: Option<Callback0>,
    on_cycle_effort: Option<Arc<dyn Fn(i32) + Send + Sync>>,
    on_toggle_transcript: Option<Arc<dyn Fn(bool) + Send + Sync>>,
    on_immediate_command: Option<Arc<dyn Fn(String) -> BoxFuture<'static, ()> + Send + Sync>>,
    on_copy: Option<AsyncCallback0>,
}

/// Side effects decided while the state lock is held and run after it is released.
enum Action {
    Cancel,
    ToggleMode,
    CycleEffort(i32),
    ToggleTranscript(bool),
    Immediate(String),
    Copy,
    Ended,
    ClipboardPaste,
}

pub type SizeFn = Box<dyn Fn() -> (usize, usize) + Send + Sync>;

pub struct Tui {
    me: Weak<Tui>,
    state: Mutex<State>,
    out: Mutex<Box<dyn Write + Send>>,
    size: SizeFn,
    cancel: Callback0,
    callbacks: Mutex<Callbacks>,
    handle: tokio::runtime::Handle,
    real_terminal: bool,
}

fn now() -> i64 {
    crate::jsutil::now_millis()
}

fn chars_of(text: &str) -> Vec<char> {
    text.chars().collect()
}

fn image_path_like(unquoted: &str) -> bool {
    re!(r"(?i)\.(?:png|jpe?g|webp|gif)$").is_match(unquoted)
        && (re!(r"^[A-Za-z]:[\\/]").is_match(unquoted) || unquoted.starts_with("\\\\") || unquoted.starts_with('/'))
        && std::path::Path::new(unquoted).exists()
}

fn unquote(trimmed: &str) -> &str {
    if trimmed.len() >= 2 && ((trimmed.starts_with('"') && trimmed.ends_with('"')) || (trimmed.starts_with('\'') && trimmed.ends_with('\''))) {
        trimmed[1..trimmed.len() - 1].trim()
    } else {
        trimmed
    }
}

impl Tui {
    /// Takes over the real terminal (alternate screen, raw mode, bracketed paste).
    pub fn start(title: &str, cancel: Callback0) -> Result<Arc<Tui>> {
        crossterm::terminal::enable_raw_mode()?;
        let size: SizeFn = Box::new(|| crossterm::terminal::size().map(|(c, r)| (c as usize, r as usize)).unwrap_or((80, 24)));
        let tui = Tui::with_io(title, cancel, Box::new(std::io::stdout()), size, true);
        let weak = Arc::downgrade(&tui);
        std::thread::Builder::new().name("goon-tui-input".into()).spawn(move || input_thread(weak))?;
        Ok(tui)
    }

    /// A UI over an arbitrary writer and size source (tests, pseudo-terminals).
    pub fn with_io(title: &str, cancel: Callback0, out: Box<dyn Write + Send>, size: SizeFn, real_terminal: bool) -> Arc<Tui> {
        let state = State {
            transcript: String::new(),
            input: vec![],
            cursor: 0,
            scroll: 0,
            max_scroll: 0,
            label: "goon> ".into(),
            status: "Ready".into(),
            mode_label: "ask".into(),
            effort_label: "default".into(),
            todo_label: String::new(),
            workspace_files: vec![],
            tokens: 0.0,
            cost_usd: 0.0,
            pending: None,
            next_pending: 1,
            closed: false,
            timer: false,
            spin_gen: 0,
            spinning: false,
            spin_tick: 0,
            history: vec![],
            history_index: 0,
            remember: false,
            tool_states: IndexMap::new(),
            output_channel: None,
            reasoning_source: None,
            turn_started_at: None,
            reasoning_buffer: String::new(),
            reasoning_header: None,
            reasoning_detail: None,
            explore_group: vec![],
            last_frame_rows: vec![],
            last_cols: 0,
            last_rows: 0,
            last_cursor_seq: String::new(),
            last_body_total_lines: 0,
            needs_full_clear: true,
            last_paste_token: None,
            last_paste_at: 0,
            attached_images: IndexMap::new(),
            image_archive: IndexMap::new(),
            next_image_idx: 1,
            pasted_texts: IndexMap::new(),
            paste_archive: IndexMap::new(),
            recent_expanded: vec![],
            next_paste_idx: 1,
            queued_prompts: vec![],
            popup_index: 0,
            popup_for: String::new(),
            esc_cancels: false,
            esc_hint: "skip".into(),
            turn_snapshots: vec![],
            masked: false,
            picker: None,
            input_restored_from_revert: false,
            esc_primed: None,
            esc_gen: 0,
            paste_gen: 0,
            status_notice: None,
            view_mode: "verbose".into(),
            in_transcript_overlay: false,
            workspace_name: String::new(),
            branch: String::new(),
            context_window: 0,
            context_used: None,
            goal: None,
            title: title.to_string(),
            body_cache: None,
        };
        let tui = Arc::new_cyclic(|me| Tui {
            me: me.clone(),
            state: Mutex::new(state),
            out: Mutex::new(out),
            size,
            cancel,
            callbacks: Mutex::new(Callbacks::default()),
            handle: tokio::runtime::Handle::current(),
            real_terminal,
        });
        // Alternate screen, mouse capture off (native selection works), alternate scroll for the
        // wheel, bracketed paste, hidden cursor.
        tui.write_raw("\x1b[?1049h\x1b[?1000l\x1b[?1006l\x1b[?1007h\x1b[?2004h\x1b[?25l");
        tui.render_now();
        tui
    }

    fn write_raw(&self, text: &str) {
        let mut out = self.out.lock().unwrap();
        let _ = out.write_all(text.as_bytes());
        let _ = out.flush();
    }

    pub fn set_on_toggle_mode(&self, f: Callback0) {
        self.callbacks.lock().unwrap().on_toggle_mode = Some(f);
    }
    pub fn set_on_cycle_effort(&self, f: Arc<dyn Fn(i32) + Send + Sync>) {
        self.callbacks.lock().unwrap().on_cycle_effort = Some(f);
    }
    pub fn set_on_toggle_transcript(&self, f: Arc<dyn Fn(bool) + Send + Sync>) {
        self.callbacks.lock().unwrap().on_toggle_transcript = Some(f);
    }
    pub fn set_on_immediate_command(&self, f: Arc<dyn Fn(String) -> BoxFuture<'static, ()> + Send + Sync>) {
        self.callbacks.lock().unwrap().on_immediate_command = Some(f);
    }
    pub fn set_on_copy(&self, f: AsyncCallback0) {
        self.callbacks.lock().unwrap().on_copy = Some(f);
    }

    fn run_actions(&self, actions: Vec<Action>) {
        let callbacks = self.callbacks.lock().unwrap().clone();
        for action in actions {
            match action {
                Action::Cancel => (self.cancel)(),
                Action::ToggleMode => {
                    if let Some(f) = &callbacks.on_toggle_mode {
                        f();
                    }
                }
                Action::CycleEffort(d) => {
                    if let Some(f) = &callbacks.on_cycle_effort {
                        f(d);
                    }
                }
                Action::ToggleTranscript(active) => {
                    if let Some(f) = &callbacks.on_toggle_transcript {
                        f(active);
                    }
                }
                Action::Immediate(cmd) => {
                    if let Some(f) = &callbacks.on_immediate_command {
                        self.handle.spawn(f(cmd));
                    }
                }
                Action::Copy => {
                    if let Some(f) = &callbacks.on_copy {
                        self.handle.spawn(f());
                    }
                }
                Action::Ended => {
                    (self.cancel)();
                    self.close();
                }
                Action::ClipboardPaste => {
                    let me = self.me.clone();
                    self.handle.spawn(async move {
                        if let Some(t) = me.upgrade() {
                            t.paste_from_system_clipboard().await;
                        }
                    });
                }
            }
        }
    }

    // ------------------------------------------------------------------ scheduling
    fn schedule(&self, st: &mut State, delay_ms: u64) {
        if st.closed || st.timer {
            return;
        }
        st.timer = true;
        let me = self.me.clone();
        self.handle.spawn(async move {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            if let Some(t) = me.upgrade() {
                let mut st = t.state.lock().unwrap();
                st.timer = false;
                t.render(&mut st);
            }
        });
    }

    pub fn render_now(&self) {
        let mut st = self.state.lock().unwrap();
        self.render(&mut st);
    }

    /// The last frame as plain rows (tests).
    pub fn rendered_frame(&self) -> String {
        self.state.lock().unwrap().last_frame_rows.join("\r\n")
    }

    pub fn transcript(&self) -> String {
        self.state.lock().unwrap().transcript.clone()
    }

    fn size(&self) -> (usize, usize) {
        (self.size)()
    }

    // ------------------------------------------------------------------ public setters
    pub fn clear(&self) {
        let mut st = self.state.lock().unwrap();
        st.transcript.clear();
        st.scroll = 0;
        st.max_scroll = 0;
        st.explore_group.clear();
        st.turn_snapshots.clear();
        st.input_restored_from_revert = false;
        st.esc_primed = None;
        st.esc_gen += 1;
        st.status_notice = None;
        st.reasoning_buffer.clear();
        st.reasoning_header = None;
        st.reasoning_detail = None;
        st.last_frame_rows.clear();
        st.last_body_total_lines = 0;
        st.needs_full_clear = true;
        self.schedule(&mut st, 16);
    }

    /// Captures the transcript before a user turn so `/revert` / Esc Esc can remove it.
    pub fn mark_turn_start(&self) {
        let mut st = self.state.lock().unwrap();
        self.flush_explore_group(&mut st);
        st.output_channel = None;
        let snap = st.transcript.clone();
        st.turn_snapshots.push(snap);
        if st.turn_snapshots.len() > 100 {
            st.turn_snapshots.remove(0);
        }
    }

    /// Rewinds the visible transcript `steps` turns; `false` means rebuild from messages.
    pub fn rewind_turns(&self, steps: usize) -> bool {
        let mut st = self.state.lock().unwrap();
        self.flush_explore_group(&mut st);
        st.output_channel = None;
        if steps > 0 && st.turn_snapshots.len() >= steps {
            let target = st.turn_snapshots.len() - steps;
            let restored = st.turn_snapshots[target].clone();
            st.turn_snapshots.truncate(target);
            st.transcript = restored;
            st.scroll = 0;
            st.max_scroll = 0;
            st.last_frame_rows.clear();
            st.last_body_total_lines = 0;
            st.needs_full_clear = true;
            self.schedule(&mut st, 0);
            return true;
        }
        st.turn_snapshots.clear();
        false
    }

    /// Restores a prompt into the composer for editing or immediate resend.
    pub fn set_input(&self, text: &str, from_revert: bool) {
        let mut st = self.state.lock().unwrap();
        st.input.clear();
        st.cursor = 0;
        st.input_restored_from_revert = from_revert;
        st.attached_images.clear();
        st.next_image_idx = 1;
        st.pasted_texts.clear();
        st.next_paste_idx = 1;
        let mut restored = text.to_string();
        let archive: Vec<(String, String)> = st.paste_archive.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        for (placeholder, content) in archive {
            if !content.is_empty() && restored.contains(&content) {
                restored = restored.replacen(&content, &placeholder, 1);
                st.pasted_texts.insert(placeholder.clone(), content);
                if let Some(c) = re!(r"^\[Pasted text #(\d+)").captures(&placeholder) {
                    st.next_paste_idx = st.next_paste_idx.max(c[1].parse::<usize>().unwrap_or(0) + 1);
                }
            }
        }
        let normalized = terminal_text(&restored.replace("\r\n", "\n").replace('\r', "\n"));
        if normalized.contains('\n') || normalized.chars().count() > LARGE_PASTE_CHAR_THRESHOLD {
            self.handle_paste(&mut st, &normalized, false);
            st.input_restored_from_revert = from_revert;
            return;
        }
        st.input = chars_of(&normalized);
        st.cursor = st.input.len();
        self.rehydrate_placeholders(&mut st);
        self.collapse_pasted_image_paths(&mut st);
        self.schedule(&mut st, 0);
    }

    pub fn insert_text(&self, text: &str) {
        let mut st = self.state.lock().unwrap();
        self.handle_paste(&mut st, text, false);
    }

    pub fn set_notice(&self, notice: Option<&str>) {
        let mut st = self.state.lock().unwrap();
        st.status_notice = notice.map(terminal_text).filter(|n| !n.is_empty() || notice.is_some());
        self.schedule(&mut st, 0);
    }

    pub fn set_title(&self, title: &str) {
        let mut st = self.state.lock().unwrap();
        st.title = terminal_text(title);
        self.schedule(&mut st, 16);
    }

    pub fn title(&self) -> String {
        self.state.lock().unwrap().title.clone()
    }

    pub fn set_mode(&self, mode: &str) {
        let mut st = self.state.lock().unwrap();
        st.mode_label = terminal_text(mode);
        // Switching to auto at a tool approval prompt approves this one request only ("always"
        // would keep the tool auto-approved after cycling on to plan/readonly/ask).
        if mode == "auto" && st.pending.is_some() && !st.remember && st.label.contains("y/N") {
            let pending = st.pending.take().unwrap();
            st.masked = false;
            let _ = pending.tx.send(Ok("y".into()));
        }
        self.schedule(&mut st, 16);
    }

    pub fn set_view_mode(&self, mode: &str) {
        let mut st = self.state.lock().unwrap();
        st.view_mode = mode.into();
        st.in_transcript_overlay = mode == "transcript";
        st.scroll = 0;
        st.needs_full_clear = true;
        self.schedule(&mut st, 0);
    }

    pub fn view_mode(&self) -> String {
        let st = self.state.lock().unwrap();
        if st.in_transcript_overlay {
            "transcript".into()
        } else {
            st.view_mode.clone()
        }
    }

    pub fn set_transcript_overlay(&self, active: bool) {
        {
            let mut st = self.state.lock().unwrap();
            st.in_transcript_overlay = active;
            if active {
                self.flush_explore_group(&mut st);
            }
            st.needs_full_clear = true;
            self.schedule(&mut st, 0);
        }
        self.run_actions(vec![Action::ToggleTranscript(active)]);
    }

    pub fn is_transcript_overlay(&self) -> bool {
        self.state.lock().unwrap().in_transcript_overlay
    }

    pub fn set_effort(&self, effort: Option<&str>) {
        let mut st = self.state.lock().unwrap();
        st.effort_label = terminal_text(effort.filter(|e| !e.is_empty()).unwrap_or("default"));
        self.schedule(&mut st, 16);
    }

    pub fn set_tokens(&self, tokens: f64) {
        let mut st = self.state.lock().unwrap();
        st.tokens = tokens;
        self.schedule(&mut st, 16);
    }

    pub fn set_cost_usd(&self, cost: f64) {
        let mut st = self.state.lock().unwrap();
        st.cost_usd = if cost.is_finite() && cost > 0.0 { cost } else { 0.0 };
        self.schedule(&mut st, 16);
    }

    pub fn set_workspace_files(&self, files: Vec<String>) {
        self.state.lock().unwrap().workspace_files = files;
    }

    /// Header identity: workspace folder name and git branch (or short detached SHA).
    pub fn set_workspace(&self, cwd: &std::path::Path, branch: Option<&str>) {
        let mut st = self.state.lock().unwrap();
        let text = terminal_text(&cwd.to_string_lossy());
        let trimmed = text.trim_end_matches(['/', '\\']).to_string();
        let name = trimmed.rsplit(['/', '\\']).next().unwrap_or("").to_string();
        st.workspace_name = if name.is_empty() { text } else { name };
        st.branch = branch.map(terminal_text).unwrap_or_default();
        self.schedule(&mut st, 16);
    }

    pub fn set_context_window(&self, tokens: u64) {
        let mut st = self.state.lock().unwrap();
        st.context_window = tokens;
        self.schedule(&mut st, 16);
    }

    /// Context occupancy; `None` hides the meter until the next provider usage report.
    pub fn set_context_usage(&self, tokens: Option<u64>) {
        let mut st = self.state.lock().unwrap();
        st.context_used = tokens;
        self.schedule(&mut st, 16);
    }

    pub fn set_goal(&self, goal: Option<SessionGoal>) {
        let mut st = self.state.lock().unwrap();
        st.goal = goal;
        self.schedule(&mut st, 16);
    }

    pub fn set_workspace_name_for_tests(&self, name: &str) {
        self.state.lock().unwrap().workspace_name = name.into();
    }

    pub fn queue_prompt(&self, text: &str) {
        if text.trim().is_empty() {
            return;
        }
        let mut st = self.state.lock().unwrap();
        st.queued_prompts.push(text.to_string());
        self.schedule(&mut st, 0);
    }

    pub fn has_queued_prompts(&self) -> bool {
        !self.state.lock().unwrap().queued_prompts.is_empty()
    }

    pub fn is_picking(&self) -> bool {
        self.state.lock().unwrap().picker.is_some()
    }

    pub fn status(&self) -> String {
        self.state.lock().unwrap().status.clone()
    }

    // ------------------------------------------------------------------ transcript
    fn flush_explore_group(&self, st: &mut State) {
        if st.explore_group.is_empty() {
            return;
        }
        let reads: Vec<String> = st.explore_group.iter().filter(|(k, _)| *k == "read").map(|(_, s)| s.clone()).collect();
        let mut child_lines = vec![];
        if !reads.is_empty() {
            child_lines.push(format!("Read {}", reads.join(", ")));
        }
        for (kind, summary) in st.explore_group.iter().filter(|(k, _)| *k != "read") {
            child_lines.push(format!("{} {summary}", if *kind == "list" { "List" } else { "Search" }));
        }
        let formatted = child_lines.iter().enumerate().map(|(i, l)| format!("  {} {l}", if i == 0 { '└' } else { ' ' })).collect::<Vec<_>>().join("\n");
        st.explore_group.clear();
        self.append_cell(st, &format!("\n{} Explored\n{formatted}\n", glyph::EXPLORED));
    }

    fn append_transcript(&self, st: &mut State, text: &str, force_bottom: bool) {
        let was_at_bottom = st.scroll == 0;
        let mut next = format!("{}{}", st.transcript, terminal_text(text));
        if next.len() > TRANSCRIPT_MAX_CHARS {
            // Trim in large steps and only at a block boundary: cutting inside a code fence used
            // to render the rest of the session as code.
            let mut from = next.len() - TRANSCRIPT_KEEP_CHARS;
            while !next.is_char_boundary(from) {
                from += 1;
            }
            let boundary = next[from..].find("\n[normal]\n").map(|i| i + from);
            let line_break = next[from..].find('\n').map(|i| i + from);
            let cut = match (boundary, line_break) {
                (Some(b), _) if b < next.len() - 1 => b + 1,
                (_, Some(l)) => l + 1,
                _ => from,
            };
            next = next[cut..].to_string();
        }
        st.transcript = next;
        if force_bottom || was_at_bottom {
            st.scroll = 0;
        }
        self.schedule(st, 16);
    }

    /// A `[normal]` marker closes any open reasoning/answer block (renders as nothing).
    fn block_reset(st: &State) -> &'static str {
        if st.transcript.is_empty() || st.transcript.ends_with('\n') {
            "[normal]\n"
        } else {
            "\n[normal]\n"
        }
    }

    fn append_cell(&self, st: &mut State, text: &str) {
        st.output_channel = None;
        let block = format!("{}{text}", Self::block_reset(st));
        self.append_transcript(st, &block, false);
    }

    pub fn write(&self, text: &str) {
        let mut st = self.state.lock().unwrap();
        // Raw @"…image.png" mentions in the prompt echo show as [Image: name.png].
        let mut cleaned = re!(r#"(?i)@"([^"]+\.(?:png|jpe?g|webp|gif))""#).replace_all(text, |c: &regex::Captures| format!("[Image: {}]", basename(&c[1]))).to_string();
        let trimmed_start = cleaned.trim_start();
        let is_user = trimmed_start.starts_with("> ") || trimmed_start.starts_with("› ");
        if is_user {
            self.flush_explore_group(&mut st);
            for (placeholder, content) in st.recent_expanded.clone() {
                if !content.is_empty() && cleaned.contains(&content) {
                    cleaned = cleaned.replacen(&content, &placeholder, 1);
                }
            }
            // Mark every line of a user turn so streamed Markdown blockquotes never render as one.
            let current = cleaned.trim_start().to_string();
            let lead = cleaned[..cleaned.len() - current.len()].to_string();
            let marker = if current.starts_with("> ") { 2 } else { "› ".len() };
            let body = current[marker..].split('\n').map(|l| if l.trim().is_empty() { l.to_string() } else { format!("{USER_MARK}{l}") }).collect::<Vec<_>>().join("\n");
            cleaned = format!("{lead}{body}");
        }
        st.output_channel = None;
        let block = format!("{}{cleaned}", Self::block_reset(&st));
        self.append_transcript(&mut st, &block, is_user);
    }

    pub async fn ask_transient(&self, block: &str, label: &str, cancel: Option<&CancellationToken>, opts: QuestionOpts) -> Result<String> {
        let (saved, appended) = {
            let mut st = self.state.lock().unwrap();
            self.flush_explore_group(&mut st);
            st.output_channel = None;
            let saved = (st.label.clone(), st.input.clone(), st.cursor, st.attached_images.clone(), st.next_image_idx, st.pasted_texts.clone(), st.next_paste_idx);
            st.input.clear();
            st.cursor = 0;
            st.attached_images.clear();
            st.pasted_texts.clear();
            let appended = format!("{}{}", Self::block_reset(&st), terminal_text(block));
            self.append_transcript(&mut st, &appended, true);
            (saved, appended)
        };
        let result = self.question(label, cancel, false, opts).await;
        let mut st = self.state.lock().unwrap();
        // Remove only the transient block: output appended meanwhile (parallel agents) stays.
        if let Some(pos) = st.transcript.rfind(&appended) {
            st.transcript.replace_range(pos..pos + appended.len(), "");
        }
        st.label = saved.0;
        st.input = saved.1;
        st.cursor = saved.2;
        st.attached_images = saved.3;
        st.next_image_idx = saved.4;
        st.pasted_texts = saved.5;
        st.next_paste_idx = saved.6;
        st.body_cache = None;
        self.schedule(&mut st, 16);
        result
    }

    pub fn event(&self, event: &Value) {
        let mut st = self.state.lock().unwrap();
        self.event_locked(&mut st, event);
        self.schedule(&mut st, 16);
    }

    fn event_locked(&self, st: &mut State, event: &Value) {
        let s = |k: &str| event.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        let first_line = |k: &str| re!(r"\r?\n").split(&s(k)).next().unwrap_or("").to_string();
        let kind = s("type");
        match kind.as_str() {
            "subagent" => {
                let state = s("state");
                let inner = event.get("event").cloned().unwrap_or(Value::Null);
                if state == "progress" && inner.get("type").and_then(Value::as_str) == Some("usage") {
                    st.tokens += inner.get("total").and_then(Value::as_f64).unwrap_or(0.0);
                    if let Some(c) = inner.get("costUsd").and_then(Value::as_f64).filter(|c| *c != 0.0) {
                        st.cost_usd += c;
                    }
                }
                let id = s("id");
                let label = if state == "progress" {
                    if inner.get("type").and_then(Value::as_str) == Some("tool") {
                        format!("{}: {}", inner.get("name").and_then(Value::as_str).unwrap_or(""), inner.get("state").and_then(Value::as_str).unwrap_or(""))
                    } else {
                        "running".into()
                    }
                } else {
                    state.clone()
                };
                st.tool_states.insert(format!("agent:{id}"), format!("agent {}: {label}", crate::jsutil::take_chars(&id, 8)));
                if state == "start" {
                    self.flush_explore_group(st);
                    self.append_cell(st, &format!("\n{} Agent {} · {}\n", glyph::AGENT, crate::jsutil::take_chars(&id, 8), crate::jsutil::slice_chars(&s("task"), 0, 160)));
                } else if state == "done" || state == "error" {
                    self.flush_explore_group(st);
                    let text = if state == "done" {
                        format!("\n{} Agent {} finished\n", glyph::AGENT, crate::jsutil::take_chars(&id, 8))
                    } else {
                        format!("\n{} Agent {} failed\n", glyph::FAILED, crate::jsutil::take_chars(&id, 8))
                    };
                    self.append_cell(st, &text);
                }
            }
            "tool_stream" => {}
            "text" | "reasoning" => {
                if kind == "text" {
                    self.flush_explore_group(st);
                }
                let text = s("text");
                if kind == "reasoning" {
                    let combined = format!("{}{text}", st.reasoning_buffer);
                    let chars: Vec<char> = combined.chars().collect();
                    st.reasoning_buffer = chars[chars.len().saturating_sub(8000)..].iter().collect();
                    let (header, detail) = text::extract_reasoning_summary(&st.reasoning_buffer);
                    if header.is_some() {
                        st.reasoning_header = header;
                    }
                    if detail.is_some() {
                        st.reasoning_detail = detail;
                    }
                }
                let source = s("source");
                if kind == "reasoning" && (st.output_channel != Some("reasoning") || st.reasoning_source.as_deref() != Some(source.as_str())) {
                    self.append_transcript(st, &format!("\n[reasoning · {source}]\n"), false);
                } else if kind == "text" && st.output_channel != Some("text") {
                    self.append_transcript(st, "\n[answer]\n", false);
                }
                if kind == "reasoning" {
                    st.reasoning_source = Some(source);
                }
                st.output_channel = Some(if kind == "text" { "text" } else { "reasoning" });
                self.append_transcript(st, &text, false);
            }
            "tool" => {
                st.output_channel = None;
                let meta = event.get("meta").cloned();
                let summary = meta.as_ref().and_then(|m| m.get("summary")).and_then(Value::as_str).filter(|s| !s.is_empty()).map(|s| format!(" {s}")).unwrap_or_default();
                let state = s("state");
                st.tool_states.insert(s("id"), format!("{}: {state}", s("name")));
                if state == "start" {
                    let verb = match meta.as_ref().and_then(|m| m.get("kind")).and_then(Value::as_str) {
                        Some("read") => "Reading".to_string(),
                        Some("list") => "Listing".into(),
                        Some("search") => "Searching".into(),
                        Some("exec") => "Running".into(),
                        Some("edit") => "Editing".into(),
                        _ => format!("Calling {}", s("name")),
                    };
                    st.reasoning_header = Some(crate::jsutil::slice_chars(&format!("{verb}{summary}"), 0, 72).to_string());
                } else if let Some(meta) = meta.filter(Value::is_object) {
                    self.record_completed_tool_cell(st, &s("name"), &state, &meta);
                }
            }
            "todo" => {
                self.flush_explore_group(st);
                let todos = event.get("todos").and_then(Value::as_array).cloned().unwrap_or_default();
                let done = todos.iter().filter(|t| t.get("status").and_then(Value::as_str) == Some("completed")).count();
                st.todo_label = if todos.is_empty() { String::new() } else { format!("{done}/{}", todos.len()) };
                let lines: Vec<String> = todos
                    .iter()
                    .map(|t| {
                        let icon = match t.get("status").and_then(Value::as_str) {
                            Some("completed") => '✓',
                            Some("in_progress") => '▸',
                            _ => '○',
                        };
                        format!("  {icon} {}", t.get("content").and_then(Value::as_str).unwrap_or(""))
                    })
                    .collect();
                if !lines.is_empty() {
                    self.append_cell(st, &format!("\n{} Todo {done}/{}\n{}\n", glyph::TODO, todos.len(), lines.join("\n")));
                }
            }
            "plan_exit" => {
                self.flush_explore_group(st);
                self.append_cell(st, &format!("\n{} Plan approved — exited plan mode\n", glyph::TODO));
            }
            "goal" => st.goal = event.get("goal").and_then(|g| serde_json::from_value(g.clone()).ok()),
            "usage" => {
                st.tokens += event.get("total").and_then(Value::as_f64).unwrap_or(0.0);
                if let Some(c) = event.get("costUsd").and_then(Value::as_f64).filter(|c| *c != 0.0) {
                    st.cost_usd += c;
                }
                // Prompt + completion of the latest request is the provider's measure of occupancy.
                let input = event.get("input").and_then(Value::as_u64).unwrap_or(0);
                if input > 0 {
                    st.context_used = Some(input + event.get("output").and_then(Value::as_u64).unwrap_or(0));
                }
            }
            "compaction" => {
                self.flush_explore_group(st);
                st.context_used = None;
                self.append_cell(st, &format!("\n{} Context compacted #{}\n", glyph::COMPACT, event.get("count").map(crate::jsutil::js_string).unwrap_or_default()));
            }
            "verify_start" => {
                self.flush_explore_group(st);
                let files = event.get("files").and_then(Value::as_array).map(|a| a.len()).unwrap_or(0);
                let commands: Vec<String> = event.get("commands").and_then(Value::as_array).map(|a| a.iter().take(3).map(crate::jsutil::js_string).collect()).unwrap_or_default();
                let commands = if commands.is_empty() { String::new() } else { format!(" · {}", commands.join(" · ")) };
                self.append_cell(st, &format!("\n{} Verify {files} file(s){}\n", glyph::RAN, crate::jsutil::slice_chars(&commands, 0, 160)));
            }
            "verify_result" => {
                let status = s("status");
                if status != "skip" {
                    self.flush_explore_group(st);
                    let g = match status.as_str() {
                        "fail" => glyph::FAILED,
                        "warn" => glyph::ALLOW,
                        _ => glyph::DONE,
                    };
                    self.append_cell(st, &format!("\n{g} Verify {status}: {}\n", first_line("reason")));
                }
            }
            "verify_rollback" => {
                self.flush_explore_group(st);
                self.append_cell(st, &format!("\n{} Verify rollback: {}\n", glyph::REVERTED, first_line("reason")));
            }
            "dag_plan_invalid" => {
                self.flush_explore_group(st);
                self.append_cell(st, &format!("\n{} DAG plan invalid: {}\n", glyph::FAILED, first_line("message")));
            }
            "dag_plan_ready" => {
                self.flush_explore_group(st);
                self.append_cell(st, &format!("\n{} DAG plan ready: {} todo(s)\n", glyph::TODO, event.get("todos").map(crate::jsutil::js_string).unwrap_or_default()));
            }
            "dag_todo_update" => {
                let status = s("status");
                let g = match status.as_str() {
                    "done" => glyph::DONE,
                    "failed" | "blocked" => glyph::FAILED,
                    _ => glyph::RAN,
                };
                self.append_cell(st, &format!("\n{g} DAG {}: {status} ({})\n", s("todoId"), s("role")));
            }
            "dag_merge" => {
                self.flush_explore_group(st);
                let verified = event.get("verified").and_then(Value::as_bool).unwrap_or(false);
                let reason = first_line("reason");
                self.append_cell(
                    st,
                    &format!(
                        "\n{} DAG merge: {}{}\n",
                        if verified { glyph::DONE } else { glyph::FAILED },
                        if verified { "verified" } else { "NOT verified" },
                        if s("reason").is_empty() { String::new() } else { format!(" — {reason}") }
                    ),
                );
            }
            "workflow_classified" => {
                self.flush_explore_group(st);
                self.append_cell(st, &format!("\n{} Workflow {}: {}\n", if s("mode") == "multi" { glyph::TODO } else { glyph::RAN }, s("mode"), first_line("reason")));
            }
            "scope_round" => {
                self.flush_explore_group(st);
                let action = s("action");
                let g = match action.as_str() {
                    "stop" => glyph::FAILED,
                    "expand" => glyph::RAN,
                    _ => glyph::DONE,
                };
                self.append_cell(st, &format!("\n{g} Scope round {} {action}: {}\n", event.get("round").map(crate::jsutil::js_string).unwrap_or_default(), first_line("reason")));
            }
            "cost_degrade" => {
                self.flush_explore_group(st);
                self.append_cell(
                    st,
                    &format!("\n{} Cost degrade: {} ({} candidate(s))\n", glyph::ALLOW, first_line("reason"), event.get("candidates").map(crate::jsutil::js_string).unwrap_or_default()),
                );
            }
            "stopped" => {
                self.flush_explore_group(st);
                self.append_cell(st, &format!("\n{} {}\n", glyph::ALLOW, s("reason")));
            }
            _ => {}
        }
    }

    fn record_completed_tool_cell(&self, st: &mut State, name: &str, state: &str, meta: &Value) {
        st.output_channel = None;
        let full = st.view_mode == "transcript" || st.in_transcript_overlay;
        let kind = meta.get("kind").and_then(Value::as_str).unwrap_or("");
        let summary = meta.get("summary").and_then(Value::as_str).unwrap_or("").to_string();
        let explore_kind: Option<&'static str> = match kind {
            "read" => Some("read"),
            "list" => Some("list"),
            "search" => Some("search"),
            _ => None,
        };
        if state == "done" && !full {
            if let Some(k) = explore_kind {
                st.explore_group.push((k, summary));
                if st.explore_group.len() >= 6 {
                    self.flush_explore_group(st);
                }
                return;
            }
        }
        self.flush_explore_group(st);
        let dur = meta.get("durationMs").and_then(Value::as_f64).map(|d| format!(" ({})", text::format_compact_duration(d as i64))).unwrap_or_default();
        if full && explore_kind.is_some() {
            let verb = match kind {
                "read" => "Read",
                "list" => "Listed",
                _ => "Searched",
            };
            let head = if state == "error" { format!("{} Failed", glyph::FAILED) } else { format!("{} {verb}", glyph::EXPLORED) };
            self.append_cell(st, &format!("\n{head} {summary}{dur}\n"));
            return;
        }
        if kind == "exec" {
            let header = if state == "error" { format!("{} Failed {summary}{dur}", glyph::FAILED) } else { format!("{} Ran {summary}{dur}", glyph::RAN) };
            let preview: Vec<String> = meta.get("outputPreview").and_then(Value::as_array).map(|a| a.iter().map(crate::jsutil::js_string).collect()).unwrap_or_default();
            let mut lines = vec![];
            if !preview.is_empty() {
                let max = if full {
                    80
                } else if st.view_mode == "verbose" {
                    15
                } else {
                    5
                };
                let shown = &preview[..preview.len().min(max)];
                for (i, l) in shown.iter().enumerate() {
                    lines.push(format!("  {} {l}", if i == 0 { '└' } else { ' ' }));
                }
                let omitted = meta.get("omittedLines").and_then(Value::as_u64).unwrap_or(0) + (preview.len() - shown.len()) as u64;
                if omitted > 0 {
                    lines.push(format!("    … +{omitted} lines"));
                }
            } else {
                lines.push("  └ (no output)".into());
            }
            self.append_cell(st, &format!("\n{header}\n{}\n", lines.join("\n")));
            return;
        }
        if kind == "edit" {
            let verb = meta.get("verb").and_then(Value::as_str).unwrap_or("Edited");
            let added = meta.get("added").and_then(Value::as_u64);
            let removed = meta.get("removed").and_then(Value::as_u64);
            let stats = if added.is_some() || removed.is_some() { format!(" (+{} -{})", added.unwrap_or(0), removed.unwrap_or(0)) } else { String::new() };
            let max = if full {
                500
            } else if st.view_mode == "verbose" {
                120
            } else {
                30
            };
            let diff_lines = meta.get("diff").and_then(Value::as_str).filter(|d| !d.is_empty()).map(|d| text::format_compact_diff(d, max)).unwrap_or_default();
            let diff_block = if diff_lines.is_empty() { String::new() } else { format!("\n{}", diff_lines.join("\n")) };
            self.append_cell(st, &format!("\n{} {verb} {summary}{stats}{diff_block}\n", if state == "error" { glyph::FAILED } else { glyph::EDIT }));
            return;
        }
        if name != "todo_write" && name != "update_plan" {
            let head = if state == "error" { format!("{} Failed", glyph::FAILED) } else { format!("{} Completed", glyph::DONE) };
            self.append_cell(st, &format!("\n{head} {summary}{dur}\n"));
        }
    }

    pub fn set_status(&self, status: &str) {
        let mut st = self.state.lock().unwrap();
        self.set_status_locked(&mut st, status);
    }

    fn set_status_locked(&self, st: &mut State, status: &str) {
        if st.closed {
            return;
        }
        st.status = terminal_text(status);
        let busy = matches!(st.status.as_str(), "Running" | "Connecting MCP" | "Compacting");
        if busy {
            if st.turn_started_at.is_none() {
                st.turn_started_at = Some(now());
            }
            if !st.spinning {
                st.spinning = true;
                st.spin_gen += 1;
                let gen = st.spin_gen;
                let me = self.me.clone();
                self.handle.spawn(async move {
                    loop {
                        tokio::time::sleep(Duration::from_millis(80)).await;
                        let Some(t) = me.upgrade() else { return };
                        let mut st = t.state.lock().unwrap();
                        if st.spin_gen != gen || st.closed {
                            return;
                        }
                        st.spin_tick = (st.spin_tick + 1) % SPINNER.len();
                        t.schedule(&mut st, 16);
                    }
                });
            }
        } else {
            self.flush_explore_group(st);
            st.turn_started_at = None;
            st.reasoning_buffer.clear();
            st.reasoning_header = None;
            st.reasoning_detail = None;
            if st.spinning {
                st.spinning = false;
                st.spin_gen += 1;
            }
        }
        st.output_channel = None;
        st.reasoning_source = None;
        if status == "Ready" {
            st.tool_states.clear();
        }
        self.schedule(st, 16);
    }

    /// Runs `work` under a temporary status and always restores the previous one afterwards.
    pub async fn with_status<T>(&self, status: &str, work: impl std::future::Future<Output = T>) -> T {
        let previous = self.status();
        self.set_status(status);
        let out = work.await;
        if !self.state.lock().unwrap().closed {
            self.set_status(if previous == status { "Ready" } else { &previous });
        }
        out
    }

    pub async fn question(&self, label: &str, cancel: Option<&CancellationToken>, remember: bool, opts: QuestionOpts) -> Result<String> {
        let (id, rx) = {
            let mut st = self.state.lock().unwrap();
            if st.closed {
                return Err(anyhow!("Terminal closed."));
            }
            if st.pending.is_some() {
                return Err(anyhow!("A terminal question is already active."));
            }
            if cancel.map(|c| c.is_cancelled()).unwrap_or(false) {
                return Err(anyhow!("This operation was aborted"));
            }
            st.label = terminal_text(label);
            // Secrets render as bullets and never enter the prompt history.
            st.masked = opts.mask && !remember;
            st.esc_cancels = opts.esc_cancels;
            st.esc_hint = opts.esc_hint.clone().unwrap_or_else(|| {
                if opts.esc_cancels {
                    "cancel".into()
                } else if label.contains("y/N") {
                    "deny".into()
                } else {
                    "skip".into()
                }
            });
            if remember && !st.queued_prompts.is_empty() {
                let next = st.queued_prompts.remove(0);
                let value = self.expand_placeholders(&mut st, &next);
                self.schedule(&mut st, 0);
                return Ok(value);
            }
            if !remember {
                st.input.clear();
                st.cursor = 0;
                st.attached_images.clear();
                st.next_image_idx = 1;
                st.pasted_texts.clear();
                st.next_paste_idx = 1;
            }
            st.history_index = st.history.len();
            st.remember = remember;
            let id = st.next_pending;
            st.next_pending += 1;
            let (tx, rx) = oneshot::channel();
            st.pending = Some(Pending { id, tx });
            self.schedule(&mut st, 16);
            (id, rx)
        };
        let never = CancellationToken::new();
        let cancel = cancel.unwrap_or(&never);
        let outcome = tokio::select! {
            r = rx => r.unwrap_or(Err(QErr::Closed)),
            _ = cancel.cancelled() => {
                let mut st = self.state.lock().unwrap();
                if st.pending.as_ref().map(|p| p.id == id).unwrap_or(false) {
                    st.pending = None;
                    st.masked = false;
                    self.schedule(&mut st, 16);
                }
                Err(QErr::Cancelled)
            }
        };
        match outcome {
            Ok(v) => Ok(v),
            Err(QErr::Cancelled) => Err(anyhow!("Input cancelled.")),
            Err(QErr::Esc) => Err(InputCancelled.into()),
            Err(QErr::Closed) => Err(anyhow!("Terminal closed.")),
        }
    }

    /// Submits a prompt or slash command from outside the keyboard (paired devices).
    pub fn submit_external_prompt(&self, text: &str) {
        let trimmed = text.trim().to_string();
        let mut actions = vec![];
        {
            let mut st = self.state.lock().unwrap();
            if trimmed.is_empty() || st.closed {
                return;
            }
            if st.pending.is_some() && st.remember {
                let pending = st.pending.take().unwrap();
                st.masked = false;
                st.scroll = 0;
                let _ = pending.tx.send(Ok(trimmed));
                self.schedule(&mut st, 0);
                return;
            }
            if (trimmed.starts_with('/') || trimmed.starts_with('!')) && self.callbacks.lock().unwrap().on_immediate_command.is_some() {
                self.schedule(&mut st, 0);
                actions.push(Action::Immediate(trimmed));
            } else {
                st.queued_prompts.push(trimmed);
                self.schedule(&mut st, 0);
            }
        }
        self.run_actions(actions);
    }

    pub async fn pick(&self, title: &str, tabs: Vec<PickerTab>, initial_tab: usize, action: &str) -> Option<(usize, PickerItem)> {
        let rx = {
            let mut st = self.state.lock().unwrap();
            if st.closed {
                return None;
            }
            if let Some(mut old) = st.picker.take() {
                if let Some(tx) = old.tx.take() {
                    let _ = tx.send(None);
                }
            }
            let (tx, rx) = oneshot::channel();
            let count = tabs.len();
            st.picker = Some(PickerState {
                title: terminal_text(title),
                tabs: tabs
                    .into_iter()
                    .map(|mut t| {
                        t.label = terminal_text(&t.label);
                        t
                    })
                    .collect(),
                tab: initial_tab.min(count.saturating_sub(1)),
                index: 0,
                filter: String::new(),
                action: action.to_string(),
                tx: Some(tx),
            });
            st.scroll = 0;
            self.schedule(&mut st, 0);
            rx
        };
        rx.await.ok().flatten()
    }

    fn resolve_picker(&self, st: &mut State, value: Option<(usize, PickerItem)>) {
        if let Some(mut p) = st.picker.take() {
            if let Some(tx) = p.tx.take() {
                let _ = tx.send(value);
            }
        }
        st.needs_full_clear = true;
        self.schedule(st, 0);
    }

    fn picker_matches(state: &PickerState, tab: usize) -> Vec<PickerItem> {
        let items = state.tabs.get(tab).map(|t| t.items.clone()).unwrap_or_default();
        let words: Vec<String> = state.filter.trim().to_lowercase().split_whitespace().map(str::to_string).collect();
        if words.is_empty() {
            return items;
        }
        items
            .into_iter()
            .filter(|item| {
                let hay = format!("{} {} {} {} {}", item.label, item.meta.clone().unwrap_or_default(), item.detail.clone().unwrap_or_default(), item.search.clone().unwrap_or_default(), item.id)
                    .to_lowercase();
                words.iter().all(|w| hay.contains(w))
            })
            .collect()
    }

    fn picker_key(&self, st: &mut State, text: Option<&str>, key: &Key) {
        let (_, rows) = self.size();
        let page = rows.saturating_sub(12).max(3);
        let state = st.picker.as_mut().unwrap();
        let matches = Self::picker_matches(state, state.tab);
        let tabs = state.tabs.len().max(1);
        let mut resolve: Option<Option<(usize, PickerItem)>> = None;
        if key.is("escape") || (key.ctrl && key.is("c")) {
            resolve = Some(None);
        } else if key.is("return") || key.is("enter") {
            if let Some(item) = matches.get(state.index) {
                resolve = Some(Some((state.tab, item.clone())));
            }
        } else if key.is("right") || (key.is("tab") && !key.shift) {
            state.tab = (state.tab + 1) % tabs;
            state.index = 0;
        } else if key.is("left") || (key.is("tab") && key.shift) || key.sequence == "\x1b[Z" {
            state.tab = (state.tab + tabs - 1) % tabs;
            state.index = 0;
        } else if key.is("up") || (key.ctrl && key.is("p")) {
            state.index = state.index.saturating_sub(1);
        } else if key.is("down") || (key.ctrl && key.is("n")) {
            state.index = (state.index + 1).min(matches.len().saturating_sub(1));
        } else if key.is("pageup") {
            state.index = state.index.saturating_sub(page);
        } else if key.is("pagedown") {
            state.index = (state.index + page).min(matches.len().saturating_sub(1));
        } else if key.is("home") {
            state.index = 0;
        } else if key.is("end") {
            state.index = matches.len().saturating_sub(1);
        } else if key.is("backspace") {
            state.filter.pop();
            state.index = 0;
        } else if key.ctrl && key.is("u") {
            state.filter.clear();
            state.index = 0;
        } else if let Some(t) = text.filter(|t| !key.ctrl && !key.meta && !t.contains(['\r', '\n'])) {
            let next = format!("{}{}", state.filter, terminal_text(t));
            state.filter = crate::jsutil::take_chars(&next, 120).to_string();
            state.index = 0;
        }
        match resolve {
            Some(v) => self.resolve_picker(st, v),
            None => self.schedule(st, 0),
        }
    }

    // ------------------------------------------------------------------ paste & placeholders
    fn record_image_archive(st: &mut State, placeholder: &str, path: &str) {
        st.image_archive.insert(placeholder.into(), path.into());
        if st.image_archive.len() > 100 {
            st.image_archive.shift_remove_index(0);
        }
    }

    fn record_paste_archive(st: &mut State, placeholder: &str, content: &str) {
        st.paste_archive.insert(placeholder.into(), content.into());
        if st.paste_archive.len() > 100 {
            st.paste_archive.shift_remove_index(0);
        }
    }

    /// Rehydrates placeholder maps after history recall or popping a queued prompt.
    fn rehydrate_placeholders(&self, st: &mut State) {
        let current: String = st.input.iter().collect();
        st.attached_images.clear();
        st.pasted_texts.clear();
        for c in re!(r"\[Image #(\d+)\]").captures_iter(&current) {
            let token = c[0].to_string();
            if let Some(path) = st.image_archive.get(&token).cloned() {
                st.attached_images.insert(token, path);
                st.next_image_idx = st.next_image_idx.max(c[1].parse::<usize>().unwrap_or(0) + 1);
            }
        }
        for c in re!(r"\[Pasted text #(\d+)[^\]]*\]").captures_iter(&current) {
            let token = c[0].to_string();
            if let Some(content) = st.paste_archive.get(&token).cloned() {
                st.pasted_texts.insert(token, content);
                st.next_paste_idx = st.next_paste_idx.max(c[1].parse::<usize>().unwrap_or(0) + 1);
            }
        }
    }

    /// Expands `[Image #N]` / `[Pasted text #N …]` placeholders back to their payloads.
    fn expand_placeholders(&self, st: &mut State, raw: &str) -> String {
        let value = re!(r"\[Image #\d+\]").replace_all(raw, |c: &regex::Captures| {
            let token = &c[0];
            match st.attached_images.get(token).or_else(|| st.image_archive.get(token)) {
                Some(path) => format!("@\"{path}\""),
                None => token.to_string(),
            }
        });
        // Drop the auto-appended trailing space after a final paste placeholder.
        let value = re!(r"(\[Pasted text #\d+[^\]]*\]) $").replace(&value, "${1}").to_string();
        let mut recent = vec![];
        let out = re!(r"(\[Pasted text #\d+[^\]]*\])( ?)").replace_all(&value, |c: &regex::Captures| {
            let token = c[1].to_string();
            let space = c.get(2).map(|m| m.as_str()).unwrap_or("");
            match st.pasted_texts.get(&token).or_else(|| st.paste_archive.get(&token)) {
                None => c[0].to_string(),
                Some(content) => {
                    recent.push((token, content.clone()));
                    if !space.is_empty() && content.contains('\n') && !content.ends_with('\n') {
                        format!("{content}\n")
                    } else {
                        format!("{content}{space}")
                    }
                }
            }
        });
        let out = out.to_string();
        for r in recent {
            st.recent_expanded.push(r);
            if st.recent_expanded.len() > 20 {
                st.recent_expanded.remove(0);
            }
        }
        out
    }

    fn insert_chars(st: &mut State, chars: &[char]) {
        let at = st.cursor.min(st.input.len());
        st.input.splice(at..at, chars.iter().copied());
        st.cursor = at + chars.len();
    }

    /// Integrates pasted text; multi-line or long pastes collapse into atomic placeholders.
    fn handle_paste(&self, st: &mut State, raw: &str, allow_coalesce: bool) {
        if let Some(p) = st.picker.as_mut() {
            let first = re!(r"\r?\n").split(raw).next().unwrap_or("");
            let next = format!("{}{}", p.filter, terminal_text(first));
            p.filter = crate::jsutil::take_chars(&next, 120).to_string();
            p.index = 0;
            self.schedule(st, 0);
            return;
        }
        let normalized = terminal_text(&raw.replace("\r\n", "\n").replace('\r', "\n"));
        let cleaned = if normalized.ends_with('\n') && !normalized[..normalized.len() - 1].contains('\n') { normalized[..normalized.len() - 1].to_string() } else { normalized };
        if cleaned.is_empty() {
            return;
        }
        st.input_restored_from_revert = false;
        st.scroll = 0;
        // 1. A single existing local image path.
        let unquoted = unquote(cleaned.trim()).to_string();
        if !unquoted.contains('\n') && image_path_like(&unquoted) {
            if st.attached_images.is_empty() && st.queued_prompts.is_empty() {
                st.next_image_idx = 1;
            }
            let placeholder = format!("[Image #{}]", st.next_image_idx);
            st.next_image_idx += 1;
            st.attached_images.insert(placeholder.clone(), unquoted.clone());
            Self::record_image_archive(st, &placeholder, &unquoted);
            Self::insert_chars(st, &chars_of(&format!("{placeholder} ")));
            self.schedule(st, 0);
            return;
        }
        // 2. Coalesce consecutive non-bracketed chunks arriving within 25ms.
        if allow_coalesce {
            if let Some(last) = st.last_paste_token.clone().filter(|t| now() - st.last_paste_at <= 25 && st.pasted_texts.contains_key(t)) {
                let merged = format!("{}{cleaned}", st.pasted_texts[&last]);
                let idx = re!(r"^\[Pasted text #(\d+)").captures(&last).and_then(|c| c[1].parse::<usize>().ok()).unwrap_or(1);
                let lines = merged.split('\n').count();
                let token = if lines >= 2 { format!("[Pasted text #{idx} +{lines} lines]") } else { format!("[Pasted text #{idx} ({} chars)]", merged.chars().count()) };
                let current: String = st.input.iter().collect();
                if current.contains(&last) {
                    st.pasted_texts.shift_remove(&last);
                    st.paste_archive.shift_remove(&last);
                    st.pasted_texts.insert(token.clone(), merged.clone());
                    Self::record_paste_archive(st, &token, &merged);
                    let next = current.replacen(&last, &token, 1);
                    let delta = token.chars().count() as i64 - last.chars().count() as i64;
                    st.input = chars_of(&next);
                    st.cursor = ((st.cursor as i64 + delta).max(0) as usize).min(st.input.len());
                    st.last_paste_token = Some(token);
                    st.last_paste_at = now();
                    self.schedule(st, 0);
                    return;
                }
            }
        }
        // 3. Multi-line or long single-line pastes become an atomic placeholder.
        let line_count = cleaned.split('\n').count();
        let char_count = cleaned.chars().count();
        let transient = st.pending.is_some() && !st.remember;
        if line_count >= 2 || (char_count > LARGE_PASTE_CHAR_THRESHOLD && (!transient || char_count > 500)) {
            if st.pasted_texts.is_empty() && st.queued_prompts.is_empty() {
                st.next_paste_idx = 1;
            }
            let idx = st.next_paste_idx;
            st.next_paste_idx += 1;
            let placeholder = if line_count >= 2 { format!("[Pasted text #{idx} +{line_count} lines]") } else { format!("[Pasted text #{idx} ({char_count} chars)]") };
            st.pasted_texts.insert(placeholder.clone(), cleaned.clone());
            Self::record_paste_archive(st, &placeholder, &cleaned);
            let suffix = if st.input.get(st.cursor) == Some(&' ') { "" } else { " " };
            Self::insert_chars(st, &chars_of(&format!("{placeholder}{suffix}")));
            st.last_paste_token = Some(placeholder);
            st.last_paste_at = now();
            self.schedule(st, 0);
            return;
        }
        // 4. Short single-line paste: inline, then look for embedded image paths.
        Self::insert_chars(st, &chars_of(&cleaned));
        self.collapse_pasted_image_paths(st);
        self.schedule(st, 0);
    }

    /// Collapses local image paths inside the composer into `[Image #N]` tokens.
    fn collapse_pasted_image_paths(&self, st: &mut State) -> bool {
        let current: String = st.input.iter().collect();
        if current.is_empty() || !re!(r"(?i)\.(?:png|jpe?g|webp|gif)").is_match(&current) {
            return false;
        }
        let unquoted = unquote(current.trim()).to_string();
        if image_path_like(&unquoted) {
            if st.attached_images.is_empty() && st.queued_prompts.is_empty() {
                st.next_image_idx = 1;
            }
            let placeholder = format!("[Image #{}]", st.next_image_idx);
            st.next_image_idx += 1;
            st.attached_images.insert(placeholder.clone(), unquoted.clone());
            Self::record_image_archive(st, &placeholder, &unquoted);
            st.input = chars_of(&format!("{placeholder} "));
            st.cursor = st.input.len();
            return true;
        }
        let pattern = re!(
            r#"(?i)(?:"((?:[A-Za-z]:[\\/]|\\\\|/)[^"\r\n]+?\.(?:png|jpe?g|webp|gif))"|'((?:[A-Za-z]:[\\/]|\\\\|/)[^'\r\n]+?\.(?:png|jpe?g|webp|gif))'|((?:[A-Za-z]:[\\/]|\\\\|/)[^\s"']+?\.(?:png|jpe?g|webp|gif)))"#
        );
        let mut changed = false;
        let mut found: Vec<String> = vec![];
        let replaced = pattern.replace_all(&current, |c: &regex::Captures| {
            let candidate = c.get(1).or(c.get(2)).or(c.get(3)).map(|m| m.as_str().trim().to_string()).unwrap_or_default();
            if candidate.is_empty() || !std::path::Path::new(&candidate).exists() {
                return c[0].to_string();
            }
            changed = true;
            found.push(candidate);
            format!("\u{0}IMG{}\u{0}", found.len() - 1)
        });
        if !changed {
            return false;
        }
        let mut out = replaced.to_string();
        for (i, candidate) in found.iter().enumerate() {
            if st.attached_images.is_empty() && st.queued_prompts.is_empty() {
                st.next_image_idx = 1;
            }
            let placeholder = format!("[Image #{}]", st.next_image_idx);
            st.next_image_idx += 1;
            st.attached_images.insert(placeholder.clone(), candidate.clone());
            Self::record_image_archive(st, &placeholder, candidate);
            out = out.replacen(&format!("\u{0}IMG{i}\u{0}"), &format!("{placeholder} "), 1);
        }
        st.input = chars_of(&out);
        st.cursor = st.input.len();
        true
    }

    fn schedule_paste_burst_check(&self, st: &mut State) {
        st.paste_gen += 1;
        let gen = st.paste_gen;
        let me = self.me.clone();
        self.handle.spawn(async move {
            tokio::time::sleep(Duration::from_millis(35)).await;
            let Some(t) = me.upgrade() else { return };
            let mut st = t.state.lock().unwrap();
            if st.paste_gen == gen && t.collapse_pasted_image_paths(&mut st) {
                t.schedule(&mut st, 0);
            }
        });
    }

    /// Windows: reads an image, file drop or text from the OS clipboard (Alt+V / raw Ctrl+V).
    async fn paste_from_system_clipboard(&self) {
        if !cfg!(windows) || self.state.lock().unwrap().closed {
            return;
        }
        let script = "[Console]::OutputEncoding = [System.Text.Encoding]::UTF8; Add-Type -AssemblyName System.Windows.Forms; Add-Type -AssemblyName System.Drawing; if ([System.Windows.Forms.Clipboard]::ContainsImage()) {   $img = [System.Windows.Forms.Clipboard]::GetImage();   $p = Join-Path ([System.IO.Path]::GetTempPath()) ('goon-clip-' + [guid]::NewGuid().ToString('N').Substring(0,8) + '.png');   $img.Save($p, [System.Drawing.Imaging.ImageFormat]::Png);   Write-Output ('IMG:' + $p); } elseif ([System.Windows.Forms.Clipboard]::ContainsFileDropList()) {   $files = [System.Windows.Forms.Clipboard]::GetFileDropList();   if ($files.Count -gt 0) { Write-Output ('FILE:' + $files[0]); } } elseif ([System.Windows.Forms.Clipboard]::ContainsText()) {   Write-Output ('TXT:' + [System.Windows.Forms.Clipboard]::GetText()); }";
        let cancel = CancellationToken::new();
        let cwd = std::env::current_dir().unwrap_or_default();
        let args: Vec<String> = ["-NoProfile", "-NonInteractive", "-Sta", "-Command", script].iter().map(|s| s.to_string()).collect();
        let options = crate::process::RunOptions { cwd: &cwd, cancel: &cancel, timeout_ms: 4000, env: None, output_limit_bytes: None, hard_output_bytes: None };
        let Ok(res) = crate::process::run_command("powershell.exe", &args, options).await else { return };
        if res.code != 0 || res.output.trim().is_empty() {
            return;
        }
        let out = re!(r"\r?\n$").replace(&res.output, "").to_string();
        let mut st = self.state.lock().unwrap();
        if st.closed {
            return;
        }
        if out.starts_with("IMG:") || out.starts_with("FILE:") {
            let path = re!(r"^(?:IMG|FILE):").replace(&out, "").trim().to_string();
            if re!(r"(?i)\.(?:png|jpe?g|webp|gif)$").is_match(&path) && std::path::Path::new(&path).exists() {
                if st.attached_images.is_empty() && st.queued_prompts.is_empty() {
                    st.next_image_idx = 1;
                }
                let placeholder = format!("[Image #{}]", st.next_image_idx);
                st.next_image_idx += 1;
                st.attached_images.insert(placeholder.clone(), path.clone());
                Self::record_image_archive(&mut st, &placeholder, &path);
                Self::insert_chars(&mut st, &chars_of(&format!("{placeholder} ")));
                self.schedule(&mut st, 0);
                return;
            }
        }
        if let Some(text) = out.strip_prefix("TXT:") {
            let text = text.to_string();
            self.handle_paste(&mut st, &text, false);
        }
    }

    /// Slash popup: commands matching the typed `/prefix` (exact first) and the selection.
    pub(crate) fn slash_popup(st: &mut State) -> (Vec<&'static str>, usize) {
        let typed: String = st.input.iter().collect();
        if st.label != "goon> " || !typed.starts_with('/') || typed.chars().any(char::is_whitespace) {
            st.popup_for.clear();
            st.popup_index = 0;
            return (vec![], 0);
        }
        let mut matches: Vec<&'static str> = SLASH_COMMANDS.iter().copied().filter(|c| c.starts_with(&typed)).collect();
        matches.sort_by_key(|c| *c != typed);
        if st.popup_for != typed {
            st.popup_for = typed;
            st.popup_index = 0;
        }
        let index = if matches.is_empty() { 0 } else { st.popup_index.min(matches.len() - 1) };
        (matches, index)
    }

    fn scroll_by(&self, st: &mut State, delta: i64) {
        if delta == 0 {
            return;
        }
        st.scroll = (st.scroll as i64 + delta).clamp(0, st.max_scroll as i64) as usize;
        self.schedule(st, 0);
    }

    // ------------------------------------------------------------------ input
    /// A batch of key events read together; non-bracketed paste bursts are detected here before
    /// they would turn into hundreds of single key presses (and premature Enters).
    pub fn raw_chunk(&self, raw: &str) -> bool {
        let mut st = self.state.lock().unwrap();
        if st.closed || raw.contains('\x1b') {
            return false;
        }
        let without = re!(r"\r$").replace(&re!(r"\r?\n$").replace(raw, ""), "").to_string();
        let multi = without.contains(['\r', '\n']);
        let large = without.chars().count() > LARGE_PASTE_CHAR_THRESHOLD;
        let burst = st.last_paste_token.is_some() && now() - st.last_paste_at <= 25 && without.chars().count() > 1;
        if multi || large || burst {
            self.handle_paste(&mut st, raw, true);
            return true;
        }
        false
    }

    /// Bracketed paste content.
    pub fn paste(&self, text: &str) {
        let mut st = self.state.lock().unwrap();
        self.handle_paste(&mut st, text, false);
    }

    pub fn resize(&self) {
        let mut st = self.state.lock().unwrap();
        st.last_frame_rows.clear();
        st.needs_full_clear = true;
        self.schedule(&mut st, 16);
    }

    pub fn ended(&self) {
        self.run_actions(vec![Action::Ended]);
    }

    fn arm_esc(&self, st: &mut State, prime: EscPrime) {
        st.esc_primed = Some(prime);
        st.esc_gen += 1;
        let gen = st.esc_gen;
        let me = self.me.clone();
        self.handle.spawn(async move {
            tokio::time::sleep(Duration::from_millis(2500)).await;
            let Some(t) = me.upgrade() else { return };
            let mut st = t.state.lock().unwrap();
            if st.esc_gen == gen {
                st.esc_primed = None;
                t.schedule(&mut st, 0);
            }
        });
        self.schedule(st, 0);
    }

    fn atomic_before(st: &State) -> Option<String> {
        let before: String = st.input[..st.cursor].iter().collect();
        re!(r"(\[(?:Image #\d+|Pasted text #\d+[^\]]*)\]\s?)$").captures(&before).map(|c| c[1].to_string())
    }

    fn atomic_after(st: &State) -> Option<String> {
        let after: String = st.input[st.cursor..].iter().collect();
        re!(r"^(\[(?:Image #\d+|Pasted text #\d+[^\]]*)\]\s?)").captures(&after).map(|c| c[1].to_string())
    }

    fn drop_token(st: &mut State, token: &str) {
        let t = token.trim();
        st.attached_images.shift_remove(t);
        st.pasted_texts.shift_remove(t);
        if st.attached_images.is_empty() && st.queued_prompts.is_empty() {
            st.next_image_idx = 1;
        }
        if st.pasted_texts.is_empty() && st.queued_prompts.is_empty() {
            st.next_paste_idx = 1;
        }
    }

    fn clear_composer(st: &mut State) {
        st.input.clear();
        st.cursor = 0;
        st.input_restored_from_revert = false;
        st.attached_images.clear();
        st.pasted_texts.clear();
    }

    pub fn key(&self, text: Option<&str>, key: Key) {
        let mut actions = vec![];
        {
            let mut st = self.state.lock().unwrap();
            self.keypress(&mut st, text, &key, &mut actions);
        }
        self.run_actions(actions);
    }

    fn keypress(&self, st: &mut State, text: Option<&str>, key: &Key, actions: &mut Vec<Action>) {
        if st.closed {
            return;
        }
        if st.picker.is_some() {
            self.picker_key(st, text, key);
            return;
        }
        if !key.is("escape") && st.esc_primed.is_some() {
            st.esc_primed = None;
            st.esc_gen += 1;
        }
        if st.status_notice.is_some() && !key.is("escape") {
            st.status_notice = None;
        }
        if key.ctrl && key.is("c") {
            actions.push(Action::Cancel);
            return;
        }
        if (key.ctrl && key.is("t")) || key.sequence == "\x14" {
            let next = !st.in_transcript_overlay;
            st.in_transcript_overlay = next;
            if next {
                self.flush_explore_group(st);
            }
            st.needs_full_clear = true;
            self.schedule(st, 0);
            actions.push(Action::ToggleTranscript(next));
            return;
        }
        if (key.ctrl && key.is("o")) || key.sequence == "\x0f" {
            actions.push(Action::Copy);
            return;
        }
        let plain = !key.ctrl && !key.meta;
        if st.in_transcript_overlay {
            if key.is("escape") || (plain && text == Some("q") && st.input.is_empty()) {
                st.in_transcript_overlay = false;
                st.needs_full_clear = true;
                self.schedule(st, 0);
                actions.push(Action::ToggleTranscript(false));
                return;
            }
            if key.is("up") || key.is("down") {
                self.scroll_by(st, if key.is("up") { 3 } else { -3 });
                return;
            }
            if key.is("home") || (plain && text == Some("g") && st.input.is_empty()) {
                st.scroll = st.max_scroll;
                self.schedule(st, 0);
                return;
            }
            if key.is("end") || (plain && text == Some("G") && st.input.is_empty()) {
                st.scroll = 0;
                self.schedule(st, 0);
                return;
            }
        }
        if key.is("escape") {
            if st.status == "Running" || st.status == "Compacting" {
                actions.push(Action::Cancel);
                return;
            }
            if st.scroll > 0 {
                st.scroll = 0;
                self.schedule(st, 0);
                return;
            }
            if st.pending.is_some() && st.remember {
                let backtrack = st.input.is_empty() || st.input_restored_from_revert;
                if backtrack {
                    if matches!(st.esc_primed, Some(EscPrime::Backtrack)) {
                        st.esc_primed = None;
                        st.esc_gen += 1;
                        let pending = st.pending.take().unwrap();
                        st.masked = false;
                        Self::clear_composer(st);
                        let _ = pending.tx.send(Ok("/revert".into()));
                        self.schedule(st, 0);
                        return;
                    }
                    self.arm_esc(st, EscPrime::Backtrack);
                    return;
                }
                if matches!(st.esc_primed, Some(EscPrime::Clear)) {
                    st.esc_primed = None;
                    st.esc_gen += 1;
                    Self::clear_composer(st);
                    st.next_image_idx = 1;
                    st.next_paste_idx = 1;
                    self.schedule(st, 0);
                    return;
                }
                self.arm_esc(st, EscPrime::Clear);
                return;
            } else if st.pending.is_some() && !st.remember {
                let pending = st.pending.take().unwrap();
                st.masked = false;
                st.input.clear();
                st.cursor = 0;
                st.attached_images.clear();
                st.pasted_texts.clear();
                let _ = pending.tx.send(if st.esc_cancels { Err(QErr::Esc) } else { Ok(String::new()) });
                self.schedule(st, 0);
                return;
            }
        }
        if key.ctrl && key.is("l") {
            st.transcript.clear();
            st.scroll = 0;
            st.max_scroll = 0;
            st.explore_group.clear();
            st.turn_snapshots.clear();
            st.input_restored_from_revert = false;
            st.esc_primed = None;
            st.status_notice = None;
            st.last_frame_rows.clear();
            st.last_body_total_lines = 0;
            st.needs_full_clear = true;
            self.schedule(st, 16);
            return;
        }
        if key.ctrl && key.is("d") && st.input.is_empty() {
            actions.push(Action::Ended);
            return;
        }
        if key.meta && key.is("up") && !st.queued_prompts.is_empty() {
            let popped = st.queued_prompts.pop().unwrap();
            st.input = chars_of(&popped);
            st.cursor = st.input.len();
            st.input_restored_from_revert = false;
            self.rehydrate_placeholders(st);
            self.schedule(st, 0);
            return;
        }
        let (_, rows) = self.size();
        if key.is("pageup") || (key.is("up") && (key.shift || key.ctrl)) {
            let step = if key.is("pageup") { (rows as i64 - 8).max(2) } else { 4 };
            self.scroll_by(st, step);
            return;
        }
        if key.is("pagedown") || (key.is("down") && (key.shift || key.ctrl)) {
            let step = if key.is("pagedown") { (rows as i64 - 8).max(2) } else { 4 };
            self.scroll_by(st, -step);
            return;
        }
        if key.is("up") || key.is("down") {
            let (matches, index) = Self::slash_popup(st);
            if !matches.is_empty() {
                let len = matches.len();
                st.popup_index = if key.is("up") { (index + len - 1) % len } else { (index + 1) % len };
                self.schedule(st, 0);
                return;
            }
            let dir = if key.is("up") { 2 } else { -2 };
            if st.pending.is_none() && st.input.is_empty() {
                self.scroll_by(st, dir);
                return;
            }
            if st.scroll > 0 {
                self.scroll_by(st, dir);
                return;
            }
            if key.is("up") && st.max_scroll > 0 && st.input.is_empty() && st.history_index == st.history.len() {
                self.scroll_by(st, 2);
                return;
            }
        }
        if (key.is("tab") && key.shift) || key.sequence == "\x1b[Z" {
            actions.push(Action::ToggleMode);
            self.schedule(st, 16);
            return;
        }
        if key.meta && (text == Some(".") || key.sequence == "\x1b.") {
            actions.push(Action::CycleEffort(1));
            self.schedule(st, 16);
            return;
        }
        if key.meta && (text == Some(",") || key.sequence == "\x1b,") {
            actions.push(Action::CycleEffort(-1));
            self.schedule(st, 16);
            return;
        }
        if (key.ctrl && key.is("v")) || (key.meta && key.is("v")) {
            actions.push(Action::ClipboardPaste);
            return;
        }
        if key.is("tab") {
            let current: String = st.input.iter().collect();
            if current.starts_with('/') {
                let (matches, index) = Self::slash_popup(st);
                let pick = matches.get(index).copied().or_else(|| SLASH_COMMANDS.iter().copied().find(|c| c.starts_with(current.trim())));
                if let Some(m) = pick {
                    st.input = chars_of(&format!("{m} "));
                    st.cursor = st.input.len();
                    st.input_restored_from_revert = false;
                    self.schedule(st, 16);
                }
                return;
            }
            if let Some(c) = re!(r"(?:^|\s)@([A-Za-z0-9_./\\-]*)$").captures(&current) {
                if !st.workspace_files.is_empty() {
                    let query = c[1].to_lowercase();
                    let found = st.workspace_files.iter().find(|f| f.to_lowercase().starts_with(&query)).or_else(|| st.workspace_files.iter().find(|f| f.to_lowercase().contains(&query))).cloned();
                    if let Some(found) = found {
                        let prefix_len = current.len() - c[1].len();
                        st.input = chars_of(&format!("{}{found} ", &current[..prefix_len]));
                        st.cursor = st.input.len();
                        st.input_restored_from_revert = false;
                        self.schedule(st, 16);
                    }
                }
            }
            return;
        }
        if key.is("return") || key.is("enter") {
            let (matches, index) = Self::slash_popup(st);
            if let Some(sel) = matches.get(index) {
                let joined: String = st.input.iter().collect();
                if *sel != joined {
                    st.input = chars_of(sel);
                    st.cursor = st.input.len();
                }
            }
            self.collapse_pasted_image_paths(st);
            let raw_joined: String = st.input.iter().collect();
            let value = self.expand_placeholders(st, &raw_joined);
            let Some(pending) = st.pending.take() else {
                let trimmed = value.trim().to_string();
                if !trimmed.is_empty() {
                    st.history.push(raw_joined.trim_end().to_string());
                    if st.history.len() > 100 {
                        st.history.remove(0);
                    }
                    Self::clear_composer(st);
                    // Slash commands and !cmd run immediately even while the agent works.
                    if (trimmed.starts_with('/') || trimmed.starts_with('!')) && self.callbacks.lock().unwrap().on_immediate_command.is_some() {
                        self.schedule(st, 0);
                        actions.push(Action::Immediate(trimmed));
                        return;
                    }
                    // Otherwise queue the prompt for the next turn (compact placeholders kept).
                    st.queued_prompts.push(raw_joined.trim_end().to_string());
                    self.schedule(st, 0);
                }
                return;
            };
            st.masked = false;
            if st.remember && !value.trim().is_empty() {
                st.history.push(raw_joined.trim_end().to_string());
                if st.history.len() > 100 {
                    st.history.remove(0);
                }
            }
            Self::clear_composer(st);
            st.scroll = 0;
            if st.queued_prompts.is_empty() {
                st.next_image_idx = 1;
                st.next_paste_idx = 1;
            }
            let _ = pending.tx.send(Ok(value));
            self.schedule(st, 16);
            return;
        }
        if key.is("left") {
            if st.cursor > 0 {
                st.cursor = match Self::atomic_before(st) {
                    Some(t) => st.cursor.saturating_sub(t.chars().count()),
                    None => st.cursor - 1,
                };
            }
        } else if key.is("right") {
            if st.cursor < st.input.len() {
                st.cursor = match Self::atomic_after(st) {
                    Some(t) => (st.cursor + t.chars().count()).min(st.input.len()),
                    None => st.cursor + 1,
                };
            }
        } else if key.is("home") || (key.ctrl && key.is("a")) {
            st.cursor = 0;
        } else if key.is("end") || (key.ctrl && key.is("e")) {
            if st.scroll > 0 && st.cursor == st.input.len() {
                st.scroll = 0;
            } else {
                st.cursor = st.input.len();
            }
        } else if key.is("backspace") {
            st.input_restored_from_revert = false;
            if st.cursor > 0 {
                match Self::atomic_before(st) {
                    Some(token) => {
                        Self::drop_token(st, &token);
                        let n = token.chars().count();
                        let start = st.cursor - n;
                        st.input.drain(start..st.cursor);
                        st.cursor = start;
                    }
                    None => {
                        st.cursor -= 1;
                        let at = st.cursor;
                        st.input.remove(at);
                    }
                }
            }
        } else if key.is("delete") {
            st.input_restored_from_revert = false;
            if st.cursor < st.input.len() {
                match Self::atomic_after(st) {
                    Some(token) => {
                        Self::drop_token(st, &token);
                        let n = token.chars().count();
                        let at = st.cursor;
                        st.input.drain(at..at + n);
                    }
                    None => {
                        let at = st.cursor;
                        st.input.remove(at);
                    }
                }
            }
        } else if key.ctrl && key.is("w") {
            st.input_restored_from_revert = false;
            match Self::atomic_before(st) {
                Some(token) => {
                    Self::drop_token(st, &token);
                    let n = token.chars().count();
                    let start = st.cursor - n;
                    st.input.drain(start..st.cursor);
                    st.cursor = start;
                }
                None => {
                    while st.cursor > 0 && st.input[st.cursor - 1] == ' ' {
                        st.cursor -= 1;
                        let at = st.cursor;
                        st.input.remove(at);
                    }
                    while st.cursor > 0 && st.input[st.cursor - 1] != ' ' {
                        st.cursor -= 1;
                        let at = st.cursor;
                        st.input.remove(at);
                    }
                }
            }
        } else if (key.is("up") || key.is("down") || (key.ctrl && (key.is("p") || key.is("n")))) && st.remember {
            st.input_restored_from_revert = false;
            let delta: i64 = if key.is("up") || key.is("p") { -1 } else { 1 };
            st.history_index = (st.history_index as i64 + delta).clamp(0, st.history.len() as i64) as usize;
            st.input = chars_of(st.history.get(st.history_index).map(String::as_str).unwrap_or(""));
            st.cursor = st.input.len();
            self.rehydrate_placeholders(st);
        } else if key.ctrl && key.is("u") {
            st.input_restored_from_revert = false;
            if !st.input.is_empty() {
                st.input.clear();
                st.cursor = 0;
                st.attached_images.clear();
                st.pasted_texts.clear();
                if st.queued_prompts.is_empty() {
                    st.next_image_idx = 1;
                    st.next_paste_idx = 1;
                }
            } else {
                self.scroll_by(st, 5);
            }
        } else if let Some(t) = text.filter(|t| !t.is_empty() && plain && st.input.len() < 16_000) {
            st.input_restored_from_revert = false;
            let normalized = t.replace("\r\n", "\n").replace('\r', "\n");
            let trimmed = if normalized.ends_with('\n') && !normalized[..normalized.len() - 1].contains('\n') { normalized[..normalized.len() - 1].to_string() } else { normalized.clone() };
            if trimmed.contains('\n') || trimmed.chars().count() > LARGE_PASTE_CHAR_THRESHOLD {
                self.handle_paste(st, t, false);
                return;
            }
            let chars = chars_of(&terminal_text(&trimmed).replace('\n', " "));
            Self::insert_chars(st, &chars);
            st.scroll = 0;
            let last = chars.last().copied().unwrap_or(' ');
            if chars.len() > 1 || "gfpe\"'GFPE".contains(last) {
                self.schedule_paste_burst_check(st);
            }
        }
        self.schedule(st, 16);
    }

    pub fn close(&self) {
        {
            let mut st = self.state.lock().unwrap();
            if st.closed {
                return;
            }
            st.closed = true;
            st.spin_gen += 1;
            st.spinning = false;
            if let Some(pending) = st.pending.take() {
                let _ = pending.tx.send(Err(QErr::Closed));
            }
            if let Some(mut p) = st.picker.take() {
                if let Some(tx) = p.tx.take() {
                    let _ = tx.send(None);
                }
            }
        }
        if self.real_terminal {
            let _ = crossterm::terminal::disable_raw_mode();
        }
        self.write_raw("\x1b[?2004l\x1b[?1007l\x1b[?1006l\x1b[?1000l\x1b[?25h\x1b[?1049l");
    }

    pub fn is_closed(&self) -> bool {
        self.state.lock().unwrap().closed
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        if self.real_terminal && !self.state.lock().map(|s| s.closed).unwrap_or(true) {
            let _ = crossterm::terminal::disable_raw_mode();
            self.write_raw("\x1b[?2004l\x1b[?1007l\x1b[?1006l\x1b[?1000l\x1b[?25h\x1b[?1049l");
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Real terminal input
// ---------------------------------------------------------------------------------------------

fn key_from_event(ev: &crossterm::event::KeyEvent) -> Option<(Option<String>, Key)> {
    use crossterm::event::{KeyCode, KeyModifiers};
    let ctrl = ev.modifiers.contains(KeyModifiers::CONTROL);
    let meta = ev.modifiers.contains(KeyModifiers::ALT);
    let shift = ev.modifiers.contains(KeyModifiers::SHIFT);
    let named = |n: &str| Key { name: n.into(), ctrl, meta, shift, sequence: String::new() };
    Some(match ev.code {
        KeyCode::Char(c) if ctrl && (c == 'j' || c == 'J') => (None, Key { name: "enter".into(), ..Default::default() }),
        KeyCode::Char(c) if ctrl || meta => {
            let lower = c.to_ascii_lowercase();
            let mut k = named(&lower.to_string());
            if meta && !ctrl {
                k.sequence = format!("\x1b{c}");
            }
            (if meta && !ctrl { Some(c.to_string()) } else { None }, k)
        }
        KeyCode::Char(c) => {
            let name = if c.is_ascii_alphabetic() { c.to_ascii_lowercase().to_string() } else { String::new() };
            (Some(c.to_string()), Key { name, ctrl: false, meta: false, shift: c.is_ascii_uppercase(), sequence: c.to_string() })
        }
        KeyCode::Enter => (Some("\r".into()), named("return")),
        KeyCode::Tab => (Some("\t".into()), named("tab")),
        KeyCode::BackTab => (None, Key { name: "tab".into(), shift: true, sequence: "\x1b[Z".into(), ..Default::default() }),
        KeyCode::Backspace => (None, named("backspace")),
        KeyCode::Delete => (None, named("delete")),
        KeyCode::Esc => (None, named("escape")),
        KeyCode::Left => (None, named("left")),
        KeyCode::Right => (None, named("right")),
        KeyCode::Up => (None, named("up")),
        KeyCode::Down => (None, named("down")),
        KeyCode::Home => (None, named("home")),
        KeyCode::End => (None, named("end")),
        KeyCode::PageUp => (None, named("pageup")),
        KeyCode::PageDown => (None, named("pagedown")),
        _ => return None,
    })
}

fn input_thread(tui: Weak<Tui>) {
    use crossterm::event::{poll, read, Event, KeyCode, KeyEventKind, KeyModifiers};
    loop {
        let Some(t) = tui.upgrade() else { return };
        if t.is_closed() {
            return;
        }
        drop(t);
        match poll(Duration::from_millis(100)) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(_) => {
                if let Some(t) = tui.upgrade() {
                    t.ended();
                }
                return;
            }
        }
        // Drain everything already available: one read() of a paste becomes one batch.
        let mut batch = vec![];
        while let Ok(true) = poll(Duration::ZERO) {
            match read() {
                Ok(ev) => batch.push(ev),
                Err(_) => break,
            }
            if batch.len() > 100_000 {
                break;
            }
        }
        let Some(t) = tui.upgrade() else { return };
        let keys: Vec<&crossterm::event::KeyEvent> = batch.iter().filter_map(|e| if let Event::Key(k) = e { Some(k) } else { None }).filter(|k| k.kind != KeyEventKind::Release).collect();
        let only_text = batch.len() > 1
            && keys.len() == batch.iter().filter(|e| !matches!(e, Event::Key(k) if k.kind == KeyEventKind::Release)).count()
            && keys.iter().all(|k| matches!(k.code, KeyCode::Char(_) | KeyCode::Enter | KeyCode::Tab) && !k.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT));
        if only_text {
            let raw: String = keys
                .iter()
                .map(|k| match k.code {
                    KeyCode::Char(c) => c.to_string(),
                    KeyCode::Enter => "\r".into(),
                    _ => "\t".into(),
                })
                .collect();
            if t.raw_chunk(&raw) {
                continue;
            }
        }
        for ev in batch {
            match ev {
                Event::Key(k) if k.kind != KeyEventKind::Release => {
                    if let Some((text, key)) = key_from_event(&k) {
                        t.key(text.as_deref(), key);
                    }
                }
                Event::Paste(text) => t.paste(&text),
                Event::Resize(_, _) => t.resize(),
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests;
