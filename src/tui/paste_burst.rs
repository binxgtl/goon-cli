//! Per-key paste-burst state machine adapted from OpenAI Codex (`bottom_pane/paste_burst.rs`).
//!
//! On Windows (ConPTY / `cmd.exe` / Windows Terminal), clipboard pastes frequently arrive as a
//! stream of `KeyCode::Char`, `KeyCode::Enter`, `Ctrl+J`, and `KeyCode::Tab` events rather than a
//! single `Event::Paste`. Classifying pastes per OS `read()` batch fails whenever the first key
//! wakes `poll()` alone or ConPTY splits chunks across pipe writes.
//!
//! `PasteBurst` operates on every individual key event:
//! - Briefly holds the first plain character (`RetainFirstChar`) for `PASTE_BURST_CHAR_INTERVAL`
//!   so single-character paste prefixes (e.g. `'N'` or `'Đ'`) never leak inline before the second
//!   character of the paste arrives.
//! - Promotes a held first character into `buffer` as soon as a second plain character or control
//!   character (`Enter`, `Ctrl+J`, `Tab`) arrives within `PASTE_BURST_CHAR_INTERVAL`.
//! - Accumulates all plain characters (including Shift and Windows AltGr) and newline/tab control
//!   characters into `buffer`, refreshing the idle timeout on every character with no wall-clock
//!   cap for multi-line or machine-speed paste bursts, while bounding continuous human typing or
//!   rollover streams via `SINGLE_LINE_BURST_WALL_CAP` (`120ms`).
//! - Emits a single `FlushResult::Paste(String)` once the stream has been completely idle for
//!   `PASTE_BURST_ACTIVE_IDLE_TIMEOUT` (`60ms` on Windows, `8ms` on Unix), while keeping an Enter
//!   suppression window (`120ms`) alive for paste-sized payloads so slightly-late trailing `Enter`
//!   events never auto-submit.

use std::time::Duration;
use std::time::Instant;

const PASTE_ENTER_SUPPRESS_WINDOW: Duration = Duration::from_millis(120);
const PASTE_BURST_CHAR_INTERVAL: Duration = Duration::from_millis(8);
const IME_REWRITE_WINDOW: Duration = Duration::from_millis(40);
const IME_REWRITE_MAX_CHARS: u16 = 16;
const SINGLE_LINE_BURST_WALL_CAP: Duration = Duration::from_millis(120);

#[cfg(not(windows))]
const PASTE_BURST_ACTIVE_IDLE_TIMEOUT: Duration = Duration::from_millis(8);
#[cfg(windows)]
const PASTE_BURST_ACTIVE_IDLE_TIMEOUT: Duration = Duration::from_millis(60);

