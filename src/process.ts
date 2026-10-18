import { spawn } from 'node:child_process';
import { existsSync } from 'node:fs';
import { join } from 'node:path';

export interface CommandResult { code: number; output: string; truncated: boolean }
const OUTPUT_LIMIT = 64 * 1024;
const HARD_OUTPUT_LIMIT = 4 * 1024 * 1024;

export function resolveExecutable(command: string): string {
  if (command !== 'bash' || process.platform !== 'win32') return command;
  const candidates = [
    process.env.GOON_BASH,
    'C:\\Program Files\\Git\\bin\\bash.exe',
    'C:\\Program Files (x86)\\Git\\bin\\bash.exe',
    process.env.LOCALAPPDATA ? join(process.env.LOCALAPPDATA, 'Programs', 'Git', 'bin', 'bash.exe') : undefined,
  ];
  for (const candidate of candidates) {
    if (candidate && existsSync(candidate)) return candidate;
  }
  return command;
}

export async function runCommand(command: string, args: string[], options: {
  cwd: string; signal: AbortSignal; timeoutMs: number;
}): Promise<CommandResult> {
  options.signal.throwIfAborted();
  const resolvedCommand = resolveExecutable(command);
  return new Promise((resolve, reject) => {
    const child = spawn(resolvedCommand, args, {
      cwd: options.cwd, env: process.env, stdio: ['ignore', 'pipe', 'pipe'],
      detached: process.platform !== 'win32', windowsHide: true,
    });
    let first = Buffer.alloc(0), tail = Buffer.alloc(0), bytes = 0;
    let terminalError: Error | undefined;
    let killTimer: ReturnType<typeof setTimeout> | undefined;
    const kill = (signal: NodeJS.Signals) => {
      if (!child.pid) return;
      if (process.platform === 'win32') {
        const taskkill = spawn('taskkill', ['/pid', String(child.pid), '/T', '/F'], { stdio: 'ignore', windowsHide: true });
        taskkill.on('error', () => child.kill(signal));
        child.kill(signal);
        child.stdout.destroy();
        child.stderr.destroy();
      } else {
        try { process.kill(-child.pid, signal); } catch (error) {
          if ((error as NodeJS.ErrnoException).code !== 'ESRCH') child.kill(signal);
        }
      }
    };
    const stop = (error: Error) => {
      if (terminalError) return;
      terminalError = error;
      kill('SIGTERM');
      killTimer = setTimeout(() => kill('SIGKILL'), 500);
      killTimer.unref();
    };
    const abort = () => stop(new Error('Command cancelled.'));
    const timer = setTimeout(() => stop(new Error(`Command timed out after ${options.timeoutMs} ms.`)), options.timeoutMs);
    options.signal.addEventListener('abort', abort, { once: true });
    // Close the spawn/listener race: a cancellation between these operations
    // must still terminate the process group.
    if (options.signal.aborted) abort();
    const capture = (chunk: Buffer) => {
      bytes += chunk.length;
      const remaining = OUTPUT_LIMIT / 2 - first.length;
      if (remaining > 0) first = Buffer.concat([first, chunk.subarray(0, remaining)]);
      tail = Buffer.concat([tail, chunk]).subarray(-OUTPUT_LIMIT / 2);
      if (bytes > HARD_OUTPUT_LIMIT) stop(new Error('Command exceeded the 4 MiB output limit.'));
    };
    child.stdout.on('data', capture);
    child.stderr.on('data', capture);
    const cleanup = () => {
      clearTimeout(timer);
      if (killTimer) { clearTimeout(killTimer); kill('SIGKILL'); }
      options.signal.removeEventListener('abort', abort);
    };
    child.once('error', error => { cleanup(); reject(error); });
    child.once('close', code => {
      cleanup();
      if (terminalError) { reject(terminalError); return; }
      // Avoid duplicating the middle bytes when output fits in the budget.
      const output = bytes <= OUTPUT_LIMIT / 2 ? first.toString('utf8') :
        bytes <= OUTPUT_LIMIT ? Buffer.concat([first, tail.subarray(tail.length - (bytes - first.length))]).toString('utf8') :
          `${first.toString('utf8')}\n...[output truncated]...\n${tail.toString('utf8')}`;
      resolve({ code: code ?? 1, output, truncated: bytes > OUTPUT_LIMIT });
    });
  });
}
