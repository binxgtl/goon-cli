//! User-requested external editor handoff. Only the input thread launches the editor.
use super::*;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
#[cfg(unix)]
#[path = "editor_unix.rs"]
mod unix;

struct EditorChild {
    child: std::process::Child,
    completed: bool,
}
impl Drop for EditorChild {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        #[cfg(unix)]
        unix::kill_group(self.child.id());
        #[cfg(windows)]
        {
            let _ = Command::new("taskkill").args(["/PID", &self.child.id().to_string(), "/T", "/F"]).stdout(Stdio::null()).stderr(Stdio::null()).status();
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn editor_command(visual: Option<String>, editor: Option<String>) -> Result<Vec<String>> {
    let raw = visual.or(editor).ok_or_else(|| anyhow!("Set VISUAL or EDITOR before opening the external editor."))?;
    #[cfg(not(windows))]
    let parts = shlex::split(&raw).ok_or_else(|| anyhow!("Failed to parse editor command."))?;
    #[cfg(windows)]
    let parts = split_windows_command(&raw);
    if parts.first().map(|s| s.is_empty()).unwrap_or(true) {
        return Err(anyhow!("Editor command is empty."));
    }
    Ok(parts)
}

// CommandLineToArgvW-style quoting. Backslashes are literal unless immediately
// before a double quote; unlike POSIX shlex this preserves C:\Users\... paths.
#[cfg(any(windows, test))]
fn split_windows_command(raw: &str) -> Vec<String> {
    let mut parts = vec![];
    let mut arg = String::new();
    let mut quoted = false;
    let mut started = false;
    let mut chars = raw.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            let mut count = 1;
            while chars.peek() == Some(&'\\') {
                chars.next();
                count += 1;
            }
            if chars.peek() == Some(&'"') {
                arg.push_str(&"\\".repeat(count / 2));
                chars.next();
                if count % 2 == 0 {
                    quoted = !quoted;
                } else {
                    arg.push('"');
                }
            } else {
                arg.push_str(&"\\".repeat(count));
            }
            started = true;
        } else if ch == '"' {
            quoted = !quoted;
            started = true;
        } else if ch.is_whitespace() && !quoted {
            if started {
                parts.push(std::mem::take(&mut arg));
                started = false;
            }
        } else {
            arg.push(ch);
            started = true;
        }
    }
    if started {
        parts.push(arg);
    }
    parts
}

fn resolve_program(program: &str) -> PathBuf {
    #[cfg(windows)]
    {
        let path = Path::new(program);
        let dirs = if path.components().count() > 1 { vec![PathBuf::new()] } else { std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).collect() };
        let extensions = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
        for dir in dirs {
            let candidate = dir.join(path);
            if candidate.is_file() {
                return candidate;
            }
            if path.extension().is_none() {
                for extension in extensions.split(';').filter(|s| !s.is_empty()) {
                    let candidate = dir.join(format!("{program}{extension}"));
                    if candidate.is_file() {
                        return candidate;
                    }
                }
            }
        }
    }
    PathBuf::from(program)
}

