# Codex UX parity — implementation ledger

Reference inspected: `.refs/codex` at `7b44ac1661433ae3a6f375fde873fa7123c935fe`.
This is a behavior migration, not a claim that all Codex features already exist in Goon.
Reference clones remain read-only. The original request is full UX parity, including stopping
child work when the user stops the parent. Incomplete rows below remain required work.

## Evidence and verification

| Flow | Codex source (under `codex-rs/`) | Goon status |
| --- | --- | --- |
| Enter submits/steers, Tab queues while busy | `tui/src/keymap.rs:1710`, `tui/src/chatwidget/input_flow.rs:51`, `tui/src/bottom_pane/chat_composer.rs:3534` | Implemented with active-run acceptance/fallback and same-run stream steering test |
| Newline does not submit | `tui/src/keymap.rs:1721` | Shift/Alt+Enter and Ctrl+J/M; real crossterm and paste-burst regression tests |
| Multiline composer and Unicode editing | `tui/src/bottom_pane/textarea.rs` | Wrapped draft viewport, grapheme left/right/delete, soft-wrap/display-width Up/Down with preferred-column retention, line/word editing, single kill buffer; tests |
| Search history, accept without send, cancel restores draft | `tui/src/bottom_pane/chat_composer.rs:75`, `chat_composer_history.rs`, `message-history/src/lib.rs` | Local search/navigation plus global text-only JSONL history, OS file locks, redaction, bounded reads (4 MiB/1000 entries), shutdown drain; tests |
| Modal prompt preserves draft/attachments | `tui/src/bottom_pane/chat_composer.rs:120`, `tui/src/chatwidget/input_restore.rs` | Central question save/restore; answer/Esc/cancellation tests |
| Esc closes completion before global interrupt | `tui/src/bottom_pane/chat_composer.rs:47` | Slash and file/skill dismissal implemented and tested |
| Esc handles approval before global stop | `tui/src/bottom_pane/mod.rs`, `approval_overlay.rs` | Selectable low/medium-risk choices, arrow/Tab/Enter/letter/number shortcuts, Esc denial; real crossterm decoder tests and paste safety; high-risk typed confirmation retained |
| Stop tree, keep interruption distinct from shutdown | `core/src/agent/control/interrupt.rs`, `legacy.rs:108`, `core/src/tasks/mod.rs:564` | User-required global child interrupt implemented; distinct interrupted state, queue clearing, cleanup wait; integration test across prior parent turns |
| Live progress instead of generic spinner only | `tui/src/chatwidget/input_flow.rs:76`, status surfaces | Tool input previews and reasoning/tool header now surfaced; richer details remain |
| Queue edit/restore | `tui/src/chatwidget/input_restore.rs:253,308,409`, `interaction.rs:242` | Latest queued item replaces current draft; queues own attachments; interruption merges queued prompts and draft with rebased IDs instead of auto-submitting; tests |
| File/skill mentions | `tui/src/bottom_pane/file_search_popup.rs`, `mentions_v2` | Cursor-local selectable fuzzy popup for files/skills, quoted whitespace paths, suffix preservation and Escape dismissal; tests |
| Session lifecycle | `tui/src/resume_picker`, `tui/src/chatwidget/input_restore.rs` | Existing resume/fork/rename plus cross-session text history; in-process thread input snapshots restore draft/cursor/queue/attachments. Startup failure handoff/recovery remains |
| Quit and discovery | `tui/src/chatwidget/interaction.rs:580`, `bottom_pane/shortcut_overlay.rs` | Same-chord Ctrl+C/Ctrl+D confirmation in one-second window; reversible empty-composer `?` help; decoder tests |
| Transcript, wheel, inline/fullscreen | `tui/src/tui.rs`, `tui/scrollback.rs`, transcript modules | Native inline viewport tracks committed source boundaries across width/style changes and front trimming. Fullscreen normal/overlay screens support SGR wheel, logical-offset drag selection/highlight, frozen revisions and reflow-stable plain copy without soft-wrap newlines, Escape-clear-first and follow-tail click. Terminal cursor-origin reflow and richer owned-screen interactions remain |
| Clarification questions | `tui/src/bottom_pane/request_user_input/mod.rs`, `async_questions` | Grouped synchronous form with per-question selection/notes/paste drafts, ←→/PageUp/Down navigation, Tab notes/options, atomic answers, cancellation/drop restore; decoder + ConPTY tests. Unanswered-submit confirmation supports Proceed/Go back, preserves skipped drafts and emits empty arrays rather than defaults. Async delivery/countdowns remain |
| Settings/onboarding | `tui/src/bottom_pane/startup.rs`, config/keymap/settings pickers | `/model` uses all discovered models in a searchable picker then stages effort; Escape returns to models without applying either stage (ConPTY exercised). Plain fallback retained; wider settings/keymaps remain |
| Fatal input recovery | `tui/src/startup_recovery.rs:145-174` | Fatal session errors recover expanded ordinary draft after terminal restoration; modal/secret input excluded and known secrets redacted. Full startup handoff/acknowledgment remains |
| Focus-aware attention | `tui/src/tui.rs`, notification dispatch | Focus reporting decoded independently of composer; attention bell suppressed while focused; lifecycle and ConPTY tests |

