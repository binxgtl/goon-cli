"""Real controlling-PTY editor job-control regressions, no third-party modules."""
import errno
import fcntl
import json
import os
import pty
import re
import select
import signal
import struct
import subprocess
import sys
import termios
import time

binary, cwd, mode = sys.argv[1:]
master, slave = pty.openpty()
fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 24, 100, 0, 0))
editor = os.path.join(cwd, 'editor.py')
info = os.path.join(cwd, 'editor-info.json')
# The editor records its foreground group and exercises an actual read immediately.
with open(editor, 'w', encoding='utf-8') as stream:
    stream.write('''import os,sys,json,signal,time,subprocess
path=sys.argv[1]
child=None
if os.environ['EDITOR_TEST_MODE']=='terminate':
 child=subprocess.Popen([sys.executable,'-c','import time;time.sleep(120)'])
with open(os.environ['EDITOR_TEST_INFO'],'w') as out:
 json.dump({'pid':os.getpid(),'pgid':os.getpgrp(),'foreground':os.tcgetpgrp(0),'grandchild':child.pid if child else None},out)
if child:
 while True: time.sleep(.1)
else:
 value=os.read(0,1)
 with open(path,'w') as out: out.write('editor-read-'+value.decode()+'\\n')
''')

def own_terminal():
    os.setsid()
    fcntl.ioctl(slave, termios.TIOCSCTTY, 0)

env = {**os.environ, 'TERM': 'xterm-256color', 'VISUAL': f'"{sys.executable}" "{editor}"',
       'EDITOR_TEST_MODE': mode, 'EDITOR_TEST_INFO': info}
child = subprocess.Popen([binary, '--cwd', cwd, '--stub'],
                         stdin=slave, stdout=slave, stderr=slave, preexec_fn=own_terminal, env=env)
output = bytearray()
ansi = re.compile(rb'\x1b\[[0-9;?]*[A-Za-z]')

def pump():
    if select.select([master], [], [], .05)[0]:
        try:
            output.extend(os.read(master, 65536))
        except OSError as error:
            if error.errno != errno.EIO: raise

def wait_for(predicate, description):
    deadline = time.monotonic() + 12
    while time.monotonic() < deadline:
        pump()
        if predicate(): return
        if child.poll() is not None: break
    raise RuntimeError(description + '\n' + output.decode(errors='replace')[-4000:])

def alive(pid):
    # A killed orphan may briefly be a zombie until init reaps it. It cannot read tty.
    if sys.platform.startswith('linux'):
        try:
            with open(f'/proc/{pid}/stat', encoding='utf-8') as stat:
                if stat.read().split()[2] == 'Z': return False
        except FileNotFoundError:
            return False
        except OSError:
            pass
    else:
        status = subprocess.run(['ps', '-o', 'stat=', '-p', str(pid)], capture_output=True, text=True)
        if status.returncode != 0 or status.stdout.strip().startswith('Z'): return False
    try: os.kill(pid, 0); return True
    except ProcessLookupError: return False

state = None
try:
    wait_for(lambda: b'/help commands' in ansi.sub(b'', output), 'startup')
    os.write(master, b'\x1b[200~seed draft\x1b[201~')
    time.sleep(.15)
    os.write(master, b'\x07')
    wait_for(lambda: os.path.exists(info), 'editor did not launch')
    with open(info, encoding='utf-8') as stream: state = json.load(stream)
    assert state['pid'] == state['pgid'] == state['foreground'], state
    assert state['pgid'] != child.pid, state
    if mode == 'terminate':
        child.send_signal(signal.SIGTERM)
    else:
        os.write(master, b'X')
        wait_for(lambda: b'editor-read-X' in ansi.sub(b'', output), 'editor could not read foreground tty')
        assert os.tcgetpgrp(master) == child.pid, 'foreground group was not restored'
        os.write(master, b'\x15')
        os.write(master, b'\x1b[200~/exit\x1b[201~')
        time.sleep(.15)
        os.write(master, b'\r')
    wait_for(lambda: child.poll() is not None, 'CLI did not exit')
    if state['grandchild']:
        wait_for(lambda: not alive(state['grandchild']), 'editor descendant survived termination')
    assert termios.tcgetattr(slave)[3] & termios.ICANON, 'terminal not restored'
    print(json.dumps({'code': child.returncode, 'foreground_restored': True,
                      'tree_stopped': not state['grandchild'] or not alive(state['grandchild'])}))
finally:
    if child.poll() is None:
        child.kill()
        child.wait()
    if state:
        try: os.killpg(state['pgid'], signal.SIGKILL)
        except ProcessLookupError: pass
    os.close(master)
    os.close(slave)