fn edit_file(seed: &str, command: &[String], directory: &Path, closed: impl Fn() -> bool) -> Result<String> {
    crate::config::mkdir_private(directory)?;
    if std::fs::symlink_metadata(directory)?.file_type().is_symlink() {
        return Err(anyhow!("Editor directory must not be a symbolic link."));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))?;
    }
    // No open handle during editor execution: Windows editors often replace the file.
    let path = tempfile::Builder::new().prefix("draft-").suffix(".md").tempfile_in(directory)?.into_temp_path();
    std::fs::write(&path, seed)?;
    let mut command_builder = Command::new(resolve_program(&command[0]));
    command_builder.args(&command[1..]).arg(&path).env_clear().envs(crate::process::child_env(None)).stdin(Stdio::inherit()).stdout(Stdio::inherit()).stderr(Stdio::inherit());
    #[cfg(unix)]
    let foreground = unix::Foreground::prepare(&mut command_builder)?;
    // Declare child after foreground: error/unwind kills/reaps the child before
    // returning terminal ownership to Goon.
    let mut child = EditorChild { child: command_builder.spawn()?, completed: false };
    let status = loop {
        if closed() {
            return Err(anyhow!("Terminal closed while editing."));
        }
        if let Some(status) = child.child.try_wait()? {
            child.completed = true;
            break status;
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    #[cfg(unix)]
    drop(foreground);
    if !status.success() {
        return Err(anyhow!("Editor exited with status {status}."));
    }
    const MAX_DRAFT_BYTES: u64 = 4 * 1024 * 1024;
    use std::io::Read;
    let mut bytes = vec![];
    std::fs::File::open(&path)?.take(MAX_DRAFT_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_DRAFT_BYTES {
        return Err(anyhow!("Edited draft exceeds 4 MiB."));
    }
    Ok(String::from_utf8(bytes)?)
}

impl Tui {
    pub fn external_editor_active(&self) -> bool {
        self.state.lock().unwrap().editor_active
    }

    pub async fn wait_for_editor_cleanup(&self) {
        loop {
            let finished = {
                let st = self.state.lock().unwrap();
                !st.editor_active && !st.input_yielded && (!st.closed || st.terminal_restored)
            };
            if finished {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    pub(super) fn apply_external_edit(st: &mut State, text: &str) {
        st.input = chars_of(&terminal_text(text.trim_end()));
        st.cursor = st.input.len();
        st.vertical_goal_col = None;
        st.pasted_texts.clear();
        st.attached_images.retain(|token, _| text.contains(token));
        st.history_index = st.history.len();
        st.history_draft = None;
        st.input_restored_from_revert = false;
        st.paste_burst.clear_after_explicit_paste();
        Self::clear_paste_burst(st);
    }

    pub(super) fn run_external_editor(&self) {
        let command = editor_command(std::env::var("VISUAL").ok(), std::env::var("EDITOR").ok());
        let seed = {
            let mut st = self.state.lock().unwrap();
            if !st.editor_requested || st.closed {
                return;
            }
            st.editor_requested = false;
            let command = match command {
                Ok(command) => command,
                Err(error) => {
                    st.set_raw_notice(&error.to_string());
                    self.schedule(&mut st, 0);
                    return;
                }
            };
            if st.picker.is_some() || st.masked || (st.pending.is_some() && !st.remember) {
                return;
            }
            // Keep image labels for attachment reconciliation; expand only paste payloads.
            let mut seed = st.input.iter().collect::<String>();
            for (token, payload) in &st.pasted_texts {
                seed = seed.replace(token, payload);
            }
            st.set_raw_notice("Save and close external editor to continue.");
            self.render(&mut st);
            st.editor_active = true;
            st.mouse_capture = false;
            self.write_raw("\x1b[?1002l\x1b[?1004l\x1b[?2004l\x1b[?1007l\x1b[?1006l\x1b[?1000l\x1b[?25h");
            if let Some(native) = st.native.as_mut() {
                self.write_raw(&native.finish());
            }
            (seed, command)
        };
        let _guard = EditorGuard(self);
        #[cfg(windows)]
        let prepared: Result<()> = if self.real_terminal { windows_console::restore_input_mode().map_err(Into::into) } else { Ok(()) };
        #[cfg(not(windows))]
        let prepared: Result<()> = Ok(());
        let result = prepared.and_then(|()| edit_file(&seed.0, &seed.1, &crate::config::data_dir().join("editor"), || self.is_closed()));
        let mut st = self.state.lock().unwrap();
        if !st.closed {
            match result {
                Ok(text) => {
                    Self::apply_external_edit(&mut st, &text);
                    st.set_raw_notice("External edit applied · Enter to submit");
                }
                Err(error) => st.set_raw_notice(&format!("Failed to open editor: {error}")),
            }
        }
    }
}

pub(super) struct EditorGuard<'a>(pub(super) &'a Tui);
impl Drop for EditorGuard<'_> {
    fn drop(&mut self) {
        let tui = self.0;
        let mut st = tui.state.lock().unwrap_or_else(|e| e.into_inner());
        st.editor_active = false;
        if st.closed || std::thread::panicking() {
            st.closed = true;
            st.spinning = false;
            st.spin_gen = st.spin_gen.wrapping_add(1);
            drop(st);
            tui.restore_terminal();
            return;
        }
        if tui.real_terminal {
            flush_terminal_input();
            // Editors may restore cooked settings themselves. Reset Crossterm's
            // cached raw-mode flag before reapplying the actual terminal mode.
            let _ = crossterm::terminal::disable_raw_mode();
            if let Err(error) = crossterm::terminal::enable_raw_mode() {
                st.set_raw_notice(&format!("Failed to restore raw input: {error}"));
            } else {
                super::set_panic_terminal_state(true, st.native.is_none());
            }
            #[cfg(windows)]
            let _ = windows_console::set_input_record_mode();
        }
        tui.write_raw("\x1b[?1004h\x1b[?2004h\x1b[?1007h\x1b[?25l");
        if let Some(native) = st.native.as_mut() {
            let (cols, rows) = tui.size();
            native.resize(cols, cols, rows, None);
        }
        st.last_frame_rows.clear();
        st.needs_full_clear = true;
        tui.schedule(&mut st, 0);
    }
}

fn flush_terminal_input() {
    #[cfg(unix)]
    unsafe {
        libc::tcflush(libc::STDIN_FILENO, libc::TCIFLUSH);
    }
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Console::{FlushConsoleInputBuffer, GetStdHandle, STD_INPUT_HANDLE};
        FlushConsoleInputBuffer(GetStdHandle(STD_INPUT_HANDLE));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn commands_prefer_visual_and_preserve_windows_paths() {
        assert!(editor_command(None, None).is_err());
        assert!(editor_command(Some("".into()), Some("vi".into())).is_err());
        assert_eq!(editor_command(Some("editor --wait".into()), Some("ignored".into())).unwrap(), vec!["editor", "--wait"]);
        assert_eq!(split_windows_command(r#""C:\Program Files\Editor\edit.exe" --wait "C:\Users\A B""#), vec![r"C:\Program Files\Editor\edit.exe", "--wait", r"C:\Users\A B"]);
    }

    #[test]
    fn closing_editor_reaps_child_and_removes_private_draft() {
        let dir = tempfile::tempdir().unwrap();
        let editor_dir = dir.path().join("private editor");
        let cmd = vec!["node".into(), "-e".into(), "setInterval(()=>{},1000)".into()];
        assert!(edit_file("seed", &cmd, &editor_dir, || true).unwrap_err().to_string().contains("Terminal closed"));
        assert_eq!(std::fs::read_dir(&editor_dir).unwrap().count(), 0);
    }

    #[test]
    fn editor_temp_file_is_updated_cleaned_up_and_failures_preserve_seed() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("edit script.mjs");
        std::fs::write(&script, "import fs from 'node:fs';fs.writeFileSync(process.argv[2], 'edited\\n');").unwrap();
        let cmd = vec!["node".into(), script.to_string_lossy().to_string()];
        let editor_dir = dir.path().join("private editor");
        assert_eq!(edit_file("seed", &cmd, &editor_dir, || false).unwrap(), "edited\n");
        assert_eq!(std::fs::read_dir(&editor_dir).unwrap().count(), 0);
        let cmd = vec!["node".into(), "-e".into(), "process.exit(3)".into()];
        assert!(edit_file("seed", &cmd, &editor_dir, || false).unwrap_err().to_string().contains("status"));
        assert_eq!(std::fs::read_dir(&editor_dir).unwrap().count(), 0);
    }
}
