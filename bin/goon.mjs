#!/usr/bin/env node
// npm entry point: runs the native goon binary for this platform and relays its exit status.
// The binary is resolved from GOON_BINARY, the package's vendor/ directory (filled at install
// time), a local cargo build (source checkouts), or downloaded once from the matching GitHub
// release and verified against the SHA-256 recorded in vendor/manifest.json.
import { spawn } from 'node:child_process';
import { createServer } from 'node:net';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const { ensureBinary } = await import('../scripts/install.mjs');

let binary;
try {
  binary = await ensureBinary(root);
} catch (error) {
  process.stderr.write(`goon: ${error instanceof Error ? error.message : String(error)}\n`);
  process.exit(1);
}

const isWindows = process.platform === 'win32';
const env = { ...process.env, GOON_WRAPPER_NODE: process.version };

// Windows has no SIGTERM and Ctrl+C reaches the whole console, so relayed signals (IPC from a
// parent, e.g. the test harness) travel over a loopback control socket the binary connects to.
let control;
let controlSocket;
const pendingSignals = [];
if (isWindows) {
  control = createServer(socket => {
    controlSocket = socket;
    socket.on('error', () => {});
    for (const sig of pendingSignals.splice(0)) socket.write(`${sig}\n`);
  });
  control.unref();
  await new Promise(resolve => control.listen(0, '127.0.0.1', resolve));
  env.GOON_CONTROL_PORT = String(control.address().port);
}

const child = spawn(binary, process.argv.slice(2), { stdio: 'inherit', env, windowsHide: false });

const relay = signal => {
  if (child.exitCode !== null || child.signalCode !== null) return;
  if (!isWindows) {
    try { child.kill(signal); } catch { /* already gone */ }
    return;
  }
  if (controlSocket) controlSocket.write(`${signal}\n`);
  else pendingSignals.push(signal);
};

// A terminal Ctrl+C already reaches the binary through the process group; relaying it again
// would count as a second interrupt. Only forward SIGINT when it cannot have come from a TTY.
process.on('SIGINT', () => { if (!process.stdin.isTTY || isWindows) relay('SIGINT'); });
process.on('SIGTERM', () => relay('SIGTERM'));
if (!isWindows) process.on('SIGHUP', () => relay('SIGHUP'));
process.on('message', message => {
  if (message && typeof message === 'object' && message.type === 'goon:signal' && (message.signal === 'SIGINT' || message.signal === 'SIGTERM')) {
    if (isWindows) {
      if (controlSocket) controlSocket.write(`${message.signal}\n`);
      else pendingSignals.push(message.signal);
    } else relay(message.signal);
  }
});

child.on('error', error => {
  process.stderr.write(`goon: cannot start ${binary}: ${error.message}\n`);
  process.exit(1);
});
child.on('exit', (code, signal) => {
  control?.close();
  if (process.connected) {
    try { process.disconnect(); } catch { /* ignore */ }
  }
  if (signal && !isWindows) {
    // Re-raise so the parent observes the same termination signal.
    process.removeAllListeners(signal);
    process.kill(process.pid, signal);
    return;
  }
  process.exit(code ?? 1);
});