## Important distinctions

- The explicit request to stop **all** children includes agents from earlier parent turns.
  Per-turn token inheritance alone does not provide that guarantee. Goon now walks managed
  children on user stop; synchronous delegates also inherit the active parent's token.
- Codex has different V1/V2 interrupt/shutdown mechanisms. Do not claim its ordinary turn
  interrupt always shuts down every child: `legacy.rs` proves tree shutdown, while
  `interrupt.rs` preserves the target thread. Goon's global stop is a user-required behavior.
- Reference UX includes feature-gated plugins, voice, remote daemon, task links and Vim;
  these are **not** implemented by the current changes. Assess all applicable flows before
  marking the original goal complete; passing current tests alone is not full parity.
- Idle Ctrl+C also interrupts managed prior-turn children without exiting on the first press.
  An explicit second same-chord press confirms exit. Text input (including a character held by
  paste-burst detection) disarms confirmation; modal approvals consume their own cancel keys.
- Persistent history is text-only, not conversation replay or unsent-draft recovery. The writer
  drains accepted records on orderly shutdown. A forced process kill or disk error is not a
  durability guarantee, and history cleanup does not replace secret-handling discipline.
- Correction to the earlier ledger: this reference has no ordinary composer draft stack.
  `chat_composer_history.rs:394-411` only browses from empty input or unchanged recalled text
  at a boundary; Down past newest clears input (`440-472`). Queue edit deliberately replaces
  the current draft (`interaction.rs:242-255`). Ctrl+R cancellation separately restores its draft.
- Unsent thread snapshots stay in memory, not a process-global disk file. Codex startup recovery
  (`startup_recovery.rs`) is also task-local memory with explicit handoff/acknowledgment; do not
  invent on-disk draft persistence and call it reference parity.
- Inline mode is initial, not terminal parity: stable rows are inserted only when idle, the live
  tail is relative, and shell output is not cleared. Source-based watermarks prevent requeueing
  committed history on width/style changes, rewind and front trimming; wrapped source lines are
  committed atomically. Resize probes run on the sole input reader (avoiding POSIX poll/query
  contention); unambiguous ASCII hard rows rebase against the measured cursor. Unicode/unknown
  reflow or failed probes use a non-erasing newline recovery, preserving shell output but retaining
  the old live frame in scrollback. This fallback is not complete reflow parity. Zellij insertion
  and soft-wrap URL preservation also remain.

## Additional reference-backed work still required

