"""Exercise the installed terminal APIs through a real POSIX pseudo-terminal."""
import errno
import fcntl
import json
import os
import pty
import re
import select
import struct
import subprocess
import sys
import termios
import time

ANSI_RE = re.compile(rb'\x1b\[[0-9;?]*[A-Za-z]')
master, slave = pty.openpty()
fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 24, 72, 0, 0))
child = subprocess.Popen(sys.argv[1:], stdin=slave, stdout=slave, stderr=slave, env={**os.environ, 'TERM': 'xterm-256color'})
output = bytearray()

def wait_for(needle):
    deadline = time.monotonic() + 12
    target = needle.encode()
    while time.monotonic() < deadline:
        if target in output or target in ANSI_RE.sub(b'', output):
            return
        ready, _, _ = select.select([master], [], [], 0.1)
        if ready:
            data = os.read(master, 65536)
            output.extend(data)
        if child.poll() is not None:
            break
    raise RuntimeError('TUI did not reach ' + needle + '\n' + output.decode(errors='replace')[-4000:])

try:
    wait_for('/help commands')
    os.write(master, b'/status\r')
    wait_for('Session tokens 0')
    # Resize the actual terminal while the UI is live.
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 18, 48, 0, 0))
    child.send_signal(__import__('signal').SIGWINCH)
    os.write(master, b'create ui.txt\r')
    wait_for('Allow write_file?')
    os.write(master, b'y\r')
    wait_for('Created by TUI.')
    time.sleep(0.1)
    os.write(master, b'/exit\r')
    child.wait(timeout=5)
    while select.select([master], [], [], 0)[0]:
        try:
            data = os.read(master, 65536)
        except OSError as error:
            if error.errno == errno.EIO:
                break
            raise
        if not data:
            break
        output.extend(data)
    raw_restored = bool(termios.tcgetattr(slave)[3] & termios.ICANON)
    print(json.dumps({'code': child.returncode, 'entered': b'\x1b[?1049h' in output,
                      'restored': output.endswith(b'\x1b[?25h\x1b[?1049l'), 'canonical': raw_restored}))
finally:
    if child.poll() is None:
        child.kill()
        child.wait()
    os.close(master)
    os.close(slave)
