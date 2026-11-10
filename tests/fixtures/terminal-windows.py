"""Exercise the real Windows TUI through ConPTY using only Python's standard library."""
import ctypes
from ctypes import wintypes
import json
import os
import re
import subprocess
import sys
import time

if os.name != "nt":
    raise RuntimeError("ConPTY fixture requires Windows")

kernel = ctypes.WinDLL("kernel32", use_last_error=True)
HANDLE = wintypes.HANDLE
SIZE_T = ctypes.c_size_t


class Coord(ctypes.Structure):
    _fields_ = [("X", ctypes.c_short), ("Y", ctypes.c_short)]


class StartupInfo(ctypes.Structure):
    _fields_ = [
        ("cb", wintypes.DWORD), ("lpReserved", wintypes.LPWSTR),
        ("lpDesktop", wintypes.LPWSTR), ("lpTitle", wintypes.LPWSTR),
        ("dwX", wintypes.DWORD), ("dwY", wintypes.DWORD),
        ("dwXSize", wintypes.DWORD), ("dwYSize", wintypes.DWORD),
        ("dwXCountChars", wintypes.DWORD), ("dwYCountChars", wintypes.DWORD),
        ("dwFillAttribute", wintypes.DWORD), ("dwFlags", wintypes.DWORD),
        ("wShowWindow", wintypes.WORD), ("cbReserved2", wintypes.WORD),
        ("lpReserved2", ctypes.POINTER(wintypes.BYTE)),
        ("hStdInput", HANDLE), ("hStdOutput", HANDLE), ("hStdError", HANDLE),
    ]


class StartupInfoEx(ctypes.Structure):
    _fields_ = [("StartupInfo", StartupInfo), ("lpAttributeList", ctypes.c_void_p)]


class ProcessInformation(ctypes.Structure):
    _fields_ = [("hProcess", HANDLE), ("hThread", HANDLE),
                ("dwProcessId", wintypes.DWORD), ("dwThreadId", wintypes.DWORD)]


def api(name, args, result=wintypes.BOOL):
    fn = getattr(kernel, name)
    fn.argtypes = args
    fn.restype = result
    return fn


create_pipe = api("CreatePipe", [ctypes.POINTER(HANDLE), ctypes.POINTER(HANDLE), ctypes.c_void_p, wintypes.DWORD])
create_console = api("CreatePseudoConsole", [Coord, HANDLE, HANDLE, wintypes.DWORD, ctypes.POINTER(HANDLE)], ctypes.c_long)
resize_console = api("ResizePseudoConsole", [HANDLE, Coord], ctypes.c_long)
close_console = api("ClosePseudoConsole", [HANDLE], None)
close_handle = api("CloseHandle", [HANDLE])
initialize = api("InitializeProcThreadAttributeList", [ctypes.c_void_p, wintypes.DWORD, wintypes.DWORD, ctypes.POINTER(SIZE_T)])
update = api("UpdateProcThreadAttribute", [ctypes.c_void_p, wintypes.DWORD, SIZE_T, ctypes.c_void_p, SIZE_T, ctypes.c_void_p, ctypes.c_void_p])
delete = api("DeleteProcThreadAttributeList", [ctypes.c_void_p], None)
create_process = api("CreateProcessW", [wintypes.LPCWSTR, wintypes.LPWSTR, ctypes.c_void_p, ctypes.c_void_p, wintypes.BOOL, wintypes.DWORD, ctypes.c_void_p, wintypes.LPCWSTR, ctypes.POINTER(StartupInfoEx), ctypes.POINTER(ProcessInformation)])
peek = api("PeekNamedPipe", [HANDLE, ctypes.c_void_p, wintypes.DWORD, ctypes.c_void_p, ctypes.POINTER(wintypes.DWORD), ctypes.c_void_p])
read = api("ReadFile", [HANDLE, ctypes.c_void_p, wintypes.DWORD, ctypes.POINTER(wintypes.DWORD), ctypes.c_void_p])
write = api("WriteFile", [HANDLE, ctypes.c_void_p, wintypes.DWORD, ctypes.POINTER(wintypes.DWORD), ctypes.c_void_p])
exit_code = api("GetExitCodeProcess", [HANDLE, ctypes.POINTER(wintypes.DWORD)])
terminate = api("TerminateProcess", [HANDLE, wintypes.UINT])
wait = api("WaitForSingleObject", [HANDLE, wintypes.DWORD], wintypes.DWORD)


def checked(result):
    if not result:
        raise ctypes.WinError(ctypes.get_last_error())
    return result


input_read, input_write, output_read, output_write, console = (HANDLE() for _ in range(5))
process = ProcessInformation()
attributes = None
output = bytearray()
ansi = re.compile(rb"\x1b(?:\[[0-?]*[ -/]*[@-~]|\][^\x07]*(?:\x07|\x1b\\))")


def pump():
    available = wintypes.DWORD()
    if not peek(output_read, None, 0, None, ctypes.byref(available), None):
        return
    if available.value:
        buffer = ctypes.create_string_buffer(min(available.value, 65536))
        count = wintypes.DWORD()
        checked(read(output_read, buffer, len(buffer), ctypes.byref(count), None))
        output.extend(buffer.raw[:count.value])


def send(text):
    data = text.encode("utf-8")
    count = wintypes.DWORD()
    checked(write(input_write, data, len(data), ctypes.byref(count), None))
    if count.value != len(data):
        raise RuntimeError("short ConPTY input write")


def status():
    code = wintypes.DWORD()
    checked(exit_code(process.hProcess, ctypes.byref(code)))
    return code.value