- Richer owned-screen pointer selection and semantic copy remain.
  Normal fullscreen wheel/drag selection/follow-click are implemented. Wheel is transcript-area
  limited (or active drag), accumulates fractional rows with direction-reset and floating-point
  epsilon snapping, and defaults to one row/tick. Positive finite `mouseScrollSpeed` JSON config
  and `GOON_MOUSE_SCROLL_SPEED` support runtime updates. Extreme speeds saturate without overflow.
  Follow owns its centered responsive render target; press alone, release outside, hidden controls
  and resized targets cannot jump. Matching release returns to latest without cancelling the turn.
  Mouse hit tests refresh pending scroll/layout first, snap wide-grapheme carets, and release
  transcript selection when clicking the composer. Duplicate geometry notifications retain
  selection; actual resize ends only the old drag gesture. Selection now owns a frozen transcript
  revision with logical-line byte anchors and explicit wrap ranges, mirroring Codex
  `transcript_view/selection.rs` and `text_logical.rs`. Rewrap remaps anchors and reveals the selected
  endpoint; streaming replacement and canonical history trimming do not retarget selected text.
  Logical copy excludes soft-wrap newlines and user/answer/code/list/quote/diff gutters while
  preserving hard blank lines. Markup projects before wrapping so links/emphasis retain stable
  offsets across rich/compact widths. Required next work: table-cell semantic/rich Markdown copy,
  per-entry rather than whole-transcript snapshot updates and hard-line selection highlighting.
  Wheel clears stale drag hit coordinates without changing selected text; a subsequent actual drag
  reactivates hit testing. Navigation/presentation changes reset fractional momentum.
- Native viewport cursor-origin resize/reflow without scrolling live rows, active-cell commit tracking, overlay isolation,
  launch-time transcript ownership config, and balanced behavior under Windows Terminal/Zellij/tmux.
- Complete startup input handoff/acknowledgment and asynchronous question delivery/countdowns.
  Selected synchronous options wrap and cannot submit when clipped; empty freeform answers require
  explicit unanswered confirmation, with real decoder coverage for navigation and cancellation.
- Reversible settings and keymap picker remain. Ctrl+G external
  editing now runs on the sole input thread with VISUAL/EDITOR argv parsing, private temporary
  markdown, paste expansion, image retention, no auto-submit, input flush/terminal restoration,
   busy-turn approval deferral and SIGINT ownership. Windows ConPTY and Linux controlling-PTY
   tests exercise the real handoff in fullscreen and inline. Full sandbox-directory isolation
   remains unverified. Editor shutdown defers terminal teardown until the child is reaped,
   and approvals atomically wait for Requested/Active editor ownership. Unix editors use a
   separate foreground process group before exec, restoring Goon's group after exit; SIGTERM
   cancellation kills the editor group and descendants before terminal restoration. An immediate
   editor stdin read and a real editor/grandchild cancellation are covered on Linux.
   Native full-height modals now restore the compact viewport at the bottom on exit without
   entering alternate screen (Codex `tui.rs:1083` also honors no-alt-screen for overlays).
    Pending finalized history reaches actual scrollback before compact repaint; a physical-row
    terminal model checks short/long pending batches, full-height/compact exits and reopening.
     Resize uncertainty still takes the documented non-erasing fallback; richer isolation remains.
  Unix Ctrl+Z now intercepts on the sole input reader before widgets, as in
  `tui/event_stream.rs:280-294` / `tui/job_control.rs`. Goon yields render/input ownership,
  restores cooked mode and the main screen, sends SIGTSTP only while owning its foreground
  process group, then restores raw mode and repaints without cancelling the run. `bg` waits
  for foreground ownership instead of stealing the shell's tty. Native inline recovery uses
  the non-erasing fallback. Paste/editor ownership excludes suspend; new approvals defer
  atomically until resume, and shutdown while yielded never reenters raw/alternate mode.
  Real Linux PTY supervisor tests cover non-orphaned job stop, cooked mode, bg/fg ownership,
  unsent draft preservation, fullscreen/inline resume and SIGTERM while suspended. macOS,
  terminal multiplexer, and externally delivered SIGTSTP behavior remain unverified.
