//! Windows console input-mode guard ported from OpenAI Codex (`tui/windows_console.rs`).
//!
//! `crossterm`'s Windows backend reads Win32 `INPUT_RECORD` events (`ReadConsoleInputW`).
//! If `ENABLE_VIRTUAL_TERMINAL_INPUT` (`0x0200`) is inherited from a parent shell or re-enabled
//! by a child process, ConPTY emits raw VT escape sequences instead of structured key records.

use std::io::Error;
use std::io::Result;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::Ordering;
use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::System::Console::GetConsoleMode;
use windows_sys::Win32::System::Console::GetStdHandle;
use windows_sys::Win32::System::Console::SetConsoleMode;
use windows_sys::Win32::System::Console::STD_INPUT_HANDLE;

const ENABLE_VIRTUAL_TERMINAL_INPUT: u32 = 0x0200;

const ORIGINAL_VT_INPUT_UNKNOWN: u8 = 0;
const ORIGINAL_VT_INPUT_DISABLED: u8 = 1;
const ORIGINAL_VT_INPUT_ENABLED: u8 = 2;

static ORIGINAL_VT_INPUT_MODE: AtomicU8 = AtomicU8::new(ORIGINAL_VT_INPUT_UNKNOWN);

fn record_original_vt_input_mode(mode: u32) {
    let state = if mode & ENABLE_VIRTUAL_TERMINAL_INPUT != 0 { ORIGINAL_VT_INPUT_ENABLED } else { ORIGINAL_VT_INPUT_DISABLED };
    let _ = ORIGINAL_VT_INPUT_MODE.compare_exchange(ORIGINAL_VT_INPUT_UNKNOWN, state, Ordering::AcqRel, Ordering::Acquire);
}

fn stdin_handle() -> Result<*mut core::ffi::c_void> {
    // SAFETY: `GetStdHandle` reads the process standard input handle without dereferencing pointers.
    let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return Err(Error::last_os_error());
    }
    Ok(handle)
}

fn console_mode(handle: *mut core::ffi::c_void) -> Result<u32> {
    let mut mode = 0u32;
    // SAFETY: `handle` was validated by `stdin_handle`, and `mode` is a valid stack `u32`.
    if unsafe { GetConsoleMode(handle, &mut mode) } == 0 {
        return Err(Error::last_os_error());
    }
    Ok(mode)
}

fn set_console_mode(handle: *mut core::ffi::c_void, mode: u32) -> Result<()> {
    // SAFETY: `handle` was validated by `stdin_handle`, and `mode` only toggles documented flags.
    if unsafe { SetConsoleMode(handle, mode) } == 0 {
        return Err(Error::last_os_error());
    }
    Ok(())
}

pub(super) fn set_input_record_mode() -> Result<()> {
    let handle = stdin_handle()?;
    let mode = console_mode(handle)?;
    record_original_vt_input_mode(mode);
    set_console_mode(handle, mode & !ENABLE_VIRTUAL_TERMINAL_INPUT)
}

pub(super) fn ensure_input_record_mode() -> Result<()> {
    let handle = stdin_handle()?;
    let mode = console_mode(handle)?;
    record_original_vt_input_mode(mode);
    if mode & ENABLE_VIRTUAL_TERMINAL_INPUT != 0 {
        set_console_mode(handle, mode & !ENABLE_VIRTUAL_TERMINAL_INPUT)?;
    }
    Ok(())
}

pub(super) fn restore_input_mode() -> Result<()> {
    let original = ORIGINAL_VT_INPUT_MODE.swap(ORIGINAL_VT_INPUT_UNKNOWN, Ordering::AcqRel);
    if original == ORIGINAL_VT_INPUT_UNKNOWN {
        return Ok(());
    }
    let handle = stdin_handle()?;
    let mode = console_mode(handle)?;
    let restored = match original {
        ORIGINAL_VT_INPUT_ENABLED => mode | ENABLE_VIRTUAL_TERMINAL_INPUT,
        ORIGINAL_VT_INPUT_DISABLED => mode & !ENABLE_VIRTUAL_TERMINAL_INPUT,
        _ => mode,
    };
    if restored != mode {
        set_console_mode(handle, restored)?;
    }
    Ok(())
}
