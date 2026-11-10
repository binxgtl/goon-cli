//! POSIX job-control handoff for the foreground external editor.
//!
//! Unlike tool commands, an editor must remain in the controlling terminal's session.
//! Give it a distinct process group, make that group foreground before exec (avoids
//! SIGTTIN on its first read), and restore the previous foreground group on every exit.
use std::io;
use std::os::unix::process::CommandExt;
use std::process::Command;

pub(super) struct Foreground {
    previous: Option<libc::pid_t>,
}

impl Foreground {
    pub(super) fn prepare(command: &mut Command) -> io::Result<Self> {
        // SAFETY: tcgetpgrp reads tty state and does not retain any pointers.
        let previous = unsafe { libc::tcgetpgrp(libc::STDIN_FILENO) };
        let previous = if previous < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ENOTTY) {
                None
            } else {
                return Err(error);
            }
        } else {
            // Do not steal the terminal from a different foreground application.
            if previous != unsafe { libc::getpgrp() } {
                return Err(io::Error::other("Goon does not own the foreground terminal"));
            }
            Some(previous)
        };
        command.process_group(0);
        if previous.is_some() {
            // SAFETY: this post-fork closure uses only async-signal-safe libc calls
            // and stack storage. It runs after std has established the child pgid.
            unsafe {
                command.pre_exec(|| set_foreground(libc::getpgrp()));
            }
        }
        Ok(Self { previous })
    }
}

impl Drop for Foreground {
    fn drop(&mut self) {
        if let Some(previous) = self.previous {
            let _ = set_foreground(previous);
        }
    }
}

fn set_foreground(group: libc::pid_t) -> io::Result<()> {
    // A background process normally receives SIGTTOU for tcsetpgrp. Temporarily
    // block it on this thread only (including restoration from the parent), and
    // restore the exact prior signal mask before executing any editor code.
    unsafe {
        let mut blocked = std::mem::zeroed::<libc::sigset_t>();
        let mut previous = std::mem::zeroed::<libc::sigset_t>();
        libc::sigemptyset(&mut blocked);
        libc::sigaddset(&mut blocked, libc::SIGTTOU);
        let error = libc::pthread_sigmask(libc::SIG_BLOCK, &blocked, &mut previous);
        if error != 0 {
            return Err(io::Error::from_raw_os_error(error));
        }
        let terminal_error = loop {
            if libc::tcsetpgrp(libc::STDIN_FILENO, group) == 0 {
                break None;
            }
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EINTR) {
                break Some(error);
            }
        };
        let mask_error = libc::pthread_sigmask(libc::SIG_SETMASK, &previous, std::ptr::null_mut());
        if let Some(error) = terminal_error {
            return Err(error);
        }
        if mask_error != 0 {
            return Err(io::Error::from_raw_os_error(mask_error));
        }
    }
    Ok(())
}

pub(super) fn kill_group(pid: u32) {
    // Only editor children created with process_group(0) reach this path. Never
    // target the parent's foreground group or pid 0.
    if pid > 0 && pid <= i32::MAX as u32 {
        unsafe {
            libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
        }
    }
}
