"""Start goon with --yes (auto-approve), switch to ask mode, and verify a write asks for approval."""
import errno
import json
import os
import pty
import re
import select
import struct
import subprocess
import sys
import termios
import fcntl
import time

ANSI_RE = re.compile(rb'\x1b\[[0-9;?]*[A-Za-z]')
master, slave = pty.openpty()
fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 30, 100, 0, 0))
def own_terminal():
    os.setsid()
    fcntl.ioctl(slave, termios.TIOCSCTTY, 0)

child = subprocess.Popen(sys.argv[1:], stdin=slave, stdout=slave, stderr=slave,
                         preexec_fn=own_terminal, env={**os.environ, 'TERM': 'xterm-256color'})
output = bytearray()


def pump(timeout=0.1):
    ready, _, _ = select.select([master], [], [], timeout)
    if ready:
        try:
            output.extend(os.read(master, 65536))
        except OSError as error:
            if error.errno != errno.EIO:
                raise


def wait_for(needle, seconds=12):
    deadline = time.monotonic() + seconds
    target = needle.encode()
    while time.monotonic() < deadline:
        if target in ANSI_RE.sub(b'', output):
            return True
        pump()
        if child.poll() is not None:
            break
    return False


asked = False
try:
    if not wait_for('/help commands'):
        raise RuntimeError('TUI did not start\n' + output.decode(errors='replace')[-3000:])
    os.write(master, b'\x1b[200~/mode ask\x1b[201~')
    time.sleep(.15)
    os.write(master, b'\r')
    if not wait_for('Mode switched to: ask'):
        raise RuntimeError('mode switch not acknowledged\n' + output.decode(errors='replace')[-3000:])
    os.write(master, b'\x1b[200~create guarded.txt\x1b[201~')
    time.sleep(.15)
    os.write(master, b'\r')
    asked = wait_for('Allow write_file?')
    if asked:
        os.write(master, b'n')
        wait_for('Permission denied')
    time.sleep(0.2)
    os.write(master, b'\x1b[200~/exit\x1b[201~')
    time.sleep(.15)
    os.write(master, b'\r')
    deadline = time.monotonic() + 8
    while child.poll() is None and time.monotonic() < deadline:
        pump()
    child.wait(timeout=1)
    print(json.dumps({'code': child.returncode, 'asked': asked}))
finally:
    if child.poll() is None:
        child.kill()
        child.wait()
    os.close(master)
    os.close(slave)
