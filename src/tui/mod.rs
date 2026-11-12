//! Full-screen terminal UI: transcript with Markdown/diff/cell rendering, composer with history,
//! paste placeholders and slash popup, pickers, approvals, transcript overlay and row-diff
//! rendering with owned-screen selection; inline mode retains terminal-native selection.

mod completion;
mod composer;
mod editor;
#[cfg(unix)]
mod job_control;
mod native;
mod paste_burst;
mod pointer;
mod questions;
mod render;
mod selection;
mod table_copy;
mod table_layout;
mod table_view;
pub mod text;
#[cfg(windows)]
mod windows_console;

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

pub use text::{detect_env_theme_mode, format_inline_markdown, format_token_count, resolve_theme_mode, slice_visible, visible_length, wrap_terminal, Palette, Theme, ThemeMode};

static PANIC_TERMINAL_STATE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);
static PANIC_HOOK_INSTALLED: std::sync::Once = std::sync::Once::new();

const PANIC_STATE_RAW: u8 = 0b001;
const PANIC_STATE_ALT: u8 = 0b010;
const PANIC_STATE_PASTE: u8 = 0b100;

pub(crate) fn set_panic_terminal_state(active: bool, alt_screen: bool) {
    use std::sync::atomic::Ordering;
    let bits = if active { PANIC_STATE_RAW | PANIC_STATE_PASTE | if alt_screen { PANIC_STATE_ALT } else { 0 } } else { 0 };
    PANIC_TERMINAL_STATE.store(bits, Ordering::SeqCst);
}

pub(crate) fn mark_raw_mode_enabled(bracketed_paste: bool) {
    use std::sync::atomic::Ordering;
    let add = PANIC_STATE_RAW | if bracketed_paste { PANIC_STATE_PASTE } else { 0 };
    PANIC_TERMINAL_STATE.fetch_or(add, Ordering::SeqCst);
}

pub(crate) fn mark_raw_mode_disabled(bracketed_paste: bool) {
    use std::sync::atomic::Ordering;
    let clear = PANIC_STATE_RAW | if bracketed_paste { PANIC_STATE_PASTE } else { 0 };
    PANIC_TERMINAL_STATE.fetch_and(!clear, Ordering::SeqCst);
}

/// Lock-free terminal mode reset writer (testable on arbitrary buffers and safe inside a panic hook).
pub fn emergency_restore_terminal_to(out: &mut dyn Write, state_bits: u8) {
    if state_bits == 0 {
        return;
    }
    let _ = out.write_all(b"\x1b[?2026l\x1b[?1002l\x1b[?1004l\x1b[?2004l\x1b[?1007l\x1b[?1006l\x1b[?1000l\x1b[0m\x1b[?25h");
    if (state_bits & PANIC_STATE_ALT) != 0 {
        let _ = out.write_all(b"\x1b[?1049l");
    } else {
        let _ = out.write_all(b"\r\n");
    }
    let _ = out.flush();
}

/// Restores terminal modes and raw input without locking `Tui` state. Safe to invoke from panic hooks.
pub fn emergency_restore_terminal() {
    use std::io::IsTerminal;
    use std::sync::atomic::Ordering;
    let bits = PANIC_TERMINAL_STATE.swap(0, Ordering::SeqCst);
    if bits == 0 {
        return;
    }
    #[cfg(windows)]
    let _ = windows_console::restore_input_mode();
    let _ = crossterm::terminal::disable_raw_mode();
    if std::io::stdout().is_terminal() && !std::io::stderr().is_terminal() {
        let mut out = std::io::stdout();
        emergency_restore_terminal_to(&mut out, bits);
    } else {
        let mut err = std::io::stderr();
        emergency_restore_terminal_to(&mut err, bits);
    }
}

/// Installs an idempotent process-wide panic hook that restores terminal state before printing panic info.
pub fn install_panic_hook() {
    PANIC_HOOK_INSTALLED.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            emergency_restore_terminal();
            prev(info);
        }));
    });
}

pub const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
pub const USER_MARK: char = '\u{E000}';
const TRANSCRIPT_MAX_CHARS: usize = 160_000;
const TRANSCRIPT_KEEP_CHARS: usize = 120_000;
const LARGE_PASTE_CHAR_THRESHOLD: usize = 150;
const PASTE_COALESCE_WINDOW_MS: i64 = 250;
const CSI_ESCAPE_TIMEOUT_MS: u64 = 15;
const BRACKETED_PASTE_IDLE_TIMEOUT_MS: u64 = 250;
const CLIPBOARD_OUTPUT_LIMIT_BYTES: usize = 2 * 1024 * 1024;

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
    pub const WORKED: &str = "✓";
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
        "/auto" => "toggle full unrestricted auto-approve / yolo mode (or /mode auto)",
        "/yolo" => "enable full unrestricted auto-approve mode (tools, shell & network)",
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
    pub choices: Vec<(String, String)>,
    /// User questions may switch to free-form input; approvals never do.
    pub allow_custom: bool,
}

enum QErr {
    Cancelled,
    Esc,
    Closed,
}

struct Pending {
    id: u64,
    tx: oneshot::Sender<std::result::Result<String, QErr>>,
    choices: Vec<(String, String)>,
    selected: usize,
    allow_custom: bool,
    custom: bool,
    form: Option<questions::QuestionForm>,
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
    origin: Option<selection::Origin>,
}

pub(crate) struct BodyCache {
    pub cols: usize,
    pub rich: bool,
    pub theme_mode: text::ThemeMode,
    pub show_reasoning: bool,
    pub source: String,
    pub state: ParseState,
    pub rows: Vec<Row>,
    pub styled: Vec<String>,
    pub source_ends: Vec<usize>,
}

#[derive(Clone, Debug)]
pub(crate) struct ActiveTool {
    pub name: String,
    pub summary: String,
    pub started_at_ms: i64,
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
    vertical_goal_col: Option<usize>,
    pub scroll: usize,
    pub max_scroll: usize,
    pub label: String,
    pub status: String,
    pub mode_label: String,
    pub effort_label: String,
    pub todo_label: String,
    workspace_files: Vec<String>,
    workspace_skills: Vec<(String, String)>,
    mention_for: String,
    dismissed_mention: Option<String>,
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
    history_payloads: Vec<Option<composer::QueuedPrompt>>,
    history_index: usize,
    history_draft: Option<composer::Draft>,
    history_search: Option<composer::HistorySearch>,
    input_session: Option<String>,
    thread_inputs: IndexMap<String, composer::ThreadInput>,
    kill_buffer: String,
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
    last_inline_paste: Option<String>,
    last_paste_trailing_nl: bool,
    last_paste_ended_cr: bool,
    last_paste_at: i64,
    pub attached_images: IndexMap<String, String>,
    image_archive: IndexMap<String, String>,
    next_image_idx: usize,
    pub pasted_texts: IndexMap<String, String>,
    paste_archive: IndexMap<String, String>,
    recent_expanded: Vec<(String, String)>,
    next_paste_idx: usize,
    queued_prompts: Vec<composer::QueuedPrompt>,
    pub popup_index: usize,
    popup_for: String,
    dismissed_slash: Option<String>,
    esc_cancels: bool,
    pub esc_hint: String,
    turn_snapshots: Vec<String>,
    pub masked: bool,
    picker: Option<PickerState>,
    input_restored_from_revert: bool,
    esc_primed: Option<EscPrime>,
    esc_gen: u64,
    quit_armed: Option<(String, std::time::Instant)>,
    shortcuts_open: bool,
    paste_gen: u64,
    paste_burst: paste_burst::PasteBurst,
    paste_tick_running: bool,
    pub status_notice: Option<String>,
    notice_gen: u64,
    pub active_tools: IndexMap<String, ActiveTool>,
    pub theme_mode: text::ThemeMode,
    pub view_mode: String,
    pub in_transcript_overlay: bool,
    pub workspace_name: String,
    pub branch: String,
    pub context_window: u64,
    pub context_used: Option<u64>,
    pub goal: Option<SessionGoal>,
    pub title: String,
    pub body_cache: Option<BodyCache>,
    body_source_ends: Vec<usize>,
    body_layout: selection::Layout,
    native: Option<native::NativeViewport>,
    native_resize_cursor: Option<(usize, usize, usize)>,
    editor_requested: bool,
    editor_active: bool,
    suspend_requested: bool,
    input_yielded: bool,
    terminal_restored: bool,
    mouse_capture: bool,
    focused: bool,
    selection: Option<selection::Selection>,
    transcript_top_row: usize,
    transcript_screen_row: usize,
    transcript_view_rows: Vec<String>,
    selection_dragging: bool,
    selection_pointer_valid: bool,
    mouse_scroll_speed: f64,
    mouse_wheel: pointer::Wheel,
    selection_clicks: pointer::Clicks,
    follow_control: pointer::Follow,
}

type Callback0 = Arc<dyn Fn() + Send + Sync>;
type AsyncCallback0 = Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>;

#[derive(Default, Clone)]
struct Callbacks {
    on_toggle_mode: Option<Callback0>,
    on_cycle_effort: Option<Arc<dyn Fn(i32) + Send + Sync>>,
    on_toggle_transcript: Option<Arc<dyn Fn(bool) + Send + Sync>>,
    on_immediate_command: Option<Arc<dyn Fn(String) -> BoxFuture<'static, ()> + Send + Sync>>,
    on_steer: Option<Arc<dyn Fn(String) -> bool + Send + Sync>>,
    on_history: Option<Arc<dyn Fn(String) + Send + Sync>>,
    on_interrupt_idle: Option<Callback0>,
    on_copy: Option<AsyncCallback0>,
}

