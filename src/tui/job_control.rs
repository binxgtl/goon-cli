//! POSIX foreground job-control handoff, executed only by the sole input reader.
use super::*;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};

pub(super) fn is_suspend(key: KeyEvent) -> bool {
    key.kind != KeyEventKind::Release
        && ((key.code == KeyCode::Char('z') && key.modifiers == KeyModifiers::CONTROL) || (key.code == KeyCode::Char('\u{1a}') && (key.modifiers.is_empty() || key.modifiers == KeyModifiers::CONTROL)))
}

impl Tui {
    pub(super) fn request_suspend(&self) {
        let mut st = self.state.lock().unwrap();
        if st.closed || st.editor_active || st.editor_requested || st.input_yielded {
            return;
        }
        // Hold any burst text in the draft; suspend must never submit it.
        match st.paste_burst.flush_before_modified_input() {
            paste_burst::FlushResult::Typed(ch) => Self::insert_chars(&mut st, &[ch]),
            paste_burst::FlushResult::Paste(text) => self.handle_paste(&mut st, &text, true),
            paste_burst::FlushResult::None => {}
        }
        st.suspend_requested = true;
    }

    pub(super) fn run_suspend(&self) {
        self.suspend_with(|| suspend_process(|| self.is_closed()));
    }

    pub(super) fn suspend_with(&self, stop: impl FnOnce() -> io::Result<()>) {
        {
            let mut st = self.state.lock().unwrap();
            if !st.suspend_requested || st.closed || st.editor_active || st.editor_requested {
                st.suspend_requested = false;
                return;
            }
            st.suspend_requested = false;
            st.input_yielded = true;
            st.selection_dragging = false;
            st.mouse_capture = false;
            self.write_raw("\x1b[?2026l\x1b[?1002l\x1b[?1004l\x1b[?2004l\x1b[?1007l\x1b[?1006l\x1b[?1000l\x1b[?25h");
            if let Some(native) = st.native.as_mut() {
                self.write_raw(&native.finish());
            } else {
                self.write_raw("\x1b[?1049l");
            }
            if self.real_terminal {
                super::set_panic_terminal_state(false, false);
            }
        }
        let mut guard = ResumeGuard { tui: self, error: None };
        let result = if self.real_terminal { crossterm::terminal::disable_raw_mode().and_then(|()| stop()) } else { stop() };
        guard.error = result.err();
    }
}

struct ResumeGuard<'a> {
    tui: &'a Tui,
    error: Option<io::Error>,
}

impl Drop for ResumeGuard<'_> {
    fn drop(&mut self) {
        let tui = self.tui;
        let mut st = tui.state.lock().unwrap_or_else(|e| e.into_inner());
        if st.closed {
            // Modes/screen were already yielded. The shell may have written or
            // moved its cursor since then; a second native.finish() would touch
            // output that is no longer ours. Only restore the raw-mode cache.
            if tui.real_terminal {
                let _ = crossterm::terminal::disable_raw_mode();
                super::set_panic_terminal_state(false, false);
            }
            st.input_yielded = false;
            st.terminal_restored = true;
            return;
        }
        if tui.real_terminal {
            // Reset Crossterm's cache: the shell may have restored cooked modes.
            let _ = crossterm::terminal::disable_raw_mode();
            if let Err(error) = crossterm::terminal::enable_raw_mode() {
                self.error = Some(error);
            } else {
                super::set_panic_terminal_state(true, st.native.is_none());
            }
            unsafe {
                libc::tcflush(libc::STDIN_FILENO, libc::TCIFLUSH);
            }
        }
        if let Some(native) = st.native.as_mut() {
            let (cols, rows) = tui.size();
            native.resize(cols, cols, rows, None);
        } else {
            tui.write_raw("\x1b[?1049h");
        }
        tui.write_raw("\x1b[?1004h\x1b[?2004h\x1b[?1007h\x1b[?25l");
        st.input_yielded = false;
        if let Some(error) = &self.error {
            st.status_notice = Some(format!("Suspend/resume failed: {}", terminal_text(&error.to_string())));
        }
        st.last_frame_rows.clear();
        st.needs_full_clear = true;
        tui.schedule(&mut st, 0);
    }
}

fn owns_foreground() -> bool {
    unsafe { libc::tcgetpgrp(libc::STDIN_FILENO) == libc::getpgrp() }
}

fn suspend_process(closed: impl Fn() -> bool) -> io::Result<()> {
    if !owns_foreground() {
        return Err(io::Error::other("Goon does not own the foreground terminal"));
    }
    static RESUMED: AtomicBool = AtomicBool::new(false);
    extern "C" fn resumed(_: libc::c_int) {
        RESUMED.store(true, Ordering::Release);
    }
    // SIGCONT may be delivered to any thread. The handler only touches an atomic;
    // no allocation, locks, terminal I/O, or async runtime work runs in it.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        let mut previous: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = resumed as *const () as libc::sighandler_t;
        action.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut action.sa_mask);
        if libc::sigaction(libc::SIGCONT, &action, &mut previous) != 0 {
            return Err(io::Error::last_os_error());
        }
        struct Signals {
            action: libc::sigaction,
            mask: Option<libc::sigset_t>,
        }
        impl Drop for Signals {
            fn drop(&mut self) {
                unsafe {
                    if let Some(mask) = &self.mask {
                        libc::pthread_sigmask(libc::SIG_SETMASK, mask, std::ptr::null_mut());
                    }
                    libc::sigaction(libc::SIGCONT, &self.action, std::ptr::null_mut());
                }
            }
        }
        let mut saved = Signals { action: previous, mask: None };
        let mut signals: libc::sigset_t = std::mem::zeroed();
        let mut mask: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut signals);
        libc::sigaddset(&mut signals, libc::SIGCONT);
        libc::sigaddset(&mut signals, libc::SIGTSTP);
        let error = libc::pthread_sigmask(libc::SIG_UNBLOCK, &signals, &mut mask);
        if error != 0 {
            return Err(io::Error::from_raw_os_error(error));
        }
        saved.mask = Some(mask);
        if closed() {
            return Ok(());
        }
        RESUMED.store(false, Ordering::Release);
        if libc::kill(0, libc::SIGTSTP) != 0 {
            return Err(io::Error::last_os_error());
        }
        // Count execution opportunities, not wall time: the process may stop
        // after kill returns but before the SIGCONT handler runs on any thread.
        for _ in 0..1000 {
            if RESUMED.load(Ordering::Acquire) || closed() {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        if !RESUMED.load(Ordering::Acquire) && !closed() {
            return Err(io::ErrorKind::TimedOut.into());
        }
        // `bg` continues execution without terminal ownership. Do not repaint or
        // enable raw mode until `fg` actually returns ownership (or shutdown).
        while !owns_foreground() && !closed() {
            std::thread::sleep(Duration::from_millis(25));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suspend_matcher_rejects_modified_release_and_other_keys() {
        assert!(is_suspend(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL)));
        assert!(is_suspend(KeyEvent::new(KeyCode::Char('\u{1a}'), KeyModifiers::NONE)));
        for modifiers in [KeyModifiers::NONE, KeyModifiers::CONTROL | KeyModifiers::SHIFT, KeyModifiers::CONTROL | KeyModifiers::ALT] {
            assert!(!is_suspend(KeyEvent::new(KeyCode::Char('z'), modifiers)));
        }
        assert!(!is_suspend(KeyEvent::new_with_kind(KeyCode::Char('z'), KeyModifiers::CONTROL, KeyEventKind::Release)));
    }
}
