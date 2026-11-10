"""Real job-control supervisor: non-orphaned foreground group, suspend/bg/fg."""
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
info = os.path.join(cwd, 'suspend-child.json')
control = os.path.join(cwd, 'suspend-control')
# The supervisor is a shell analogue in the same session, different process group.
# A setsid Goon by itself is an orphaned group and SIGTSTP is legitimately ignored.
supervisor = r'''
import fcntl,json,os,signal,subprocess,sys,termios,time
binary,cwd,info,control,mode=sys.argv[1:]
os.setsid()
fcntl.ioctl(0,termios.TIOCSCTTY,0)
signal.signal(signal.SIGTTOU,signal.SIG_IGN)
def reset_job_signals():
 for sig in (signal.SIGTTOU,signal.SIGTTIN,signal.SIGTSTP): signal.signal(sig,signal.SIG_DFL)
child=subprocess.Popen([binary,'--cwd',cwd,'--stub']+(['--no-alt-screen'] if mode=='inline' else []),process_group=0,preexec_fn=reset_job_signals)
os.tcsetpgrp(0,child.pid)
with open(info,'w') as out: json.dump({'pid':child.pid},out)
last=''
while True:
 try:
  with open(control) as stream: command=stream.read()
 except FileNotFoundError: command=''
 if command!=last:
  last=command
  if command=='bg':
   os.tcsetpgrp(0,os.getpgrp())
   os.write(1,b'\r\nSHELL-OUTPUT-WHILE-SUSPENDED\r\n')
   os.killpg(child.pid,signal.SIGCONT)
  elif command=='fg':
   os.tcsetpgrp(0,child.pid)
   os.killpg(child.pid,signal.SIGCONT)
  elif command=='terminate':
   os.kill(child.pid,signal.SIGTERM)
   os.killpg(child.pid,signal.SIGCONT)
 if child.poll() is not None: sys.exit(child.returncode)
 time.sleep(.01)
'''
child = subprocess.Popen([sys.executable, '-c', supervisor, binary, cwd, info, control, mode],
                         stdin=slave, stdout=slave, stderr=slave,
                         env={**os.environ, 'TERM': 'xterm-256color'})
output = bytearray()
ansi = re.compile(rb'\x1b\[[0-9;?]*[A-Za-z]')
pid = None

def pump():
    if select.select([master], [], [], .05)[0]:
        try: output.extend(os.read(master, 65536))
        except OSError as error:
            if error.errno != errno.EIO: raise

def wait_for(predicate, description):
    deadline = time.monotonic() + 12
    while time.monotonic() < deadline:
        pump()
        if predicate(): return
        if child.poll() is not None: break
    raise RuntimeError(description + '\n' + output.decode(errors='replace')[-4000:])

def state():
    with open(f'/proc/{pid}/stat', encoding='utf-8') as stream:
        return stream.read().split()[2]

def command(value):
    with open(control + '.tmp', 'w') as stream: stream.write(value)
    os.replace(control + '.tmp', control)

try:
    wait_for(lambda: os.path.exists(info), 'supervisor startup')
    with open(info, encoding='utf-8') as stream: pid = json.load(stream)['pid']
    wait_for(lambda: b'/help commands' in ansi.sub(b'', output), 'TUI startup')
    os.write(master, b'\x1b[200~suspend-preserved-draft\x1b[201~')
    wait_for(lambda: b'suspend-preserved-draft' in ansi.sub(b'', output), 'draft input')
    os.write(master, b'\x1a')
    wait_for(lambda: state() == 'T', 'Ctrl+Z did not suspend foreground job')
    assert termios.tcgetattr(slave)[3] & termios.ICANON, 'suspended tty is not cooked'
    time.sleep(1.2)  # Time stopped cannot consume a resume-handler timeout.
    if mode == 'terminate':
        command('terminate')
    else:
        command('bg')
        wait_for(lambda: b'SHELL-OUTPUT-WHILE-SUSPENDED' in output, 'shell output missing')
        time.sleep(.2)
        assert termios.tcgetattr(slave)[3] & termios.ICANON, 'bg must not steal tty raw mode'
        output.clear()
        command('fg')
        wait_for(lambda: b'suspend-preserved-draft' in ansi.sub(b'', output), 'draft not redrawn after fg')
        assert not termios.tcgetattr(slave)[3] & termios.ICANON, 'fg must restore raw mode'
        assert os.tcgetpgrp(master) == pid
        if mode == 'inline':
            assert b'\x1b[?1049' not in output, 'inline resume entered alternate screen'
        os.write(master, b'\x15')
        os.write(master, b'\x1b[200~/exit\x1b[201~')
        time.sleep(.15)
        os.write(master, b'\r')
    wait_for(lambda: child.poll() is not None, 'job did not exit')
    assert termios.tcgetattr(slave)[3] & termios.ICANON, 'exit did not restore cooked tty'
    print(json.dumps({'code': child.returncode, 'suspended': True, 'canonical': True}))
finally:
    if pid:
        try: os.killpg(pid, signal.SIGKILL)
        except ProcessLookupError: pass
    if child.poll() is None:
        child.kill()
        child.wait()
    os.close(master)
    os.close(slave)