/// Side effects decided while the state lock is held and run after it is released.
enum Action {
    Cancel,
    InterruptIdle,
    ToggleMode,
    CycleEffort(i32),
    ToggleTranscript(bool),
    Immediate(String),
    Steer { expanded: String, queued: composer::QueuedPrompt },
    Copy,
    CopySelection(selection::CopyRequest),
    Ended,
    Quit,
    ClipboardPaste,
    RecordHistory(String),
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
        Self::start_with_alt_screen(title, cancel, true)
    }

    pub fn start_with_alt_screen(title: &str, cancel: Callback0, alternate: bool) -> Result<Arc<Tui>> {
        install_panic_hook();
        crossterm::terminal::enable_raw_mode()?;
        set_panic_terminal_state(true, alternate);
        #[cfg(windows)]
        let _ = windows_console::set_input_record_mode();
        let size: SizeFn = Box::new(|| crossterm::terminal::size().map(|(c, r)| (c as usize, r as usize)).unwrap_or((80, 24)));
        let tui = Tui::with_io_mode(title, cancel, Box::new(std::io::stdout()), size, true, alternate);
        let weak = Arc::downgrade(&tui);
        if let Err(err) = std::thread::Builder::new().name("goon-tui-input".into()).spawn(move || input_thread(weak)) {
            tui.close();
            return Err(err.into());
        }
        Ok(tui)
    }

    /// A UI over an arbitrary writer and size source (tests, pseudo-terminals).
    pub fn with_io(title: &str, cancel: Callback0, out: Box<dyn Write + Send>, size: SizeFn, real_terminal: bool) -> Arc<Tui> {
        Self::with_io_mode(title, cancel, out, size, real_terminal, true)
    }

    pub fn with_io_mode(title: &str, cancel: Callback0, out: Box<dyn Write + Send>, size: SizeFn, real_terminal: bool, alternate: bool) -> Arc<Tui> {
        let state = State {
            transcript: String::new(),
            input: vec![],
            cursor: 0,
            vertical_goal_col: None,
            scroll: 0,
            max_scroll: 0,
            label: "goon> ".into(),
            status: "Ready".into(),
            mode_label: "ask".into(),
            effort_label: "default".into(),
            todo_label: String::new(),
            workspace_files: vec![],
            workspace_skills: vec![],
            mention_for: String::new(),
            dismissed_mention: None,
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
            history_payloads: vec![],
            history_index: 0,
            history_draft: None,
            history_search: None,
            input_session: None,
            thread_inputs: IndexMap::new(),
            kill_buffer: String::new(),
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
            last_inline_paste: None,
            last_paste_trailing_nl: false,
            last_paste_ended_cr: false,
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
            dismissed_slash: None,
            esc_cancels: false,
            esc_hint: "skip".into(),
            turn_snapshots: vec![],
            masked: false,
            picker: None,
            input_restored_from_revert: false,
            esc_primed: None,
            esc_gen: 0,
            quit_armed: None,
            shortcuts_open: false,
            paste_gen: 0,
            paste_burst: paste_burst::PasteBurst::default(),
            paste_tick_running: false,
            status_notice: None,
            notice_gen: 0,
            active_tools: IndexMap::new(),
            theme_mode: if real_terminal { text::detect_env_theme_mode() } else { text::ThemeMode::Dark },
            view_mode: "verbose".into(),
            in_transcript_overlay: false,
            workspace_name: String::new(),
            branch: String::new(),
            context_window: 0,
            context_used: None,
            goal: None,
            title: title.to_string(),
            body_cache: None,
            body_source_ends: vec![],
            body_layout: selection::Layout::default(),
            native: (!alternate).then(native::NativeViewport::default),
            native_resize_cursor: None,
            editor_requested: false,
            editor_active: false,
            suspend_requested: false,
            input_yielded: false,
            terminal_restored: false,
            mouse_capture: false,
            focused: true,
            selection: None,
            transcript_top_row: 0,
            transcript_screen_row: 0,
            transcript_view_rows: vec![],
            selection_dragging: false,
            selection_pointer_valid: false,
            mouse_scroll_speed: 1.0,
            mouse_wheel: pointer::Wheel::default(),
            selection_clicks: pointer::Clicks::default(),
            follow_control: pointer::Follow::default(),
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
        // wheel, bracketed paste, hidden cursor. Decode focus events separately from input.
        if alternate {
            tui.write_raw("\x1b[?1049h");
        }
        tui.write_raw("\x1b[?1000l\x1b[?1006l\x1b[?1007h\x1b[?2004h\x1b[?25l");
        tui.write_raw("\x1b[?1004h");
        tui.render_now();
        tui
    }

    fn write_raw(&self, text: &str) {
        let mut out = self.out.lock().unwrap();
        let _ = out.write_all(text.as_bytes());
        let _ = out.flush();
    }

    pub fn set_mouse_scroll_speed(&self, speed: f64) {
        let mut st = self.state.lock().unwrap();
        st.mouse_scroll_speed = if speed.is_finite() && speed > 0.0 { speed } else { 1.0 };
        st.mouse_wheel = pointer::Wheel::default();
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
    pub fn set_on_steer(&self, f: Arc<dyn Fn(String) -> bool + Send + Sync>) {
        self.callbacks.lock().unwrap().on_steer = Some(f);
    }
    pub fn set_on_interrupt_idle(&self, f: Callback0) {
        self.callbacks.lock().unwrap().on_interrupt_idle = Some(f);
    }
    pub fn set_prompt_history(&self, entries: Vec<String>, record: Arc<dyn Fn(String) + Send + Sync>) {
        let mut st = self.state.lock().unwrap();
        st.history = entries;
        st.history_payloads = vec![None; st.history.len()];
        st.history_index = st.history.len();
        self.callbacks.lock().unwrap().on_history = Some(record);
    }
    pub fn set_on_copy(&self, f: AsyncCallback0) {
        self.callbacks.lock().unwrap().on_copy = Some(f);
    }

    fn run_actions(&self, actions: Vec<Action>) {
        let callbacks = self.callbacks.lock().unwrap().clone();
        for action in actions {
            match action {
                Action::Cancel => (self.cancel)(),
                Action::InterruptIdle => {
                    if let Some(interrupt) = &callbacks.on_interrupt_idle {
                        interrupt();
                    }
                }
                Action::RecordHistory(text) => {
                    if let Some(record) = &callbacks.on_history {
                        record(text);
                    }
                }
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
                Action::Steer { expanded, queued } => {
                    if !callbacks.on_steer.as_ref().map(|f| f(expanded)).unwrap_or(false) {
                        let mut st = self.state.lock().unwrap();
                        st.queued_prompts.push(queued);
                        self.schedule(&mut st, 0);
                    }
                }
                Action::CopySelection(request) => {
                    let me = self.me.clone();
                    self.handle.spawn(async move {
                        let status = crate::cli::copy_text_to_clipboard(&request.text).await;
                        if let Some(tui) = me.upgrade() {
                            tui.finish_selection_copy(&request, status);
                        }
                    });
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
                Action::Quit => {
                    if let Some(f) = &callbacks.on_immediate_command {
                        self.handle.spawn(f("/exit".into()));
                    } else {
                        (self.cancel)();
                        self.close();
                    }
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
        if st.closed || st.timer || st.input_yielded || st.suspend_requested {
            return;
        }
        st.timer = true;
        let me = self.me.clone();
        self.handle.spawn(async move {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            if let Some(t) = me.upgrade() {
                let mut actions = vec![];
                {
                    let mut st = t.state.lock().unwrap();
                    st.timer = false;
                    t.flush_paste_burst_if_due(&mut st, std::time::Instant::now(), cfg!(windows), &mut actions);
                    t.render(&mut st);
                }
                t.run_actions(actions);
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
        st.selection = None;
        st.scroll = 0;
        st.max_scroll = 0;
        if let Some(native) = st.native.as_mut() {
            native.reset();
        }
        st.explore_group.clear();
        st.active_tools.clear();
        st.turn_snapshots.clear();
        st.input_restored_from_revert = false;
        st.esc_primed = None;
        st.esc_gen += 1;
        st.status_notice = None;
        st.notice_gen = st.notice_gen.wrapping_add(1);
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
        st.active_tools.clear();
        st.output_channel = None;
        if steps > 0 && st.turn_snapshots.len() >= steps {
            let target = st.turn_snapshots.len() - steps;
            let restored = st.turn_snapshots[target].clone();
            st.turn_snapshots.truncate(target);
            st.transcript = restored;
            st.selection = None;
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
        st.vertical_goal_col = None;
        st.attached_images.clear();
        st.next_image_idx = 1;
        st.pasted_texts.clear();
        st.next_paste_idx = 1;
        st.paste_burst.clear_after_explicit_paste();
        Self::clear_paste_burst(&mut st);
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
        st.paste_burst.clear_after_explicit_paste();
        self.handle_paste(&mut st, text, false);
    }

    pub(crate) fn is_retry_notice(notice: &str) -> bool {
        notice.starts_with('↻') || notice.contains("retrying (")
    }

    pub(crate) fn set_notice_locked(&self, st: &mut State, notice: Option<String>) {
        st.status_notice = notice.filter(|n| !n.is_empty());
        st.notice_gen = st.notice_gen.wrapping_add(1);
        let busy = matches!(st.status.as_str(), "Running" | "Connecting MCP" | "Compacting");
        let is_retry = st.status_notice.as_deref().is_some_and(Self::is_retry_notice);
        if st.status_notice.is_some() && busy && !is_retry {
            let gen = st.notice_gen;
            let me = self.me.clone();
            self.handle.spawn(async move {
                tokio::time::sleep(Duration::from_secs(5)).await;
                let Some(t) = me.upgrade() else { return };
                let mut st = t.state.lock().unwrap();
                if st.notice_gen == gen && !st.closed {
                    st.status_notice = None;
                    t.schedule(&mut st, 0);
                }
            });
        }
    }

    #[cfg(test)]
    pub(crate) fn expire_busy_notice_for_test(&self) {
        let mut st = self.state.lock().unwrap();
        if st.status_notice.as_deref().is_some_and(|n| !Self::is_retry_notice(n)) {
            st.status_notice = None;
            st.notice_gen = st.notice_gen.wrapping_add(1);
            self.schedule(&mut st, 0);
        }
    }

    #[cfg(test)]
    pub(crate) fn backdate_active_tool_for_test(&self, id: &str, elapsed_ms: i64) {
        let mut st = self.state.lock().unwrap();
        if let Some(tool) = st.active_tools.get_mut(id) {
            tool.started_at_ms = now() - elapsed_ms;
        }
        self.schedule(&mut st, 0);
    }

    pub fn set_theme_mode(&self, mode: text::ThemeMode) {
        let mut st = self.state.lock().unwrap();
        if st.theme_mode != mode {
            st.theme_mode = mode;
            st.body_cache = None;
            st.last_frame_rows.clear();
            st.needs_full_clear = true;
            self.schedule(&mut st, 0);
        }
    }

    pub fn theme_mode(&self) -> text::ThemeMode {
        self.state.lock().unwrap().theme_mode
    }

    pub fn set_notice(&self, notice: Option<&str>) {
        let mut st = self.state.lock().unwrap();
        let clean = notice.map(terminal_text).filter(|n| !n.is_empty());
        self.set_notice_locked(&mut st, clean);
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
        st.selection = None;
        st.selection_dragging = false;
        st.view_mode = mode.into();
        st.mouse_wheel = pointer::Wheel::default();
        st.follow_control = pointer::Follow::default();
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
            if active != st.in_transcript_overlay {
                st.selection = None;
                st.selection_dragging = false;
                st.mouse_wheel = pointer::Wheel::default();
                st.follow_control = pointer::Follow::default();
            }
            st.in_transcript_overlay = active;
            if !active && st.native.is_some() {
                st.scroll = 0;
            }
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

    pub fn is_focused(&self) -> bool {
        self.state.lock().unwrap().focused
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
        let prompt = composer::QueuedPrompt::capture(&st, text);
        st.queued_prompts.push(prompt);
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
            if let Some(native) = st.native.as_mut() {
                native.trim_source_prefix(cut);
            }
            // A selection owns its immutable revision, including trimmed history.
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

    pub fn record_turn_duration(&self, elapsed_ms: i64) {
        let mut st = self.state.lock().unwrap();
        self.flush_explore_group(&mut st);
        let dur = text::format_turn_duration(elapsed_ms);
        self.append_cell(&mut st, &format!("\n{} Worked for {dur}\n", glyph::WORKED));
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
        let appended = {
            let mut st = self.state.lock().unwrap();
            self.flush_explore_group(&mut st);
            st.output_channel = None;
            let appended = format!("{}{}", Self::block_reset(&st), terminal_text(block));
            self.append_transcript(&mut st, &appended, true);
            appended
        };
        let _guard = composer::TransientGuard { tui: self.me.clone(), appended };
        self.question(label, cancel, false, opts).await
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
                } else if state == "done" || state == "error" || state == "interrupted" {
                    self.flush_explore_group(st);
                    let text = if state == "done" {
                        format!("\n{} Agent {} finished\n", glyph::AGENT, crate::jsutil::take_chars(&id, 8))
                    } else if state == "interrupted" {
                        format!("\n{} Agent {} interrupted\n", glyph::AGENT, crate::jsutil::take_chars(&id, 8))
                    } else {
                        format!("\n{} Agent {} failed\n", glyph::FAILED, crate::jsutil::take_chars(&id, 8))
                    };
                    self.append_cell(st, &text);
                }
            }
            "tool_stream" => {
                if st.status_notice.as_deref().is_some_and(Self::is_retry_notice) {
                    st.status_notice = None;
                    st.notice_gen = st.notice_gen.wrapping_add(1);
                }
                if !s("preview").is_empty() {
                    st.reasoning_header = Some(format!("Preparing {} · {}", s("name"), crate::jsutil::take_chars(&s("preview"), 90)));
                }
            }
            "text" | "reasoning" => {
                if st.status_notice.as_deref().is_some_and(Self::is_retry_notice) {
                    st.status_notice = None;
                    st.notice_gen = st.notice_gen.wrapping_add(1);
                }
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
                if st.status_notice.as_deref().is_some_and(Self::is_retry_notice) {
                    st.status_notice = None;
                    st.notice_gen = st.notice_gen.wrapping_add(1);
                }
                st.output_channel = None;
                let meta = event.get("meta").cloned();
                let raw_summary = meta.as_ref().and_then(|m| m.get("summary")).and_then(Value::as_str).unwrap_or("");
                let clean_summary = crate::jsutil::slice_chars(&terminal_text(raw_summary).split_whitespace().collect::<Vec<_>>().join(" "), 0, 72).to_string();
                let state = s("state");
                let id_raw = s("id");
                let name = s("name");
                let tool_key = if id_raw.is_empty() { name.clone() } else { id_raw.clone() };
                st.tool_states.insert(id_raw, format!("{name}: {state}"));
                if state == "start" {
                    st.active_tools.insert(tool_key, ActiveTool { name: terminal_text(&name), summary: clean_summary, started_at_ms: now() });
                    if st.reasoning_header.as_deref().is_some_and(|h| h.starts_with("Preparing ")) {
                        st.reasoning_header = None;
                    }
                } else {
                    st.active_tools.shift_remove(&tool_key);
                    if let Some(meta) = meta.filter(Value::is_object) {
                        self.record_completed_tool_cell(st, &name, &state, &meta);
                    }
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
            if state == "error" {
                let error = meta.get("error").and_then(Value::as_str).filter(|s| !s.is_empty());
                let detail = error.map(|s| format!("\n  └ {}", terminal_text(s))).unwrap_or_default();
                self.append_cell(st, &format!("\n{} Failed to edit {summary}{dur}{detail}\n", glyph::FAILED));
                return;
            }
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
            self.append_cell(st, &format!("\n{} {verb} {summary}{stats}{diff_block}\n", glyph::EDIT));
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
            st.active_tools.clear();
            if st.status_notice.as_deref().is_some_and(Self::is_retry_notice) {
                st.status_notice = None;
                st.notice_gen = st.notice_gen.wrapping_add(1);
            }
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
        self.question_inner(label, cancel, remember, opts, None).await
    }

    async fn question_inner(&self, label: &str, cancel: Option<&CancellationToken>, remember: bool, opts: QuestionOpts, form: Option<questions::QuestionForm>) -> Result<String> {
        let mut form = form;
        let (id, rx, saved) = loop {
            let installed = {
                let mut st = self.state.lock().unwrap();
                if st.editor_active || st.editor_requested || st.input_yielded || st.suspend_requested {
                    None
                } else {
                    if st.closed {
                        return Err(anyhow!("Terminal closed."));
                    }
                    if st.pending.is_some() {
                        return Err(anyhow!("A terminal question is already active."));
                    }
                    if cancel.map(|c| c.is_cancelled()).unwrap_or(false) {
                        return Err(anyhow!("This operation was aborted"));
                    }
                    if !remember {
                        // A held composer character belongs to the draft, never to a new approval
                        // or secret question. Resolve it before the modal snapshot and reset timing.
                        match st.paste_burst.flush_before_modified_input() {
                            paste_burst::FlushResult::Typed(ch) => Self::insert_chars(&mut st, &[ch]),
                            paste_burst::FlushResult::Paste(text) => self.handle_paste(&mut st, &text, true),
                            paste_burst::FlushResult::None => {}
                        }
                        st.quit_armed = None;
                        st.shortcuts_open = false;
                    }
                    let saved = if remember {
                        None
                    } else {
                        Some(composer::QuestionSnapshot {
                            draft: composer::Draft::capture(&st),
                            label: st.label.clone(),
                            remember: st.remember,
                            masked: st.masked,
                            esc_cancels: st.esc_cancels,
                            esc_hint: st.esc_hint.clone(),
                            history_index: st.history_index,
                            history_draft: st.history_draft.take(),
                            history_search: st.history_search.take(),
                            kill_buffer: st.kill_buffer.clone(),
                            image_archive: st.image_archive.clone(),
                            paste_archive: st.paste_archive.clone(),
                            recent_expanded: st.recent_expanded.clone(),
                        })
                    };
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
                        let value = next.expand();
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
                    st.pending = Some(Pending { id, tx, choices: opts.choices.clone(), selected: 0, allow_custom: opts.allow_custom, custom: false, form: form.take() });
                    self.schedule(&mut st, 16);
                    Some((id, rx, saved))
                }
            };
            if let Some(installed) = installed {
                break installed;
            }
            if self.is_closed() {
                return Err(anyhow!("Terminal closed."));
            }
            if let Some(cancel) = cancel {
                tokio::select! {
                    _ = cancel.cancelled() => return Err(anyhow!("This operation was aborted")),
                    _ = tokio::time::sleep(Duration::from_millis(25)) => {}
                }
            } else {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        };
        let _guard = composer::QuestionGuard { tui: self.me.clone(), id, saved };
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
            if trimmed.is_empty() || st.closed || st.editor_active || st.editor_requested || st.input_yielded || st.suspend_requested {
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
                let prompt = composer::QueuedPrompt::capture(&st, &trimmed);
                st.queued_prompts.push(prompt);
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

    fn clear_paste_burst(st: &mut State) {
        st.last_paste_token = None;
        st.last_inline_paste = None;
        st.last_paste_trailing_nl = false;
        st.last_paste_ended_cr = false;
    }

    /// Integrates pasted text; multi-line or long pastes collapse into atomic placeholders.
    fn handle_paste(&self, st: &mut State, raw: &str, allow_coalesce: bool) {
        if st.editor_active || st.editor_requested || st.input_yielded || st.suspend_requested {
            return;
        }
        // Pasting text must never approve a tool or leak into the restored composer.
        if st.pending.as_ref().map(|p| !p.choices.is_empty() && !p.custom).unwrap_or(false) {
            return;
        }
        if let Some(form) = st.pending.as_mut().and_then(|p| p.form.as_mut()) {
            form.questions[form.index].committed = false;
            form.questions[form.index].notes = true;
        }
        st.quit_armed = None;
        if st.masked {
            Self::insert_chars(st, &chars_of(&terminal_text(raw)));
            self.schedule(st, 0);
            return;
        }
        if let Some(search) = st.history_search.as_mut() {
            search.query.push_str(&terminal_text(raw));
            self.search_history(st, true, true);
            self.schedule(st, 0);
            return;
        }
        if let Some(p) = st.picker.as_mut() {
            let first = re!(r"\r?\n").split(raw).next().unwrap_or("");
            let next = format!("{}{}", p.filter, terminal_text(first));
            p.filter = crate::jsutil::take_chars(&next, 120).to_string();
            p.index = 0;
            self.schedule(st, 0);
            return;
        }
        let split_trailing_nl = |s: String| -> (String, bool) {
            match s.strip_suffix('\n') {
                Some(without_nl) if !without_nl.is_empty() => (without_nl.to_string(), true),
                _ => (s, false),
            }
        };
        let within_window = allow_coalesce && (st.last_paste_token.is_some() || st.last_inline_paste.is_some()) && now() - st.last_paste_at <= PASTE_COALESCE_WINDOW_MS;
        let ended_cr = raw.ends_with('\r');
        let effective_raw = if within_window && st.last_paste_ended_cr { raw.strip_prefix('\n').unwrap_or(raw) } else { raw };
        if within_window && effective_raw.is_empty() && !raw.is_empty() {
            st.last_paste_ended_cr = false;
            st.last_paste_at = now();
            return;
        }
        let normalized = terminal_text(&effective_raw.replace("\r\n", "\n").replace('\r', "\n"));
        // Coalesce consecutive non-bracketed chunks arriving within PASTE_COALESCE_WINDOW_MS.
        if within_window && !normalized.is_empty() {
            st.input_restored_from_revert = false;
            st.scroll = 0;
            if let Some(last) = st.last_paste_token.clone().filter(|t| st.pasted_texts.contains_key(t)) {
                let current: String = st.input.iter().collect();
                if current.contains(&last) {
                    let prev = &st.pasted_texts[&last];
                    let combined = if st.last_paste_trailing_nl { format!("{prev}\n{normalized}") } else { format!("{prev}{normalized}") };
                    let (merged, trailing_nl) = split_trailing_nl(combined);
                    let idx = re!(r"^\[Pasted text #(\d+)").captures(&last).and_then(|c| c[1].parse::<usize>().ok()).unwrap_or(1);
                    let lines = merged.split('\n').count();
                    let token = if lines >= 2 { format!("[Pasted text #{idx} +{lines} lines]") } else { format!("[Pasted text #{idx} ({} chars)]", merged.chars().count()) };
                    st.pasted_texts.shift_remove(&last);
                    st.paste_archive.shift_remove(&last);
                    st.pasted_texts.insert(token.clone(), merged.clone());
                    Self::record_paste_archive(st, &token, &merged);
                    let next = current.replacen(&last, &token, 1);
                    let delta = token.chars().count() as i64 - last.chars().count() as i64;
                    st.input = chars_of(&next);
                    st.cursor = ((st.cursor as i64 + delta).max(0) as usize).min(st.input.len());
                    st.last_paste_token = Some(token);
                    st.last_inline_paste = None;
                    st.last_paste_trailing_nl = trailing_nl;
                    st.last_paste_ended_cr = ended_cr;
                    st.last_paste_at = now();
                    self.schedule(st, 0);
                    return;
                }
            }
            if let Some(prev_inline) = st.last_inline_paste.clone() {
                let cursor = st.cursor.min(st.input.len());
                let before: String = st.input[..cursor].iter().collect();
                if before.ends_with(&prev_inline) {
                    let combined = if st.last_paste_trailing_nl { format!("{prev_inline}\n{normalized}") } else { format!("{prev_inline}{normalized}") };
                    let (merged, trailing_nl) = split_trailing_nl(combined);
                    let remove_chars = prev_inline.chars().count();
                    let start = cursor - remove_chars;
                    st.input.drain(start..cursor);
                    st.cursor = start;
                    let line_count = merged.split('\n').count();
                    let char_count = merged.chars().count();
                    let transient = st.pending.is_some() && !st.remember;
                    if line_count >= 2 || (char_count > LARGE_PASTE_CHAR_THRESHOLD && (!transient || char_count > 500)) {
                        if st.pasted_texts.is_empty() && st.queued_prompts.is_empty() {
                            st.next_paste_idx = 1;
                        }
                        let idx = st.next_paste_idx;
                        st.next_paste_idx += 1;
                        let placeholder = if line_count >= 2 { format!("[Pasted text #{idx} +{line_count} lines]") } else { format!("[Pasted text #{idx} ({char_count} chars)]") };
                        st.pasted_texts.insert(placeholder.clone(), merged.clone());
                        Self::record_paste_archive(st, &placeholder, &merged);
                        let suffix = if st.input.get(st.cursor) == Some(&' ') { "" } else { " " };
                        Self::insert_chars(st, &chars_of(&format!("{placeholder}{suffix}")));
                        st.last_paste_token = Some(placeholder);
                        st.last_inline_paste = None;
                    } else {
                        Self::insert_chars(st, &chars_of(&merged));
                        if self.collapse_pasted_image_paths(st) {
                            Self::clear_paste_burst(st);
                            self.schedule(st, 0);
                            return;
                        }
                        st.last_paste_token = None;
                        st.last_inline_paste = Some(merged);
                    }
                    st.last_paste_trailing_nl = trailing_nl;
                    st.last_paste_ended_cr = ended_cr;
                    st.last_paste_at = now();
                    self.schedule(st, 0);
                    return;
                }
            }
        }
        let (cleaned, trailing_nl) = split_trailing_nl(normalized);
        if cleaned.is_empty() {
            return;
        }
        st.input_restored_from_revert = false;
        st.status_notice = None;
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
            Self::clear_paste_burst(st);
            self.schedule(st, 0);
            return;
        }
        // 2. Multi-line or long single-line pastes become an atomic placeholder.
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
            if allow_coalesce {
                st.last_paste_token = Some(placeholder);
                st.last_inline_paste = None;
                st.last_paste_trailing_nl = trailing_nl;
                st.last_paste_ended_cr = ended_cr;
                st.last_paste_at = now();
            } else {
                Self::clear_paste_burst(st);
            }
            self.schedule(st, 0);
            return;
        }
        // 3. Short single-line paste: inline, then look for embedded image paths.
        Self::insert_chars(st, &chars_of(&cleaned));
        let collapsed_img = self.collapse_pasted_image_paths(st);
        if allow_coalesce && !collapsed_img {
            st.last_paste_token = None;
            st.last_inline_paste = Some(cleaned);
            st.last_paste_trailing_nl = trailing_nl;
            st.last_paste_ended_cr = ended_cr;
            st.last_paste_at = now();
        } else {
            Self::clear_paste_burst(st);
        }
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
        let options = crate::process::RunOptions {
            cwd: &cwd,
            cancel: &cancel,
            timeout_ms: 4000,
            env: None,
            output_limit_bytes: Some(CLIPBOARD_OUTPUT_LIMIT_BYTES),
            hard_output_bytes: Some(CLIPBOARD_OUTPUT_LIMIT_BYTES),
        };
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
            st.paste_burst.clear_after_explicit_paste();
            self.handle_paste(&mut st, &text, false);
        }
    }

    /// Slash popup: commands matching the typed `/prefix` (exact first) and the selection.
    pub(crate) fn slash_popup(st: &mut State) -> (Vec<&'static str>, usize) {
        let typed: String = st.input.iter().collect();
        if st.dismissed_slash.as_ref().map(|s| s != &typed).unwrap_or(false) {
            st.dismissed_slash = None;
        }
        if st.label != "goon> " || st.history_search.is_some() || st.dismissed_slash.as_ref() == Some(&typed) || !typed.starts_with('/') || typed.chars().any(char::is_whitespace) {
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
        let scroll = (st.scroll as i64).saturating_add(delta).clamp(0, st.max_scroll as i64) as usize;
        if scroll != st.scroll {
            if let Some(selection) = st.selection.as_mut() {
                selection.invalidate_copy();
            }
            st.scroll = scroll;
        }
        self.schedule(st, 0);
    }

    fn handle_mouse(&self, event: crossterm::event::MouseEvent) {
        use crossterm::event::{MouseButton, MouseEventKind};
        let mut st = self.state.lock().unwrap();
        if st.closed
            || st.editor_active
            || st.editor_requested
            || st.input_yielded
            || st.suspend_requested
            || (st.native.is_some() && !st.in_transcript_overlay)
            || st.picker.is_some()
            || (st.pending.is_some() && !st.remember)
        {
            return;
        }
        // Scroll, streaming output and popup layout may have changed since the
        // last paint. Hit-test the frame we are about to show, never stale rows.
        self.render(&mut st);
        if let Some(jump) = st.follow_control.handle(event) {
            if jump {
                st.selection = None;
                st.selection_dragging = false;
                st.scroll = 0;
                st.mouse_wheel = pointer::Wheel::default();
            }
            self.schedule(&mut st, 0);
            return;
        }
        match event.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                st.selection_clicks = pointer::Clicks::default();
                let row = event.row as usize;
                let inside = row >= st.transcript_screen_row && row < st.transcript_screen_row + st.transcript_view_rows.len() && (event.column as usize) < self.size().0;
                if inside || st.selection_dragging {
                    st.selection_pointer_valid = false;
                    let direction = if event.kind == MouseEventKind::ScrollUp { 1.0 } else { -1.0 };
                    let speed = st.mouse_scroll_speed;
                    let delta = st.mouse_wheel.rows(direction, speed);
                    self.scroll_by(&mut st, delta);
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let row = event.row as usize;
                if row >= st.transcript_screen_row && row < st.transcript_screen_row + st.transcript_view_rows.len() {
                    let local = row - st.transcript_screen_row;
                    let Some(point) = st.body_layout.hit(st.transcript_top_row + local, event.column as usize) else { return };
                    if event.modifiers.contains(crossterm::event::KeyModifiers::SHIFT) && st.selection.is_some() {
                        st.selection_clicks = pointer::Clicks::default();
                        if let Some(mut selection) = st.selection.take() {
                            selection.extend_pointer(point, &st.body_layout, st.transcript_top_row + local, event.column as usize);
                            st.selection = Some(selection);
                        }
                    } else {
                        let clicks = st.selection_clicks.press(event.column, event.row, std::time::Instant::now());
                        let source = st.selection.as_ref().map(|s| s.source.as_ref()).unwrap_or(&st.transcript);
                        let mut selected = selection::Selection::begin(point, source, &st.body_layout, clicks);
                        selected.begin_pointer(st.transcript_top_row + local, event.column as usize);
                        selected.extend_pointer(point, &st.body_layout, st.transcript_top_row + local, event.column as usize);
                        st.selection = Some(selected);
                    }
                    st.selection_dragging = true;
                    st.selection_pointer_valid = true;
                    self.schedule(&mut st, 0);
                } else if st.selection.take().is_some() {
                    st.selection_clicks = pointer::Clicks::default();
                    st.selection_dragging = false;
                    self.schedule(&mut st, 0);
                }
            }
            MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Up(MouseButton::Left) => {
                if !st.selection_dragging || st.transcript_view_rows.is_empty() {
                    return;
                }
                if matches!(event.kind, MouseEventKind::Drag(_)) {
                    st.selection_clicks = pointer::Clicks::default();
                    st.selection_pointer_valid = true;
                }
                let row = (event.row as usize).clamp(st.transcript_screen_row, st.transcript_screen_row + st.transcript_view_rows.len().saturating_sub(1));
                let local = row - st.transcript_screen_row;
                if st.selection_pointer_valid {
                    if let Some(point) = st.body_layout.hit(st.transcript_top_row + local, event.column as usize) {
                        if let Some(mut selection) = st.selection.take() {
                            selection.extend_pointer(point, &st.body_layout, st.transcript_top_row + local, event.column as usize);
                            st.selection = Some(selection);
                        }
                    }
                }
                if matches!(event.kind, MouseEventKind::Up(_)) {
                    st.selection_clicks.release(event.column, event.row);
                    st.selection_dragging = false;
                    if st.selection.as_ref().is_some_and(selection::Selection::is_empty) {
                        st.selection = None;
                    }
                }
                self.schedule(&mut st, 0);
            }
            _ => {}
        }
    }

    fn sync_mouse_mode(&self, st: &mut State) {
        let capture = (st.in_transcript_overlay || st.native.is_none()) && st.picker.is_none() && !(st.pending.is_some() && !st.remember);
        if capture == st.mouse_capture {
            return;
        }
        st.mouse_capture = capture;
        st.selection_clicks = pointer::Clicks::default();
        st.mouse_wheel = pointer::Wheel::default();
        st.follow_control = pointer::Follow::default();
        if capture {
            // SGR is essential on ConPTY; otherwise wheel records can masquerade as keys.
            self.write_raw("\x1b[?1007l\x1b[?1000h\x1b[?1006h\x1b[?1002h");
        } else {
            self.write_raw("\x1b[?1002l\x1b[?1000l\x1b[?1006l\x1b[?1007h");
        }
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
        let nl_terminated_paste = raw.ends_with('\n') && !without.is_empty();
        let fast_inline_chunk = !raw.contains(['\r', '\n']) && without.chars().count() >= 8;
        let has_active_burst = st.last_paste_token.is_some() || st.last_inline_paste.is_some();
        let burst = has_active_burst && now() - st.last_paste_at <= PASTE_COALESCE_WINDOW_MS && (raw.chars().count() >= 2 || raw == "\n");
        if multi || large || nl_terminated_paste || fast_inline_chunk || burst {
            self.handle_paste(&mut st, raw, true);
            return true;
        }
        false
    }

    /// Bracketed paste content.
    pub fn paste(&self, text: &str) {
        let mut actions = vec![];
        {
            let mut st = self.state.lock().unwrap();
            let flushed = st.paste_burst.flush_before_modified_input();
            self.apply_flush_result(&mut st, flushed, cfg!(windows), &mut actions);
            st.paste_burst.clear_after_explicit_paste();
            self.handle_paste(&mut st, text, false);
        }
        self.run_actions(actions);
    }

    pub fn resize(&self) {
        let mut st = self.state.lock().unwrap();
        // Resize events are dispatched on the sole input reader. On POSIX a DSR
        // query from the render task would contend with that reader's poll/read
        // lock for up to two seconds. Probe here instead, with painting excluded.
        let geometry_changed = self.size() != (st.last_cols, st.last_rows);
        if self.real_terminal && st.native.is_some() && geometry_changed {
            let (cols, rows) = self.size();
            st.native_resize_cursor = crossterm::cursor::position().ok().map(|(_, y)| (cols, rows, y as usize));
        }
        if geometry_changed {
            // Logical anchors survive reflow; only the old pointer gesture ends.
            if let Some(selection) = st.selection.as_mut() {
                selection.reset_navigation();
            }
            st.selection_clicks = pointer::Clicks::default();
            st.selection_dragging = false;
            st.mouse_wheel = pointer::Wheel::default();
            st.follow_control = pointer::Follow::default();
        }
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
        st.vertical_goal_col = None;
        st.input_restored_from_revert = false;
        st.attached_images.clear();
        st.pasted_texts.clear();
        st.paste_burst.clear_after_explicit_paste();
        Self::clear_paste_burst(st);
    }

    pub fn key(&self, text: Option<&str>, key: Key) {
        let mut actions = vec![];
        {
            let mut st = self.state.lock().unwrap();
            let flushed = st.paste_burst.flush_before_modified_input();
            self.apply_flush_result(&mut st, flushed, cfg!(windows), &mut actions);
            self.keypress(&mut st, text, &key, &mut actions);
        }
        self.run_actions(actions);
    }

    fn keypress(&self, st: &mut State, text: Option<&str>, key: &Key, actions: &mut Vec<Action>) {
        let before = st.pending.as_ref().and_then(|p| p.form.as_ref().map(|form| (p.id, form.index, st.input.clone())));
        self.keypress_inner(st, text, key, actions);
        if let Some((pending_id, index, input)) = before {
            if input != st.input {
                if let Some(pending) = st.pending.as_mut().filter(|p| p.id == pending_id) {
                    if let Some(form) = pending.form.as_mut().filter(|f| f.index == index) {
                        form.questions[index].committed = false;
                    }
                }
            }
        }
    }

    fn keypress_inner(&self, st: &mut State, text: Option<&str>, key: &Key, actions: &mut Vec<Action>) {
        st.selection_clicks = pointer::Clicks::default();
        if st.closed || st.editor_active || st.editor_requested || st.input_yielded || st.suspend_requested {
            return;
        }
        if key.ctrl
            && !key.meta
            && key.is("g")
            && st.picker.is_none()
            && !st.masked
            && !st.in_transcript_overlay
            && !st.shortcuts_open
            && st.history_search.is_none()
            && Self::mention_popup(st).is_none()
            && Self::slash_popup(st).0.is_empty()
            && (st.pending.is_none() || st.remember)
        {
            st.editor_requested = true;
            st.status_notice = Some("Opening external editor…".into());
            self.schedule(st, 0);
            return;
        }
        if !key.ctrl || !(key.is("c") || key.is("d")) {
            st.quit_armed = None;
        }
        if !(key.is("up") || key.is("down") || (key.ctrl && (key.is("p") || key.is("n")))) {
            st.vertical_goal_col = None;
        }
        Self::clear_paste_burst(st);
        if st.picker.is_some() {
            self.picker_key(st, text, key);
            return;
        }
        if st.pending.as_ref().and_then(|p| p.form.as_ref()).is_some() && self.form_key(st, text, key) {
            return;
        }
        if st.pending.as_ref().map(|p| !p.choices.is_empty() && !p.custom).unwrap_or(false) {
            let pending = st.pending.as_mut().unwrap();
            if pending.allow_custom && !key.ctrl && !key.meta && text == Some("o") {
                pending.custom = true;
            } else if key.is("up") || (key.ctrl && key.is("p")) {
                pending.selected = pending.selected.saturating_sub(1);
            } else if key.is("down") || (key.ctrl && key.is("n")) || key.is("tab") {
                pending.selected = (pending.selected + 1).min(pending.choices.len() - 1);
            } else if key.is("escape") || (key.ctrl && (key.is("c") || key.is("d"))) {
                let pending = st.pending.take().unwrap();
                let _ = pending.tx.send(if pending.allow_custom { Err(QErr::Esc) } else { Ok("n".into()) });
            } else if key.is("return") || key.is("enter") {
                let pending = st.pending.take().unwrap();
                let answer = pending.choices[pending.selected].0.clone();
                let _ = pending.tx.send(Ok(answer));
            } else if let Some(text) = text.filter(|_| !key.ctrl && !key.meta) {
                let selected = pending.choices.iter().position(|(value, _)| value.eq_ignore_ascii_case(text));
                if let Some(index) = selected {
                    let pending = st.pending.take().unwrap();
                    let _ = pending.tx.send(Ok(pending.choices[index].0.clone()));
                } else if let Ok(index) = text.parse::<usize>() {
                    if index > 0 && index <= pending.choices.len() {
                        let pending = st.pending.take().unwrap();
                        let _ = pending.tx.send(Ok(pending.choices[index - 1].0.clone()));
                    }
                }
            }
            self.schedule(st, 0);
            return;
        }
        // Search owns input until accepted/cancelled; neither Enter nor Ctrl+C can send/exit.
        if st.history_search.is_some() {
            if key.is("escape") || (key.ctrl && key.is("c")) {
                Self::cancel_history_search(st);
            } else if key.is("return") || key.is("enter") {
                if st.history_search.as_ref().unwrap().matched.is_some() {
                    st.history_search = None;
                    st.history_index = st.history.len();
                    st.history_draft = None;
                }
            } else if key.ctrl && (key.is("r") || key.is("s")) {
                self.search_history(st, key.is("r"), false);
            } else if key.is("backspace") {
                st.history_search.as_mut().unwrap().query.pop();
                self.search_history(st, true, true);
            } else if let Some(text) = text.filter(|_| !key.ctrl && !key.meta) {
                st.history_search.as_mut().unwrap().query.push_str(text);
                self.search_history(st, true, true);
            }
            self.schedule(st, 0);
            return;
        }
        if !key.is("escape") && st.esc_primed.is_some() {
            st.esc_primed = None;
            st.esc_gen += 1;
        }
        if st.status_notice.is_some() && !key.is("escape") {
            st.status_notice = None;
        }
        let copy_shortcut = key.ctrl && !key.meta && (key.is("c") || key.is("C") || key.is("o") || (key.is("insert") && !key.shift));
        let copy_and_follow = !key.ctrl
            && !key.meta
            && !key.shift
            && (key.is("return") || key.is("enter"))
            && (st.pending.is_none() || st.remember)
            && Self::mention_popup(st).is_none()
            && Self::slash_popup(st).0.is_empty();
        if (copy_shortcut || copy_and_follow) && (st.pending.is_none() || st.remember) {
            if let Some(request) = st.selection.as_ref().and_then(|s| s.copy_request(copy_and_follow)) {
                actions.push(Action::CopySelection(request));
                return;
            }
        }
        if key.ctrl && !key.meta && key.shift && (key.is("c") || key.is("C")) {
            return; // Copy-only chord without a selection must never fall through to interrupt.
        }
        if !key.ctrl
            && !key.meta
            && matches!(key.name.as_str(), "left" | "right" | "up" | "down")
            && st.selection.is_some()
            && (st.pending.is_none() || st.remember)
            && !st.shortcuts_open
            && Self::mention_popup(st).is_none()
            && Self::slash_popup(st).0.is_empty()
        {
            self.render(st); // Refresh wrapped row geometry before moving the logical endpoint.
            if let Some(mut selection) = st.selection.take() {
                if let Some(row) = selection.move_endpoint(&st.body_layout, &key.name) {
                    let height = st.transcript_view_rows.len().max(1);
                    let previous_scroll = st.scroll;
                    if row < st.transcript_top_row {
                        st.scroll = st.body_layout.row_count().saturating_sub(row + height);
                    } else if row >= st.transcript_top_row + height {
                        st.scroll = st.body_layout.row_count().saturating_sub(row + 1);
                    }
                    if previous_scroll != st.scroll {
                        selection.invalidate_copy();
                    }
                }
                st.selection = Some(selection);
            }
            st.selection_dragging = false;
            st.selection_pointer_valid = false;
            st.mouse_wheel = pointer::Wheel::default();
            self.schedule(st, 0);
            return;
        }
        if key.ctrl && key.is("c") {
            if st.pending.is_some() && !st.remember {
                let pending = st.pending.take().unwrap();
                let _ = pending.tx.send(if st.esc_cancels { Err(QErr::Esc) } else { Ok(String::new()) });
                self.schedule(st, 0);
                return;
            }
            if self.quit_shortcut(st, "c", actions) {
                return;
            }
            if !st.input.is_empty() && (st.pending.is_none() || st.remember) {
                Self::clear_composer(st);
                st.history_index = st.history.len();
                st.history_draft = None;
                self.schedule(st, 0);
                return;
            }
            if matches!(st.status.as_str(), "Running" | "Compacting" | "Connecting MCP" | "Stopping") {
                actions.push(Action::Cancel);
            } else {
                actions.push(Action::InterruptIdle);
            }
            return;
        }
        if (key.ctrl && key.is("t")) || key.sequence == "\x14" {
            let next = !st.in_transcript_overlay;
            st.selection = None;
            st.selection_dragging = false;
            st.in_transcript_overlay = next;
            st.mouse_wheel = pointer::Wheel::default();
            st.follow_control = pointer::Follow::default();
            if !next {
                st.scroll = 0;
            }
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
        if st.shortcuts_open {
            st.shortcuts_open = false;
            self.schedule(st, 0);
            if key.is("escape") || (plain && text == Some("?")) {
                return;
            }
        } else if plain && text == Some("?") && st.input.is_empty() && (st.pending.is_none() || st.remember) {
            st.shortcuts_open = true;
            self.schedule(st, 0);
            return;
        }
        if st.in_transcript_overlay {
            if key.is("escape") || (plain && text == Some("q") && st.input.is_empty()) {
                if st.selection.take().is_some() {
                    st.selection_dragging = false;
                    self.schedule(st, 0);
                    return;
                }
                st.in_transcript_overlay = false;
                st.scroll = 0;
                st.mouse_wheel = pointer::Wheel::default();
                st.follow_control = pointer::Follow::default();
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
                self.scroll_by(st, st.max_scroll as i64);
                self.schedule(st, 0);
                return;
            }
            if key.is("end") || (plain && text == Some("G") && st.input.is_empty()) {
                st.selection = None;
                st.selection_dragging = false;
                st.scroll = 0;
                st.mouse_wheel = pointer::Wheel::default();
                st.follow_control = pointer::Follow::default();
                self.schedule(st, 0);
                return;
            }
        }
        if key.ctrl && !key.meta && key.is("end") && (st.pending.is_none() || st.remember) && (st.native.is_none() || st.in_transcript_overlay) {
            st.selection = None;
            st.selection_dragging = false;
            st.scroll = 0;
            st.mouse_wheel = pointer::Wheel::default();
            st.follow_control = pointer::Follow::default();
            self.schedule(st, 0);
            return;
        }
        if key.is("escape") {
            if st.selection.take().is_some() {
                st.selection_dragging = false;
                self.schedule(st, 0);
                return;
            }
            if let Some(popup) = Self::mention_popup(st) {
                st.dismissed_mention = Some(popup.signature);
                self.schedule(st, 0);
                return;
            }
            if !Self::slash_popup(st).0.is_empty() {
                st.dismissed_slash = Some(st.input.iter().collect());
                self.schedule(st, 0);
                return;
            }
            if st.pending.is_some() && !st.remember {
                let pending = st.pending.take().unwrap();
                st.masked = false;
                Self::clear_composer(st);
                let _ = pending.tx.send(if st.esc_cancels { Err(QErr::Esc) } else { Ok(String::new()) });
                self.schedule(st, 0);
                return;
            }
            // Transcript navigation owns this Escape before turn cancellation.
            // Returning to live output must not also stop the work being viewed.
            if st.scroll > 0 {
                st.scroll = 0;
                st.mouse_wheel = pointer::Wheel::default();
                st.follow_control = pointer::Follow::default();
                self.schedule(st, 0);
                return;
            }
            if st.status == "Running" || st.status == "Compacting" || st.status == "Stopping" {
                actions.push(Action::Cancel);
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
            if let Some(native) = st.native.as_mut() {
                native.reset();
                self.write_raw("\x1b[3J");
            }
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
            if st.pending.is_some() && !st.remember {
                let pending = st.pending.take().unwrap();
                let _ = pending.tx.send(if st.esc_cancels { Err(QErr::Esc) } else { Ok(String::new()) });
                self.schedule(st, 0);
                return;
            }
            self.quit_shortcut(st, "d", actions);
            return;
        }
        if key.meta && key.is("up") && !st.queued_prompts.is_empty() && (st.pending.is_none() || st.remember) && Self::mention_popup(st).is_none() && Self::slash_popup(st).0.is_empty() {
            self.edit_latest_queued(st);
            return;
        }
        let (_, rows) = self.size();
        if st.native.is_some() && !st.in_transcript_overlay && (key.is("pageup") || key.is("pagedown") || ((key.is("up") || key.is("down")) && (key.shift || key.ctrl))) {
            // Native terminal scrollback owns paging; do not move a second transcript offset.
            return;
        }
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
            if let Some(popup) = Self::mention_popup(st) {
                st.popup_index = if key.is("up") { (popup.selected + popup.items.len() - 1) % popup.items.len() } else { (popup.selected + 1) % popup.items.len() };
                self.schedule(st, 0);
                return;
            }
            let (matches, index) = Self::slash_popup(st);
            if !matches.is_empty() {
                let len = matches.len();
                st.popup_index = if key.is("up") { (index + len - 1) % len } else { (index + 1) % len };
                self.schedule(st, 0);
                return;
            }
        }
        if key.ctrl && key.is("r") && (st.pending.is_none() || st.remember) {
            self.begin_history_search(st);
            self.schedule(st, 0);
            return;
        }
        if ((key.is("return") || key.is("enter")) && (key.shift || key.meta || key.ctrl)) || (key.ctrl && (key.is("j") || key.is("m"))) {
            Self::insert_newline(st);
            self.schedule(st, 0);
            return;
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
            if let Some(popup) = Self::mention_popup(st) {
                self.accept_mention(st, popup);
                return;
            }
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
            if !current.trim().is_empty() && (st.pending.is_none() || st.remember) {
                if st.pending.is_none() {
                    Self::remember_prompt(st, &current);
                    actions.push(Action::RecordHistory(self.expand_placeholders(st, &current)));
                    let prompt = composer::QueuedPrompt::capture(st, current.trim_end());
                    st.queued_prompts.push(prompt);
                    Self::clear_composer(st);
                    self.schedule(st, 0);
                } else {
                    self.keypress(st, None, &Key::named("return"), actions);
                }
            }
            return;
        }
        if key.is("return") || key.is("enter") {
            if let Some(popup) = Self::mention_popup(st) {
                self.accept_mention(st, popup);
                return;
            }
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
                    Self::remember_prompt(st, &raw_joined);
                    actions.push(Action::RecordHistory(value.clone()));
                    let queued = composer::QueuedPrompt::capture(st, raw_joined.trim_end());
                    Self::clear_composer(st);
                    // Slash commands and !cmd run immediately even while the agent works.
                    if (trimmed.starts_with('/') || trimmed.starts_with('!')) && self.callbacks.lock().unwrap().on_immediate_command.is_some() {
                        self.schedule(st, 0);
                        actions.push(Action::Immediate(trimmed));
                        return;
                    }
                    // Enter steers the current run; Tab explicitly queues for a later turn.
                    actions.push(Action::Steer { expanded: trimmed, queued });
                    self.schedule(st, 0);
                }
                return;
            };
            st.masked = false;
            if st.remember && !value.trim().is_empty() {
                Self::remember_prompt(st, &raw_joined);
                actions.push(Action::RecordHistory(value.clone()));
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
        if ((key.is("left") || key.is("right")) && (key.ctrl || key.meta)) || (key.meta && (key.is("b") || key.is("f"))) {
            let forward = key.is("right") || key.is("f");
            let target = if forward { composer::word_right(st) } else { composer::word_left(st) };
            st.cursor = composer::snap_atomic(st, target, forward);
        } else if key.is("left") || (key.ctrl && key.is("b")) {
            if st.cursor > 0 {
                st.cursor = match Self::atomic_before(st) {
                    Some(t) => st.cursor.saturating_sub(t.chars().count()),
                    None => composer::previous_grapheme(&st.input, st.cursor),
                };
            }
        } else if key.is("right") || (key.ctrl && key.is("f")) {
            if st.cursor < st.input.len() {
                st.cursor = match Self::atomic_after(st) {
                    Some(t) => (st.cursor + t.chars().count()).min(st.input.len()),
                    None => composer::next_grapheme(&st.input, st.cursor),
                };
            }
        } else if key.is("home") || (key.ctrl && key.is("a")) {
            st.cursor = composer::line_start(st);
        } else if key.is("end") || (key.ctrl && key.is("e")) {
            if st.scroll > 0 && st.cursor == st.input.len() {
                st.scroll = 0;
            } else {
                st.cursor = composer::line_end(st);
            }
        } else if (key.is("backspace") && (key.ctrl || key.meta)) || (key.ctrl && key.is("w")) {
            let start = composer::word_left(st);
            Self::kill_range(st, start, st.cursor);
        } else if (key.is("delete") && (key.ctrl || key.meta)) || (key.meta && key.is("d")) {
            let end = composer::word_right(st);
            Self::kill_range(st, st.cursor, end);
        } else if key.is("backspace") || (key.ctrl && key.is("h")) {
            st.input_restored_from_revert = false;
            st.paste_burst.note_backspace(std::time::Instant::now());
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
                        let start = composer::previous_grapheme(&st.input, st.cursor);
                        st.input.drain(start..st.cursor);
                        st.cursor = start;
                    }
                }
            }
        } else if key.is("delete") || (key.ctrl && key.is("d")) {
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
                        let end = composer::next_grapheme(&st.input, at);
                        st.input.drain(at..end);
                    }
                }
            }
        } else if key.is("up") || key.is("down") || (key.ctrl && (key.is("p") || key.is("n"))) {
            let older = key.is("up") || key.is("p");
            let label_cols = if st.label == "goon> " { 2 } else { visible_length(&st.label) };
            let cols = self.size().0.saturating_sub(1 + label_cols).max(1);
            if !Self::move_vertical(st, older, cols) && (st.pending.is_none() || st.remember) {
                self.navigate_history(st, older);
            }
        } else if key.ctrl && key.is("u") {
            let start = composer::line_start(st);
            Self::kill_range(st, if start == st.cursor { start.saturating_sub(1) } else { start }, st.cursor);
        } else if key.ctrl && key.is("k") {
            let end = composer::line_end(st);
            Self::kill_range(st, st.cursor, if end == st.cursor { (end + 1).min(st.input.len()) } else { end });
        } else if key.ctrl && key.is("y") {
            let killed = chars_of(&st.kill_buffer);
            Self::insert_chars(st, &killed);
            self.rehydrate_placeholders(st);
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
        self.schedule(st, 0);
    }

    fn insert_typed_chars_inline(&self, st: &mut State, raw: &str) {
        if st.esc_primed.is_some() {
            st.esc_primed = None;
            st.esc_gen += 1;
        }
        if st.status_notice.is_some() {
            st.status_notice = None;
        }
        Self::clear_paste_burst(st);
        let room = 16_000usize.saturating_sub(st.input.len());
        if room > 0 {
            let cleaned = terminal_text(raw).replace('\n', " ");
            let chars: Vec<char> = cleaned.chars().take(room).collect();
            if !chars.is_empty() {
                st.input_restored_from_revert = false;
                Self::insert_chars(st, &chars);
                st.scroll = 0;
                let last = chars.last().copied().unwrap_or(' ');
                if chars.len() > 1 || "gfpe\"'GFPE".contains(last) {
                    self.schedule_paste_burst_check(st);
                }
            }
        }
        self.schedule(st, 0);
    }

    fn apply_flush_result(&self, st: &mut State, res: paste_burst::FlushResult, is_windows: bool, actions: &mut Vec<Action>) -> bool {
        let was_typed_burst = st.paste_burst.take_typed_burst_flag();
        match res {
            paste_burst::FlushResult::None => false,
            paste_burst::FlushResult::Typed(ch) => {
                self.insert_typed_chars_inline(st, &ch.to_string());
                true
            }
            paste_burst::FlushResult::Paste(pasted) if was_typed_burst => {
                self.insert_typed_chars_inline(st, &pasted);
                true
            }
            paste_burst::FlushResult::Paste(pasted) => {
                if st.esc_primed.is_some() {
                    st.esc_primed = None;
                    st.esc_gen += 1;
                }
                if st.status_notice.is_some() {
                    st.status_notice = None;
                }
                if !is_windows && pasted.ends_with('\r') && !pasted.ends_with("\r\n") {
                    let line = &pasted[..pasted.len() - 1];
                    if !line.contains(['\r', '\n']) && line.chars().count() <= LARGE_PASTE_CHAR_THRESHOLD {
                        st.paste_burst.clear_after_explicit_paste();
                        Self::clear_paste_burst(st);
                        let cleaned = terminal_text(line);
                        if !cleaned.is_empty() {
                            st.input_restored_from_revert = false;
                            Self::insert_chars(st, &chars_of(&cleaned));
                        }
                        self.keypress(st, Some("\r"), &Key::named("return"), actions);
                        return true;
                    }
                }
                let allow_coalesce = pasted.contains(['\r', '\n']) || pasted.chars().count() >= 16;
                self.handle_paste(st, &pasted, allow_coalesce);
                true
            }
        }
    }

    fn flush_paste_burst_if_due(&self, st: &mut State, now: std::time::Instant, is_windows: bool, actions: &mut Vec<Action>) -> bool {
        let res = st.paste_burst.flush_if_due(now, is_windows);
        self.apply_flush_result(st, res, is_windows, actions)
    }

    #[cfg(test)]
    pub(crate) fn flush_paste_burst_at(&self, now: std::time::Instant, is_windows: bool) -> bool {
        let mut actions = vec![];
        let flushed = {
            let mut st = self.state.lock().unwrap();
            let did = self.flush_paste_burst_if_due(&mut st, now, is_windows, &mut actions);
            if did && !st.paste_burst.is_active() {
                self.schedule(&mut st, 0);
            }
            did
        };
        self.run_actions(actions);
        flushed
    }

    fn ensure_paste_tick(&self, st: &mut State, is_windows: bool) {
        if st.closed || st.paste_tick_running || !st.paste_burst.is_active() {
            return;
        }
        st.paste_tick_running = true;
        let me = self.me.clone();
        self.handle.spawn(async move {
            loop {
                let sleep_dur = {
                    let Some(t) = me.upgrade() else { return };
                    let mut st = t.state.lock().unwrap();
                    if st.closed {
                        st.paste_tick_running = false;
                        return;
                    }
                    match st.paste_burst.next_flush_delay(std::time::Instant::now(), is_windows) {
                        Some(d) => d,
                        None => {
                            st.paste_tick_running = false;
                            return;
                        }
                    }
                };
                tokio::time::sleep(sleep_dur).await;
                let Some(t) = me.upgrade() else { return };
                let mut actions = vec![];
                let keep_going = {
                    let mut st = t.state.lock().unwrap();
                    if st.closed {
                        st.paste_tick_running = false;
                        false
                    } else {
                        let did = t.flush_paste_burst_if_due(&mut st, std::time::Instant::now(), is_windows, &mut actions);
                        if did && !st.paste_burst.is_active() {
                            t.schedule(&mut st, 0);
                        }
                        if st.paste_burst.is_active() {
                            true
                        } else {
                            st.paste_tick_running = false;
                            false
                        }
                    }
                };
                t.run_actions(actions);
                if !keep_going {
                    return;
                }
            }
        });
    }

    pub(crate) fn handle_crossterm_key(&self, ev: crossterm::event::KeyEvent, now: std::time::Instant, is_windows: bool) {
        use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};
        if ev.kind == KeyEventKind::Release
            || matches!(
                ev.code,
                KeyCode::Modifier(_)
                    | KeyCode::CapsLock
                    | KeyCode::ScrollLock
                    | KeyCode::NumLock
                    | KeyCode::PrintScreen
                    | KeyCode::Pause
                    | KeyCode::Menu
                    | KeyCode::KeypadBegin
                    | KeyCode::Media(_)
                    | KeyCode::Null
            )
        {
            return;
        }
        let mut actions = vec![];
        {
            let mut st = self.state.lock().unwrap();
            self.flush_paste_burst_if_due(&mut st, now, is_windows, &mut actions);
            if st.closed {
                drop(st);
                self.run_actions(actions);
                return;
            }
            let choice_question = st
                .pending
                .as_ref()
                .map(|p| {
                    (!p.choices.is_empty() && !p.custom)
                        || p.form.as_ref().map(|form| form.confirm.is_some() || (!form.questions[form.index].notes && !form.questions[form.index].options.is_empty())).unwrap_or(false)
                })
                .unwrap_or(false);
            let shortcut_help = ev.code == KeyCode::Char('?')
                && st.input.is_empty()
                && !ev.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER | KeyModifiers::HYPER | KeyModifiers::META);
            if st.picker.is_some() || st.in_transcript_overlay || st.history_search.is_some() || choice_question || st.shortcuts_open || shortcut_help {
                let flushed = st.paste_burst.flush_before_modified_input();
                self.apply_flush_result(&mut st, flushed, is_windows, &mut actions);
                if let Some((text, key)) = key_from_event(&ev) {
                    self.keypress(&mut st, text.as_deref(), &key, &mut actions);
                }
            } else {
                let ctrl = ev.modifiers.contains(KeyModifiers::CONTROL);
                let alt = ev.modifiers.contains(KeyModifiers::ALT);
                let shift = ev.modifiers.contains(KeyModifiers::SHIFT);
                let super_meta = ev.modifiers.intersects(KeyModifiers::SUPER | KeyModifiers::HYPER | KeyModifiers::META);
                let is_altgr = is_windows && ctrl && alt;
                let has_non_text_modifier = ((ctrl || alt) && !is_altgr) || super_meta;

                if ev.code == KeyCode::Enter && !has_non_text_modifier && !shift {
                    if st.paste_burst.append_control_char_if_active('\r', now) {
                        self.ensure_paste_tick(&mut st, is_windows);
                    } else if st.paste_burst.newline_should_insert_instead_of_submit(now) {
                        st.paste_burst.extend_window(now);
                        self.handle_paste(&mut st, "\n", true);
                    } else {
                        self.keypress(&mut st, Some("\r"), &Key::named("return"), &mut actions);
                    }
                } else if (ev.code == KeyCode::Enter && (shift || alt) && !ctrl && !super_meta) || matches!(ev.code, KeyCode::Char(c) if ctrl && !alt && !super_meta && c.eq_ignore_ascii_case(&'j')) {
                    if st.paste_burst.append_control_char_if_active('\n', now) {
                        self.ensure_paste_tick(&mut st, is_windows);
                    } else if st.paste_burst.newline_should_insert_instead_of_submit(now) {
                        st.paste_burst.extend_window(now);
                        self.handle_paste(&mut st, "\n", true);
                    } else {
                        let flushed = st.paste_burst.flush_before_modified_input();
                        self.apply_flush_result(&mut st, flushed, is_windows, &mut actions);
                        self.keypress(&mut st, None, &Key { name: "return".into(), shift: true, ..Default::default() }, &mut actions);
                    }
                } else if matches!(ev.code, KeyCode::Char(c) if ctrl && !alt && !super_meta && c.eq_ignore_ascii_case(&'m')) {
                    if st.paste_burst.append_control_char_if_active('\r', now) {
                        self.ensure_paste_tick(&mut st, is_windows);
                    } else if st.paste_burst.newline_should_insert_instead_of_submit(now) {
                        st.paste_burst.extend_window(now);
                        self.handle_paste(&mut st, "\n", true);
                    } else {
                        self.keypress(&mut st, None, &Key { name: "m".into(), ctrl: true, ..Default::default() }, &mut actions);
                    }
                } else if (ev.code == KeyCode::Tab && !has_non_text_modifier && !shift) || matches!(ev.code, KeyCode::Char(c) if ctrl && !alt && !super_meta && c.eq_ignore_ascii_case(&'i')) {
                    if st.paste_burst.append_control_char_if_active('\t', now) {
                        self.ensure_paste_tick(&mut st, is_windows);
                    } else {
                        let flushed = st.paste_burst.flush_before_modified_input();
                        self.apply_flush_result(&mut st, flushed, is_windows, &mut actions);
                        self.keypress(&mut st, Some("\t"), &Key::named("tab"), &mut actions);
                    }
                } else if matches!(ev.code, KeyCode::Char(ch) if !has_non_text_modifier && !ch.is_control()) {
                    st.quit_armed = None;
                    let KeyCode::Char(ch) = ev.code else { unreachable!() };
                    match st.paste_burst.on_plain_char(ch, now) {
                        paste_burst::CharDecision::Passthrough => {
                            self.insert_typed_chars_inline(&mut st, &ch.to_string());
                        }
                        paste_burst::CharDecision::RetainFirstChar => {
                            self.ensure_paste_tick(&mut st, is_windows);
                        }
                        paste_burst::CharDecision::BeginBufferFromPending | paste_burst::CharDecision::BufferAppend => {
                            st.paste_burst.append_char_to_buffer(ch, now);
                            self.ensure_paste_tick(&mut st, is_windows);
                        }
                    }
                } else {
                    let flushed = st.paste_burst.flush_before_modified_input();
                    self.apply_flush_result(&mut st, flushed, is_windows, &mut actions);
                    if let Some((text, key)) = key_from_event(&ev) {
                        self.keypress(&mut st, text.as_deref(), &key, &mut actions);
                    }
                    if ev.code == KeyCode::Backspace || matches!(ev.code, KeyCode::Char('h' | 'H') if ctrl && !alt && !super_meta) {
                        st.paste_burst.note_backspace(now);
                    }
                }
            }
        }
        self.run_actions(actions);
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
            st.paste_burst.clear_after_explicit_paste();
            st.paste_tick_running = false;
            if let Some(pending) = st.pending.take() {
                let _ = pending.tx.send(Err(QErr::Closed));
            }
            if let Some(mut p) = st.picker.take() {
                if let Some(tx) = p.tx.take() {
                    let _ = tx.send(None);
                }
            }
            if st.editor_active || st.input_yielded {
                // The editor owns stdio until its child has been reaped. Its
                // handoff guard performs terminal restoration after cleanup.
                return;
            }
        }
        self.restore_terminal();
    }

    pub(super) fn restore_terminal(&self) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if st.terminal_restored {
            return;
        }
        st.terminal_restored = true;
        if self.real_terminal {
            set_panic_terminal_state(false, false);
            #[cfg(windows)]
            let _ = windows_console::restore_input_mode();
            let _ = crossterm::terminal::disable_raw_mode();
        }
        let sgr_reset = if text::Palette::for_mode(st.theme_mode).is_styled() { "\x1b[0m" } else { "" };
        self.write_raw(&format!("\x1b[?2026l\x1b[?1002l\x1b[?1004l\x1b[?2004l\x1b[?1007l\x1b[?1006l\x1b[?1000l{sgr_reset}\x1b[?25h"));
        if st.native.is_none() {
            self.write_raw("\x1b[?1049l");
        } else {
            let finish = st.native.as_mut().unwrap().finish();
            self.write_raw(&finish);
        }
    }

    pub fn is_closed(&self) -> bool {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).closed
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if self.real_terminal && !st.closed && !st.terminal_restored {
            st.terminal_restored = true;
            set_panic_terminal_state(false, false);
            #[cfg(windows)]
            let _ = windows_console::restore_input_mode();
            let _ = crossterm::terminal::disable_raw_mode();
            let sgr_reset = if text::Palette::for_mode(st.theme_mode).is_styled() { "\x1b[0m" } else { "" };
            self.write_raw(&format!("\x1b[?2026l\x1b[?1002l\x1b[?1004l\x1b[?2004l\x1b[?1007l\x1b[?1006l\x1b[?1000l{sgr_reset}\x1b[?25h"));
            if st.native.is_none() {
                self.write_raw("\x1b[?1049l");
            } else {
                let finish = st.native.as_mut().unwrap().finish();
                self.write_raw(&finish);
            }
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
        KeyCode::Char(c) if ctrl && !meta && (c == 'j' || c == 'J') => return None,
        KeyCode::Char(c) if ctrl && meta && !c.is_control() => {
            let name = if c.is_ascii_alphabetic() { c.to_ascii_lowercase().to_string() } else { String::new() };
            (Some(c.to_string()), Key { name, ctrl: false, meta: false, shift: c.is_ascii_uppercase(), sequence: c.to_string() })
        }
        KeyCode::Char(c) if ctrl || meta => {
            let lower = c.to_ascii_lowercase();
            let mut k = named(&lower.to_string());
            if ctrl && !meta && c == 'C' {
                k.shift = true; // Some terminals omit SHIFT on Ctrl+Shift+C.
            }
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
        KeyCode::Insert => (None, named("insert")),
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

fn csi_char(ev: &crossterm::event::KeyEvent) -> Option<char> {
    use crossterm::event::{KeyCode, KeyModifiers};
    let ctrl = ev.modifiers.contains(KeyModifiers::CONTROL);
    let alt = ev.modifiers.contains(KeyModifiers::ALT);
    match ev.code {
        KeyCode::Char(c) if (!ctrl && !alt) || (ctrl && alt && !c.is_control()) => Some(c),
        _ => None,
    }
}

fn csi_lead(ev: &crossterm::event::KeyEvent) -> Option<&'static str> {
    use crossterm::event::{KeyCode, KeyModifiers};
    let ctrl = ev.modifiers.contains(KeyModifiers::CONTROL);
    let alt = ev.modifiers.contains(KeyModifiers::ALT);
    match ev.code {
        KeyCode::Esc if !ctrl && !alt => Some("\x1b"),
        KeyCode::Char('[') if alt && !ctrl => Some("\x1b["),
        _ => None,
    }
}

fn paste_char_from_key(ev: &crossterm::event::KeyEvent, in_bracketed: bool) -> Option<char> {
    use crossterm::event::{KeyCode, KeyModifiers};
    let ctrl = ev.modifiers.contains(KeyModifiers::CONTROL);
    let alt = ev.modifiers.contains(KeyModifiers::ALT);
    let shift = ev.modifiers.contains(KeyModifiers::SHIFT);
    match ev.code {
        KeyCode::Char(c) if (!ctrl && !alt) || (ctrl && alt && !c.is_control()) => Some(c),
        KeyCode::Char(c) if ctrl && !alt && c.eq_ignore_ascii_case(&'j') => Some('\n'),
        KeyCode::Char(c) if ctrl && !alt && c.eq_ignore_ascii_case(&'m') => Some('\r'),
        KeyCode::Char(c) if ctrl && !alt && c.eq_ignore_ascii_case(&'i') => Some('\t'),
        KeyCode::Enter if !ctrl && !alt => Some('\r'),
        KeyCode::Tab if !ctrl && !alt && (!shift || in_bracketed) => Some('\t'),
        KeyCode::BackTab if !ctrl && !alt && in_bracketed => Some('\t'),
        _ => None,
    }
}

#[derive(Default, Clone)]
struct CsiBuffer {
    prefix: String,
    raw: Vec<crossterm::event::KeyEvent>,
}

enum CsiStep {
    None,
    Buffering,
    PasteStart,
    PasteEnd,
    ShiftEnter,
    Mismatch(Vec<crossterm::event::KeyEvent>),
}

impl CsiBuffer {
    fn is_empty(&self) -> bool {
        self.prefix.is_empty()
    }

    fn take(&mut self) -> Vec<crossterm::event::KeyEvent> {
        self.prefix.clear();
        std::mem::take(&mut self.raw)
    }

    fn step(&mut self, key: crossterm::event::KeyEvent, in_bracketed: bool) -> CsiStep {
        if self.prefix.is_empty() {
            if let Some(lead) = csi_lead(&key) {
                self.prefix.push_str(lead);
                self.raw.push(key);
                return CsiStep::Buffering;
            }
            return CsiStep::None;
        }
        let matched = match self.prefix.as_str() {
            "\x1b" if csi_char(&key) == Some('[') => Some('['),
            "\x1b[" => csi_char(&key).filter(|c| *c == '2' || (!in_bracketed && *c == '1')),
            "\x1b[2" if csi_char(&key) == Some('0') => Some('0'),
            "\x1b[20" => csi_char(&key).filter(|c| matches!(c, '0' | '1')),
            "\x1b[200" | "\x1b[201" if csi_char(&key) == Some('~') => {
                let is_start = self.prefix == "\x1b[200";
                self.prefix.clear();
                self.raw.clear();
                return if is_start { CsiStep::PasteStart } else { CsiStep::PasteEnd };
            }
            "\x1b[1" if !in_bracketed && csi_char(&key) == Some('3') => Some('3'),
            "\x1b[13" if !in_bracketed && csi_char(&key) == Some(';') => Some(';'),
            "\x1b[13;" if !in_bracketed && csi_char(&key) == Some('2') => Some('2'),
            "\x1b[13;2" if !in_bracketed && csi_char(&key) == Some('u') => {
                self.prefix.clear();
                self.raw.clear();
                return CsiStep::ShiftEnter;
            }
            _ => None,
        };
        if let Some(ch) = matched {
            self.prefix.push(ch);
            self.raw.push(key);
            CsiStep::Buffering
        } else {
            CsiStep::Mismatch(self.take())
        }
    }
}

pub(crate) struct BatchProbe {
    pub in_bracketed: bool,
    pub has_pending_csi: bool,
    pub ended_on_paste_end: bool,
}

#[derive(Default)]
pub(crate) struct InputDecoder {
    pending_csi: CsiBuffer,
    in_bracketed_paste: bool,
    bracketed_buf: String,
    end_csi: CsiBuffer,
}

impl InputDecoder {
    pub(crate) fn has_pending_csi(&self) -> bool {
        !self.in_bracketed_paste && !self.pending_csi.is_empty()
    }

    pub(crate) fn in_bracketed_paste(&self) -> bool {
        self.in_bracketed_paste
    }

    pub(crate) fn probe_batch(&self, events: &[crossterm::event::Event]) -> BatchProbe {
        use crossterm::event::{Event, KeyEventKind};
        let mut in_bracketed = self.in_bracketed_paste;
        let mut pending_csi = self.pending_csi.clone();
        let mut end_csi = self.end_csi.clone();
        let mut ended_on_paste_end = false;
        for ev in events {
            match ev {
                Event::Key(k) if k.kind == KeyEventKind::Release => continue,
                Event::Key(k) => {
                    ended_on_paste_end = false;
                    if in_bracketed {
                        match end_csi.step(*k, true) {
                            CsiStep::PasteEnd => {
                                in_bracketed = false;
                                ended_on_paste_end = true;
                            }
                            CsiStep::Mismatch(_) => {
                                let _ = end_csi.step(*k, true);
                            }
                            _ => {}
                        }
                    } else {
                        match pending_csi.step(*k, false) {
                            CsiStep::PasteStart => {
                                in_bracketed = true;
                                end_csi.take();
                            }
                            CsiStep::Mismatch(_) => {
                                let _ = pending_csi.step(*k, false);
                            }
                            _ => {}
                        }
                    }
                }
                Event::Paste(_) => {
                    ended_on_paste_end = !in_bracketed;
                    if !in_bracketed {
                        pending_csi.take();
                    }
                }
                _ => {}
            }
        }
        BatchProbe { in_bracketed, has_pending_csi: if in_bracketed { !end_csi.is_empty() } else { !pending_csi.is_empty() }, ended_on_paste_end }
    }

    pub(crate) fn feed_batch(&mut self, tui: &Tui, batch: Vec<crossterm::event::Event>) {
        self.feed_batch_at(tui, batch, std::time::Instant::now(), cfg!(windows));
    }

    pub(crate) fn feed_batch_at(&mut self, tui: &Tui, batch: Vec<crossterm::event::Event>, now: std::time::Instant, is_windows: bool) {
        use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
        let mut resized = false;
        for ev in batch {
            match ev {
                Event::Key(k) if k.kind == KeyEventKind::Release => {}
                Event::Key(k) if self.in_bracketed_paste => match self.end_csi.step(k, true) {
                    CsiStep::None => {
                        if let Some(ch) = paste_char_from_key(&k, true) {
                            self.bracketed_buf.push(ch);
                        }
                    }
                    CsiStep::Buffering | CsiStep::PasteStart | CsiStep::ShiftEnter => {}
                    CsiStep::PasteEnd => {
                        self.in_bracketed_paste = false;
                        let text = std::mem::take(&mut self.bracketed_buf);
                        tui.paste(&text);
                    }
                    CsiStep::Mismatch(flushed) => {
                        for fk in flushed {
                            if let Some(ch) = paste_char_from_key(&fk, true) {
                                self.bracketed_buf.push(ch);
                            }
                        }
                        if let CsiStep::None = self.end_csi.step(k, true) {
                            if let Some(ch) = paste_char_from_key(&k, true) {
                                self.bracketed_buf.push(ch);
                            }
                        }
                    }
                },
                #[cfg(unix)]
                Event::Key(k) if !is_windows && job_control::is_suspend(k) && self.pending_csi.is_empty() => {
                    tui.request_suspend();
                }
                Event::Key(k) => match self.pending_csi.step(k, false) {
                    CsiStep::None => tui.handle_crossterm_key(k, now, is_windows),
                    CsiStep::Buffering | CsiStep::PasteEnd => {}
                    CsiStep::ShiftEnter => tui.handle_crossterm_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT), now, is_windows),
                    CsiStep::PasteStart => {
                        self.in_bracketed_paste = true;
                        self.bracketed_buf.clear();
                        self.end_csi.take();
                    }
                    CsiStep::Mismatch(flushed) => {
                        for fk in flushed {
                            tui.handle_crossterm_key(fk, now, is_windows);
                        }
                        match self.pending_csi.step(k, false) {
                            CsiStep::None => tui.handle_crossterm_key(k, now, is_windows),
                            CsiStep::ShiftEnter => tui.handle_crossterm_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT), now, is_windows),
                            _ => {}
                        }
                    }
                },
                Event::Paste(text) => {
                    if self.in_bracketed_paste {
                        for fk in self.end_csi.take() {
                            if let Some(ch) = paste_char_from_key(&fk, true) {
                                self.bracketed_buf.push(ch);
                            }
                        }
                        self.bracketed_buf.push_str(&text);
                    } else {
                        for fk in self.pending_csi.take() {
                            tui.handle_crossterm_key(fk, now, is_windows);
                        }
                        tui.paste(&text);
                    }
                }
                Event::Resize(_, _) => resized = true,
                Event::Mouse(event) => tui.handle_mouse(event),
                Event::FocusGained => {
                    tui.state.lock().unwrap().focused = true;
                }
                Event::FocusLost => {
                    tui.state.lock().unwrap().focused = false;
                }
            }
        }
        if resized {
            tui.resize();
        }
    }

    pub(crate) fn flush_timeout(&mut self, tui: &Tui) {
        self.flush_timeout_at(tui, std::time::Instant::now(), cfg!(windows));
    }

    pub(crate) fn flush_timeout_at(&mut self, tui: &Tui, now: std::time::Instant, is_windows: bool) {
        if self.in_bracketed_paste {
            for fk in self.end_csi.take() {
                if let Some(ch) = paste_char_from_key(&fk, true) {
                    self.bracketed_buf.push(ch);
                }
            }
            self.in_bracketed_paste = false;
            let text = std::mem::take(&mut self.bracketed_buf);
            tui.paste(&text);
        } else if !self.pending_csi.is_empty() {
            for fk in self.pending_csi.take() {
                tui.handle_crossterm_key(fk, now, is_windows);
            }
        }
    }
}

fn drain_ready_events(batch: &mut Vec<crossterm::event::Event>) -> bool {
    use crossterm::event::{poll, read};
    match read() {
        Ok(ev) => batch.push(ev),
        Err(_) => return false,
    }
    while batch.len() < 100_000 {
        match poll(Duration::ZERO) {
            Ok(true) => match read() {
                Ok(ev) => batch.push(ev),
                Err(_) => return false,
            },
            _ => break,
        }
    }
    true
}

fn input_thread(tui: Weak<Tui>) {
    use crossterm::event::poll;
    use std::time::Instant;
    let mut decoder = InputDecoder::default();
    loop {
        let Some(t) = tui.upgrade() else { return };
        if t.is_closed() {
            return;
        }
        drop(t);
        #[cfg(windows)]
        let _ = windows_console::ensure_input_record_mode();
        let wait = if decoder.has_pending_csi() {
            Duration::from_millis(CSI_ESCAPE_TIMEOUT_MS)
        } else if decoder.in_bracketed_paste() {
            Duration::from_millis(BRACKETED_PASTE_IDLE_TIMEOUT_MS)
        } else {
            Duration::from_millis(100)
        };
        match poll(wait) {
            Ok(true) => {}
            Ok(false) => {
                if decoder.has_pending_csi() || decoder.in_bracketed_paste() {
                    if let Some(t) = tui.upgrade() {
                        decoder.flush_timeout(&t);
                    }
                }
                continue;
            }
            Err(_) => {
                if let Some(t) = tui.upgrade() {
                    t.ended();
                }
                return;
            }
        }
        let mut batch = vec![];
        if !drain_ready_events(&mut batch) {
            if let Some(t) = tui.upgrade() {
                t.ended();
            }
            return;
        }
        let mut last_event_at = Instant::now();
        let mut csi_timed_out = false;
        while batch.len() < 100_000 {
            let probe = decoder.probe_batch(&batch);
            if probe.ended_on_paste_end {
                break;
            }
            if probe.has_pending_csi {
                match poll(Duration::from_millis(CSI_ESCAPE_TIMEOUT_MS)) {
                    Ok(true) => {
                        if !drain_ready_events(&mut batch) {
                            break;
                        }
                        last_event_at = Instant::now();
                    }
                    Ok(false) => {
                        csi_timed_out = true;
                        break;
                    }
                    Err(_) => break,
                }
            } else if probe.in_bracketed {
                if last_event_at.elapsed() >= Duration::from_millis(BRACKETED_PASTE_IDLE_TIMEOUT_MS) {
                    csi_timed_out = true;
                    break;
                }
                let remain = Duration::from_millis(BRACKETED_PASTE_IDLE_TIMEOUT_MS).saturating_sub(last_event_at.elapsed());
                match poll(Duration::from_millis(50).min(remain)) {
                    Ok(true) => {
                        if !drain_ready_events(&mut batch) {
                            break;
                        }
                        last_event_at = Instant::now();
                    }
                    Ok(false) => {
                        if last_event_at.elapsed() >= Duration::from_millis(BRACKETED_PASTE_IDLE_TIMEOUT_MS) {
                            csi_timed_out = true;
                            break;
                        }
                    }
                    Err(_) => break,
                }
            } else {
                break;
            }
        }
        let Some(t) = tui.upgrade() else { return };
        decoder.feed_batch(&t, batch);
        if csi_timed_out {
            decoder.flush_timeout(&t);
        }
        if t.state.lock().unwrap().editor_requested {
            decoder = InputDecoder::default();
            t.run_external_editor();
        }
        #[cfg(unix)]
        if t.state.lock().unwrap().suspend_requested {
            decoder = InputDecoder::default();
            t.run_suspend();
        }
    }
}

#[cfg(test)]
mod tests;