- Richer focus/program-status integration, live progress/expandable tool output, hyperlink behavior,
  independently configurable footer/status, and non-destructive backtrack preview.

## Checks at this stage

- Latest Windows Rust run: 204 library + 1 parity + 327 integration = 532 passed.
- Latest Linux/WSL Ubuntu 24.04 Rust run: 208 library + 1 parity + 339 integration = 548 passed.
- Node contracts/E2E against rebuilt binaries: Windows 81 passed, 7 POSIX/Linux-only tests skipped;
  Linux 88 passed, 0 skipped. Both suites have 0 failures.
- Regression reproduced before fix: Escape while reading scrolled history dispatched Cancel before
  returning to latest during Running/Compacting/Stopping. Navigation now consumes that Escape first;
  a distinct Escape at the live tail still stops the turn. Escape/Ctrl+T closing Transcript resets
  hidden scroll and preserves the running turn/draft. Ctrl+End returns fullscreen transcript to
  latest without moving the composer caret. Decoder tests cover both Windows/POSIX branches,
  fullscreen/inline overlays and existing approval-Escape precedence.
- `cargo fmt --check`, `git diff --check`,
  `cargo clippy --all-targets --locked -j 2 -- -D warnings`: passed.
- `cargo build --locked -j 2` and all-target Clippy: passed on Windows and Linux;
  Node suites used each platform's rebuilt native binary (Linux via `GOON_BINARY`).
- `npx --no-install oxlint tests bin scripts`: exit 0, with one pre-existing unnecessary-escape
  warning in `tests/parity/synth.mjs` (no errors).
- Windows process-tree cancellation exercised with a real Node parent/grandchild, not a mocked
  token: cancellation returns only after the grandchild has exited. App.stop, prior-turn managed
  children, queued follow-ups, synchronous delegates, idempotent stop and resumability are tested.
- Dropped terminal-approval futures (remote race) restore drafts and remove stale pending prompts.
  Masked prompts avoid archiving paste content; modal editing state is restored on drop.
- Real decoder tests cover selectable approvals on both Windows/POSIX decoder branches,
  shortcut help, quit confirmation/disarming, and idle interrupt dispatch. History tests cover
  concurrent writers, malformed records, the latest-1000 limit and immediate shutdown drain.
- Windows ConPTY end-to-end tests now exercise both fullscreen and inline modes: `/status`,
  model→effort back/cancel without saving config, repeated 46×14/110×32/80×24 shrink/grow
  with fresh draft repaint checks, write approval, actual file output,
  turn completion, exit, terminal mode sequences and
  session lock release. They use the rebuilt native binary and a stdlib-only Python fixture.
- Linux controlling-PTY tests additionally cover editor foreground ownership/restoration,
  process-group descendant cancellation, canonical-mode restoration, temporary-file cleanup,
  status/resize/approval and mode switching. Linux validation uses scoped Rust 1.97.0 / Node 24.11.0.
- No manual visual recording, macOS run or multiplexer verification yet. ConPTY/PTY lifecycle
  coverage does not prove general visual reflow, scrollback duplication, selection or full terminal parity.
- Clarification regressions cover preserving committed notes through caret motion, invalidating
  on real edits (including kill-line), and switching option-list pastes into visible notes mode.
- Shared rendering now measures whole graphemes with `unicode-width`, matching composer editing:
  ZWJ families/professions, skin-tone modifiers, flags, keycaps, variation selectors and combining
  text wrap/clip atomically. ANSI clipping preserves graphemes even with internal SGR changes;
  compact draft windows use the same units. Tests cover pointer copy columns and stale scroll
  hit tests. Six old TypeScript-corpus mismatches are intentional fixes, narrowly documented in
  `tests/parity.rs`; the corpus itself is unchanged.