#[derive(Clone, Default)]
pub(crate) struct PasteBurst {
    last_plain_char_time: Option<Instant>,
    consecutive_plain_char_burst: u16,
    max_fast_run: u16,
    burst_window_until: Option<Instant>,
    burst_started_at: Option<Instant>,
    after_backspace_at: Option<Instant>,
    ime_rewrite_active: bool,
    ime_rewrite_chars: u16,
    last_flush_was_typed_burst: bool,
    buffer: String,
    active: bool,
    pending_first_char: Option<(char, Instant)>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CharDecision {
    /// Insert `ch` directly into the composer (`0ms` echo) during a post-`Backspace` IME rewrite.
    Passthrough,
    /// Append `ch` to the already-active paste buffer.
    BufferAppend,
    /// Hold the first fast character briefly to see whether a burst follows.
    RetainFirstChar,
    /// Promote the held first character into `buffer`; caller appends the current `ch`.
    BeginBufferFromPending,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum FlushResult {
    Paste(String),
    Typed(char),
    None,
}

impl PasteBurst {
    pub(crate) fn char_interval() -> Duration {
        PASTE_BURST_CHAR_INTERVAL
    }

    pub(crate) fn active_idle_timeout(is_windows: bool) -> Duration {
        let _ = PASTE_BURST_ACTIVE_IDLE_TIMEOUT;
        if is_windows {
            Duration::from_millis(60)
        } else {
            Duration::from_millis(8)
        }
    }

    #[cfg(test)]
    pub(crate) fn recommended_char_flush_delay() -> Duration {
        Self::char_interval() + Duration::from_millis(1)
    }

    #[cfg(test)]
    pub(crate) fn recommended_active_flush_delay(is_windows: bool) -> Duration {
        Self::active_idle_timeout(is_windows) + Duration::from_millis(1)
    }

    /// Records a `Backspace` / `Ctrl+H` event so subsequent Vietnamese Telex/VNI IME syllable
    /// rewrites (`Backspace×N` + Unicode/ASCII replacement burst) echo immediately at `0ms`
    /// without entering the `60ms` Windows `PasteBurst` hold or `120ms` Enter suppression window.
    pub(crate) fn note_backspace(&mut self, now: Instant) {
        self.after_backspace_at = Some(now);
        self.ime_rewrite_active = false;
        self.ime_rewrite_chars = 0;
    }

    fn is_human_typed_burst(&self) -> bool {
        self.is_active_internal()
            && !self.buffer.contains(['\r', '\n', '\t'])
            && (self.max_fast_run <= 3 || (self.max_fast_run <= 10 && self.consecutive_plain_char_burst == 1 && self.buffer.chars().count() <= 24))
    }

    /// Returns `true` when a short human key-rollover (`max_fast_run <= 3`, `< 16` chars,
    /// no control chars) is followed `> 8ms` later by an interactive control key (`Enter`,
    /// `Shift+Enter`/`Ctrl+J`, `Ctrl+M`, or `Tab`), so the rolled-over suffix flushes inline
    /// first instead of swallowing the control key into the `60ms` Windows paste buffer.
    pub(crate) fn should_flush_human_rollover_before_control(&self, now: Instant) -> bool {
        self.active
            && !self.buffer.contains(['\r', '\n', '\t'])
            && self.max_fast_run <= 3
            && self.buffer.chars().count() < 16
            && self.last_plain_char_time.is_some_and(|t| now.saturating_duration_since(t) > PASTE_BURST_CHAR_INTERVAL)
    }

    pub(crate) fn next_flush_delay(&self, now: Instant, is_windows: bool) -> Option<Duration> {
        if !self.is_active() {
            return None;
        }
        let timeout = if self.is_active_internal() { Self::active_idle_timeout(is_windows) } else { Self::char_interval() };
        let Some(last) = self.last_plain_char_time else {
            return Some(Duration::from_millis(1));
        };
        let elapsed = now.saturating_duration_since(last);
        let idle_delay = timeout.saturating_sub(elapsed) + Duration::from_millis(1);
        if self.is_human_typed_burst() {
            if let Some(started) = self.burst_started_at {
                let wall_elapsed = now.saturating_duration_since(started);
                let wall_delay = SINGLE_LINE_BURST_WALL_CAP.saturating_sub(wall_elapsed).max(Duration::from_millis(1));
                return Some(idle_delay.min(wall_delay));
            }
        }
        Some(idle_delay)
    }

    /// Classifies a text-producing character (`plain`, `Shift`, or Windows `AltGr`).
    /// Applies uniformly to ASCII and Unicode so mixed prefixes like Vietnamese `"Nếu..."` or
    /// `"Đây..."` never leak their first character inline before buffering starts, while
    /// post-`Backspace` IME syllable rewrites (`"tiếng"`, `"đường"`, `"chào"`) pass through at `0ms`.
    pub(crate) fn on_plain_char(&mut self, ch: char, now: Instant) -> CharDecision {
        let char_interval = Self::char_interval();

        if !self.active && self.pending_first_char.is_none() && !matches!(ch, '\r' | '\n') {
            let within_bs_window = self.after_backspace_at.is_some_and(|bs| now.saturating_duration_since(bs) <= IME_REWRITE_WINDOW);
            let continuing_ime_burst = self.ime_rewrite_active && self.last_plain_char_time.is_some_and(|prev| now.saturating_duration_since(prev) <= char_interval);
            if (within_bs_window || continuing_ime_burst) && self.ime_rewrite_chars < IME_REWRITE_MAX_CHARS {
                self.ime_rewrite_active = true;
                self.ime_rewrite_chars = self.ime_rewrite_chars.saturating_add(1);
                self.last_plain_char_time = Some(now);
                self.consecutive_plain_char_burst = 0;
                self.max_fast_run = 0;
                return CharDecision::Passthrough;
            }
            self.ime_rewrite_active = false;
            self.ime_rewrite_chars = 0;
            self.after_backspace_at = None;
        }

        if self.active {
            self.note_plain_char(now, char_interval);
            self.max_fast_run = self.max_fast_run.max(self.consecutive_plain_char_burst);
            self.burst_window_until = Some(now + PASTE_ENTER_SUPPRESS_WINDOW);
            return CharDecision::BufferAppend;
        }

        if let Some((held, held_at)) = self.pending_first_char {
            if now.saturating_duration_since(held_at) <= char_interval {
                self.note_plain_char(now, char_interval);
                self.active = true;
                self.burst_started_at = Some(held_at);
                self.max_fast_run = self.consecutive_plain_char_burst.max(2);
                let _ = self.pending_first_char.take();
                self.buffer.push(held);
                self.burst_window_until = Some(now + PASTE_ENTER_SUPPRESS_WINDOW);
                return CharDecision::BeginBufferFromPending;
            }
        }

        self.consecutive_plain_char_burst = 1;
        self.max_fast_run = 1;
        self.last_plain_char_time = Some(now);
        self.pending_first_char = Some((ch, now));
        CharDecision::RetainFirstChar
    }

    fn note_plain_char(&mut self, now: Instant, char_interval: Duration) {
        match self.last_plain_char_time {
            Some(prev) if now.saturating_duration_since(prev) <= char_interval => {
                self.consecutive_plain_char_burst = self.consecutive_plain_char_burst.saturating_add(1);
            }
            _ => self.consecutive_plain_char_burst = 1,
        }
        self.last_plain_char_time = Some(now);
    }

    pub(crate) fn append_char_to_buffer(&mut self, ch: char, now: Instant) {
        if self.burst_started_at.is_none() {
            self.burst_started_at = Some(now);
        }
        self.buffer.push(ch);
        self.last_plain_char_time = Some(now);
        self.burst_window_until = Some(now + PASTE_ENTER_SUPPRESS_WINDOW);
    }

    /// Accumulates `'\r'`, `'\n'`, or `'\t'` into the burst instead of executing `Enter` or `Tab`.
    /// Promotes a held `pending_first_char` so single-char first lines (`"N\r\n..."`) stay inside
    /// the burst buffer without auto-submitting.
    pub(crate) fn append_control_char_if_active(&mut self, ch: char, now: Instant) -> bool {
        if self.is_active() {
            let char_interval = Self::char_interval();
            if let Some((held, held_at)) = self.pending_first_char.take() {
                if self.burst_started_at.is_none() {
                    self.burst_started_at = Some(held_at);
                }
                self.buffer.push(held);
            } else if self.burst_started_at.is_none() {
                self.burst_started_at = Some(now);
            }
            self.note_plain_char(now, char_interval);
            self.max_fast_run = self.max_fast_run.max(self.consecutive_plain_char_burst);
            self.active = true;
            self.buffer.push(ch);
            self.burst_window_until = Some(now + PASTE_ENTER_SUPPRESS_WINDOW);
            true
        } else {
            false
        }
    }

    pub(crate) fn take_typed_burst_flag(&mut self) -> bool {
        std::mem::take(&mut self.last_flush_was_typed_burst)
    }

    pub(crate) fn flush_if_due(&mut self, now: Instant, is_windows: bool) -> FlushResult {
        let timeout = if self.is_active_internal() { Self::active_idle_timeout(is_windows) } else { Self::char_interval() };
        let timed_out = self.last_plain_char_time.is_some_and(|t| now.saturating_duration_since(t) > timeout);
        if self.is_human_typed_burst() {
            if let Some(started) = self.burst_started_at {
                let burst_span = self.last_plain_char_time.map(|last| last.saturating_duration_since(started)).unwrap_or_default();
                let live_cap_hit = !timed_out && now.saturating_duration_since(started) >= SINGLE_LINE_BURST_WALL_CAP;
                if burst_span >= SINGLE_LINE_BURST_WALL_CAP || live_cap_hit {
                    self.active = false;
                    self.burst_started_at = None;
                    self.burst_window_until = None;
                    self.last_plain_char_time = None;
                    self.consecutive_plain_char_burst = 0;
                    self.max_fast_run = 0;
                    self.last_flush_was_typed_burst = true;
                    let out = std::mem::take(&mut self.buffer);
                    return FlushResult::Paste(out);
                }
            }
        }
        if timed_out {
            self.flush_pending()
        } else {
            FlushResult::None
        }
    }

    pub(crate) fn flush_pending(&mut self) -> FlushResult {
        self.last_flush_was_typed_burst = false;
        if self.is_active_internal() {
            let was_typed = self.is_human_typed_burst();
            self.active = false;
            self.burst_started_at = None;
            self.last_plain_char_time = None;
            self.consecutive_plain_char_burst = 0;
            self.max_fast_run = 0;
            self.last_flush_was_typed_burst = was_typed;
            let out = std::mem::take(&mut self.buffer);
            // Keep the 120ms Enter suppression window alive for multi-line pastes and machine-speed
            // single-line pastes (>= 16 chars), while clearing it on human-typed bursts and short
            // (< 16 char) single-line inputs so interactive Enter submits immediately.
            if !out.contains(['\r', '\n']) && (was_typed || out.chars().count() < 16) {
                self.burst_window_until = None;
            }
            FlushResult::Paste(out)
        } else if let Some((ch, _at)) = self.pending_first_char.take() {
            self.last_plain_char_time = None;
            self.consecutive_plain_char_burst = 0;
            self.max_fast_run = 0;
            FlushResult::Typed(ch)
        } else {
            FlushResult::None
        }
    }

    /// Flushes any pending first char or active burst buffer immediately before a non-text key
    /// (arrows, Backspace, Esc, Ctrl shortcuts, or post-rollover Enter/Tab). Preserves the
    /// typed-vs-paste distinction on `last_flush_was_typed_burst` for `apply_flush_result`.
    pub(crate) fn flush_before_modified_input(&mut self) -> FlushResult {
        let res = self.flush_pending();
        let was_typed = self.last_flush_was_typed_burst;
        self.clear_window_after_non_char();
        self.last_flush_was_typed_burst = was_typed;
        res
    }

    pub(crate) fn newline_should_insert_instead_of_submit(&self, now: Instant) -> bool {
        let in_burst_window = self.burst_window_until.is_some_and(|until| now <= until);
        self.is_active() || in_burst_window
    }

    pub(crate) fn extend_window(&mut self, now: Instant) {
        self.burst_window_until = Some(now + PASTE_ENTER_SUPPRESS_WINDOW);
    }

    pub(crate) fn clear_window_after_non_char(&mut self) {
        self.consecutive_plain_char_burst = 0;
        self.max_fast_run = 0;
        self.last_plain_char_time = None;
        self.burst_window_until = None;
        self.burst_started_at = None;
        self.after_backspace_at = None;
        self.ime_rewrite_active = false;
        self.ime_rewrite_chars = 0;
        self.last_flush_was_typed_burst = false;
        self.active = false;
        self.pending_first_char = None;
    }

    pub(crate) fn clear_after_explicit_paste(&mut self) {
        self.last_plain_char_time = None;
        self.consecutive_plain_char_burst = 0;
        self.max_fast_run = 0;
        self.burst_window_until = None;
        self.burst_started_at = None;
        self.after_backspace_at = None;
        self.ime_rewrite_active = false;
        self.ime_rewrite_chars = 0;
        self.last_flush_was_typed_burst = false;
        self.active = false;
        self.buffer.clear();
        self.pending_first_char = None;
    }

    pub(crate) fn is_active(&self) -> bool {
        self.is_active_internal() || self.pending_first_char.is_some()
    }

    fn is_active_internal(&self) -> bool {
        self.active || !self.buffer.is_empty()
    }
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn first_char_is_held_then_flushes_as_typed_for_ascii_and_vietnamese() {
        for ch in ['N', 'ế', 'Đ'] {
            let mut burst = PasteBurst::default();
            let t0 = Instant::now();
            assert_eq!(burst.on_plain_char(ch, t0), CharDecision::RetainFirstChar);
            assert!(burst.is_active());

            let t1 = t0 + PasteBurst::recommended_char_flush_delay();
            assert_eq!(burst.flush_if_due(t1, true), FlushResult::Typed(ch));
            assert!(!burst.is_active());
            assert!(!burst.newline_should_insert_instead_of_submit(t1));
        }
    }

    #[test]
    fn mixed_ascii_and_vietnamese_prefix_buffers_cleanly_without_leaking_first_char() {
        let mut burst = PasteBurst::default();
        let mut now = Instant::now();
        let text = "Nếu spawn process mới\r\ndòng 2";
        for (idx, ch) in text.chars().enumerate() {
            match ch {
                '\r' | '\n' => assert!(burst.append_control_char_if_active(ch, now)),
                c => match burst.on_plain_char(c, now) {
                    CharDecision::RetainFirstChar => assert_eq!(idx, 0),
                    CharDecision::BeginBufferFromPending | CharDecision::BufferAppend => {
                        burst.append_char_to_buffer(c, now);
                    }
                    CharDecision::Passthrough => panic!("unexpected passthrough without Backspace"),
                },
            }
            now += Duration::from_millis(1);
        }
        let flush_at = now + PasteBurst::recommended_active_flush_delay(true);
        assert_eq!(burst.flush_if_due(flush_at, true), FlushResult::Paste(text.to_string()));
        assert!(burst.newline_should_insert_instead_of_submit(flush_at));
        let after_suppress = now + PASTE_ENTER_SUPPRESS_WINDOW + Duration::from_millis(1);
        assert!(!burst.newline_should_insert_instead_of_submit(after_suppress));
    }

    #[test]
    fn vietnamese_telex_post_backspace_rewrite_passes_through_immediately_and_never_suppresses_enter() {
        let mut burst = PasteBurst::default();
        let mut now = Instant::now();
        for _ in 0..3 {
            let _ = burst.flush_before_modified_input();
            burst.note_backspace(now);
            now += Duration::from_millis(1);
        }
        for ch in "ếng".chars() {
            assert_eq!(burst.on_plain_char(ch, now), CharDecision::Passthrough);
            assert!(!burst.is_active());
            assert!(!burst.newline_should_insert_instead_of_submit(now));
            now += Duration::from_millis(1);
        }
        assert!(!burst.newline_should_insert_instead_of_submit(now));
    }

    #[test]
    fn continuous_windows_single_line_typing_caps_at_120ms_with_typed_burst_flag_and_clears_enter_window() {
        let mut burst = PasteBurst::default();
        let t0 = Instant::now();
        let mut now = t0;
        assert_eq!(burst.on_plain_char('t', now), CharDecision::RetainFirstChar);
        now += Duration::from_millis(4);
        assert_eq!(burst.on_plain_char('h', now), CharDecision::BeginBufferFromPending);
        burst.append_char_to_buffer('h', now);
        for ch in ['e', ' ', 'q'] {
            now += Duration::from_millis(35);
            assert_eq!(burst.on_plain_char(ch, now), CharDecision::BufferAppend);
            burst.append_char_to_buffer(ch, now);
        }
        now += Duration::from_millis(20);
        assert_eq!(burst.flush_if_due(now, true), FlushResult::Paste("the q".into()));
        assert!(burst.take_typed_burst_flag());
        assert!(!burst.is_active());
        assert!(!burst.newline_should_insert_instead_of_submit(now));
    }

    #[test]
    fn conpty_long_first_line_multiline_and_single_line_pastes_never_hit_wall_cap_and_preserve_enter_window() {
        let mut burst = PasteBurst::default();
        let t0 = Instant::now();
        let feed_plain = |b: &mut PasteBurst, ch: char, at: Instant| match b.on_plain_char(ch, at) {
            CharDecision::RetainFirstChar => {}
            CharDecision::BeginBufferFromPending | CharDecision::BufferAppend => b.append_char_to_buffer(ch, at),
            CharDecision::Passthrough => panic!("unexpected passthrough"),
        };

        // 150-char Line 1 arriving in 3 x 50-char ConPTY chunks at t = 0ms, 55ms, 110ms
        for chunk_idx in 0..3 {
            let chunk_time = t0 + Duration::from_millis(chunk_idx * 55);
            for _ in 0..50 {
                feed_plain(&mut burst, 'a', chunk_time);
            }
        }
        assert_eq!(burst.next_flush_delay(t0 + Duration::from_millis(110), true), Some(Duration::from_millis(61)));
        assert_eq!(burst.flush_if_due(t0 + Duration::from_millis(120), true), FlushResult::None);

        // \r\nLine 2 arrives at t = 122ms and stays inside the same paste burst
        let t_nl = t0 + Duration::from_millis(122);
        assert!(burst.append_control_char_if_active('\r', t_nl));
        assert!(burst.append_control_char_if_active('\n', t_nl));
        for ch in "Line 2".chars() {
            feed_plain(&mut burst, ch, t_nl);
        }
        let flush_at = t_nl + Duration::from_millis(61);
        let expected = format!("{}\r\nLine 2", "a".repeat(150));
        assert_eq!(burst.flush_if_due(flush_at, true), FlushResult::Paste(expected));
        assert!(!burst.take_typed_burst_flag());
        assert!(burst.newline_should_insert_instead_of_submit(flush_at));

        // 60-char single-line machine-speed paste preserves 120ms trailing-Enter suppression window
        let mut single = PasteBurst::default();
        let s0 = Instant::now();
        for _ in 0..60 {
            feed_plain(&mut single, 'x', s0);
        }
        let s_flush = s0 + Duration::from_millis(61);
        assert_eq!(single.flush_if_due(s_flush, true), FlushResult::Paste("x".repeat(60)));
        assert!(!single.take_typed_burst_flag());
        assert!(single.newline_should_insert_instead_of_submit(s0 + Duration::from_millis(65)));
        assert!(!single.newline_should_insert_instead_of_submit(s0 + Duration::from_millis(121)));
    }

    #[test]
    fn human_250_char_9ms_stream_after_rollover_flushes_live_every_120ms() {
        let mut burst = PasteBurst::default();
        let t0 = Instant::now();
        let mut now = t0;
        assert_eq!(burst.on_plain_char('a', now), CharDecision::RetainFirstChar);
        now += Duration::from_millis(5);
        assert_eq!(burst.on_plain_char('b', now), CharDecision::BeginBufferFromPending);
        burst.append_char_to_buffer('b', now);

        let mut inline_flushed = String::from("");
        for _ in 0..248 {
            now += Duration::from_millis(9);
            match burst.flush_if_due(now, true) {
                FlushResult::Typed(c) => inline_flushed.push(c),
                FlushResult::Paste(s) => {
                    assert!(burst.take_typed_burst_flag(), "9ms human stream must flush as typed burst");
                    assert!(s.chars().count() <= 20, "buffered slice exceeded live bound: {}", s.chars().count());
                    inline_flushed.push_str(&s);
                }
                FlushResult::None => {}
            }
            match burst.on_plain_char('k', now) {
                CharDecision::RetainFirstChar => {}
                CharDecision::BeginBufferFromPending | CharDecision::BufferAppend => burst.append_char_to_buffer('k', now),
                CharDecision::Passthrough => inline_flushed.push('k'),
            }
        }
        now += Duration::from_millis(65);
        match burst.flush_if_due(now, true) {
            FlushResult::Typed(c) => inline_flushed.push(c),
            FlushResult::Paste(s) => {
                assert!(burst.take_typed_burst_flag());
                inline_flushed.push_str(&s);
            }
            FlushResult::None => {}
        }
        assert_eq!(inline_flushed.chars().count(), 250);
    }

    #[test]
    fn vietnamese_ime_syllable_commit_without_backspace_plus_160_human_chars_flushes_inline_at_120ms() {
        let mut burst = PasteBurst::default();
        let t0 = Instant::now();
        let mut now = t0;
        for (i, ch) in "nghiêng ".chars().enumerate() {
            match burst.on_plain_char(ch, now) {
                CharDecision::RetainFirstChar => assert_eq!(i, 0),
                CharDecision::BeginBufferFromPending | CharDecision::BufferAppend => burst.append_char_to_buffer(ch, now),
                CharDecision::Passthrough => panic!("unexpected passthrough"),
            }
        }

        let mut collected = String::new();
        for _ in 0..160 {
            now += Duration::from_millis(40);
            match burst.flush_if_due(now, true) {
                FlushResult::Typed(c) => collected.push(c),
                FlushResult::Paste(s) => {
                    assert!(burst.take_typed_burst_flag(), "IME syllable + 40ms human typing must flush as typed burst");
                    collected.push_str(&s);
                }
                FlushResult::None => {}
            }
            match burst.on_plain_char('a', now) {
                CharDecision::RetainFirstChar => {}
                CharDecision::BeginBufferFromPending | CharDecision::BufferAppend => burst.append_char_to_buffer('a', now),
                CharDecision::Passthrough => collected.push('a'),
            }
        }
        now += Duration::from_millis(10);
        match burst.flush_if_due(now, true) {
            FlushResult::Typed(c) => collected.push(c),
            FlushResult::Paste(s) => {
                assert!(burst.take_typed_burst_flag());
                collected.push_str(&s);
            }
            FlushResult::None => {}
        }
        assert_eq!(collected.chars().count(), 168);
        assert!(!burst.newline_should_insert_instead_of_submit(now));
    }

    #[test]
    fn human_rollover_flushes_before_control_and_flush_before_modified_input_preserves_typed_flag() {
        let mut burst = PasteBurst::default();
        let t0 = Instant::now();
        assert_eq!(burst.on_plain_char('n', t0), CharDecision::RetainFirstChar);
        let t1 = t0 + Duration::from_millis(5);
        assert_eq!(burst.on_plain_char('g', t1), CharDecision::BeginBufferFromPending);
        burst.append_char_to_buffer('g', t1);

        assert!(!burst.should_flush_human_rollover_before_control(t0 + Duration::from_millis(7)));
        let t_enter = t0 + Duration::from_millis(35);
        assert!(burst.should_flush_human_rollover_before_control(t_enter));
        assert_eq!(burst.flush_before_modified_input(), FlushResult::Paste("ng".into()));
        assert!(burst.take_typed_burst_flag());
        assert!(!burst.newline_should_insert_instead_of_submit(t_enter));
    }
}