def wait_for(text):
    deadline = time.monotonic() + 15
    needle = text.encode("utf-8")
    while time.monotonic() < deadline:
        pump()
        if needle in output or needle in ansi.sub(b"", output):
            return
        if status() != 259:
            break
        time.sleep(0.02)
    raise RuntimeError("TUI did not reach " + text + "\n" + output.decode(errors="replace")[-5000:])


try:
    checked(create_pipe(ctypes.byref(input_read), ctypes.byref(input_write), None, 0))
    checked(create_pipe(ctypes.byref(output_read), ctypes.byref(output_write), None, 0))
    result = create_console(Coord(100, 30), input_read, output_write, 0, ctypes.byref(console))
    if result < 0:
        raise RuntimeError("CreatePseudoConsole failed: " + hex(result & 0xffffffff))
    size = SIZE_T()
    initialize(None, 1, 0, ctypes.byref(size))
    attributes = ctypes.create_string_buffer(size.value)
    checked(initialize(attributes, 1, 0, ctypes.byref(size)))
    checked(update(attributes, 0, 0x00020016, console, ctypes.sizeof(HANDLE), None, None))
    startup = StartupInfoEx()
    startup.StartupInfo.cb = ctypes.sizeof(startup)
    startup.lpAttributeList = ctypes.cast(attributes, ctypes.c_void_p)
    # Explicit console handles prevent inheriting the Node test runner's redirected pipes.
    # Exercise the native executable; wrapper signal propagation has separate contract tests.
    native = os.environ.get("GOON_BINARY", os.path.abspath(os.path.join(os.path.dirname(__file__), "../../target/debug/goon.exe")))
    bootstrap = "import subprocess,sys; i=open('CONIN$','r'); o=open('CONOUT$','w'); sys.exit(subprocess.call(sys.argv[1:],stdin=i,stdout=o,stderr=o))"
    command = ctypes.create_unicode_buffer(subprocess.list2cmdline([sys.executable, "-c", bootstrap, native, *sys.argv[3:]]))
    checked(create_process(None, command, None, None, False, 0x00080000, None, None, ctypes.byref(startup), ctypes.byref(process)))
    close_handle(input_read)
    input_read = HANDLE()
    close_handle(output_write)
    output_write = HANDLE()
    wait_for("/help commands")
    send("\x1b[200~editor seed one\neditor seed two\neditor seed three\x1b[201~")
    time.sleep(0.15)
    send("\x07")  # Ctrl+G hands the terminal to VISUAL, then restores the unsent draft.
    wait_for("editor-restored-marker")
    send("\x15")
    time.sleep(0.1)
    # Bracketed paste avoids classifying synthetic command bytes as a rapid unbracketed paste.
    send("\x1b[200~/status\x1b[201~")
    time.sleep(0.15)
    send("\r")
    wait_for("Session tokens 0")
    send("\x1b[200~/model\x1b[201~")
    time.sleep(0.15)
    send("\r")
    wait_for("Select model")
    send("\r")
    wait_for("Reasoning effort")
    # Escape returns to model selection; Escape there cancels without applying either stage.
    send("\x1b")
    time.sleep(0.1)
    send("\x1b")
    time.sleep(0.1)
    send("\x1b[200~/status\x1b[201~")
    time.sleep(0.15)
    send("\r")
    wait_for("Session tokens 0")
    time.sleep(0.25)
    checked(resize_console(console, Coord(80, 24)) >= 0)
    # Keep a non-empty composer/caret alive across repeated width and height changes.
    # Matching only old output would miss a stalled resize query or a lost draft.
    for columns, rows in [(46, 14), (110, 32), (80, 24)]:
        checked(resize_console(console, Coord(columns, rows)) >= 0)
        before = len(output)
        send("\x1b[200~resize-draft-marker\x1b[201~")
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            pump()
            if b"resize-draft-marker" in ansi.sub(b"", output[before:]):
                break
            time.sleep(0.02)
        else:
            raise RuntimeError("draft not repainted after resize")
        send("\x15")  # Ctrl+U clears only the editable draft.
        time.sleep(0.1)
    send("\x1b[200~create ui.txt\x1b[201~")
    time.sleep(0.15)
    send("\r")
    wait_for("Question 1/2")
    send("2")
    wait_for("Question 2/2")
    send("\x1b[D")
    time.sleep(0.1)
    send("\r")
    time.sleep(0.1)
    send("\r")
    wait_for("Allow write_file?")
    send("y")
    wait_for("Created by TUI.")
    wait_for("Worked for ")
    send("\x1b[200~/exit\x1b[201~")
    time.sleep(0.15)
    send("\r")
    deadline = time.monotonic() + 15
    while status() == 259 and time.monotonic() < deadline:
        pump()
        time.sleep(0.02)
    if status() == 259:
        raise RuntimeError("TUI did not exit")
    pump()
    inline = "--no-alt-screen" in sys.argv
    print(json.dumps({"code": status(), "inline": inline,
                      "entered": b"\x1b[?1049h" in output,
                      "left": b"\x1b[?1049l" in output,
                      "paste_disabled": b"\x1b[?2004l" in output}))
finally:
    if process.hProcess:
        if status() == 259:
            # The wrapper may have a native child. Terminate the entire test-owned tree only.
            subprocess.run(["taskkill", "/PID", str(process.dwProcessId), "/T", "/F"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False)
            terminate(process.hProcess, 1)
        wait(process.hProcess, 2000)
    # The output pipe is drained while ConPTY shuts down; ClosePseudoConsole can block otherwise.
    if console:
        import threading
        closer = threading.Thread(target=close_console, args=(console,), daemon=True)
        closer.start()
        while closer.is_alive():
            pump()
            closer.join(0.02)
    if attributes is not None:
        delete(attributes)
    for handle in [process.hThread, process.hProcess, input_read, input_write, output_read, output_write]:
        if handle:
            close_handle(handle)
