/**
 * End-to-end helpers: run the real CLI (`bin/goon.mjs`) as a user would, in a throwaway
 * workspace, against the deterministic stub provider (no network, no API key).
 *
 * Conventions:
 * - Every scenario gets a fresh `fs.mkdtemp` root with a `workspace/` subdirectory and isolated
 *   `XDG_CONFIG_HOME` / `XDG_DATA_HOME`, removed again in `t.after`.
 * - The child is spawned with `cwd: workspace` and an absolute path to `bin/goon.mjs`.
 * - Assertions never touch volatile fields (session ids, timestamps, durations).
 */
import { spawn } from 'node:child_process';
import { mkdir, mkdtemp, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

export const REPO_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..');
export const BIN = join(REPO_ROOT, 'bin', 'goon.mjs');

/** Flags every scenario passes so runs stay deterministic and offline. */
export const BASE_FLAGS = ['--stub', '--no-mcp', '--no-subagents'];

// Inherited env vars that could change behaviour between machines; scenarios opt back in via `env`.
const STRIPPED_ENV = [
  'GOON_API_KEY', 'GOON_BASE_URL', 'GOON_MODEL', 'GOON_STUB', 'GOON_STUB_FIXTURE',
  'GOON_EVENT_LOG', 'GOON_PRICING_JSON', 'GOON_VERIFY_CMDS', 'GOON_MAX_COST_USD',
  'GOON_MAX_TIME_MS', 'GOON_ALLOW_NET', 'GOON_ALLOW_NET_HOSTS', 'GOON_TEST_BIN',
];

export async function e2eFixture(t) {
  const root = await mkdtemp(join(tmpdir(), 'goon-e2e-'));
  const workspace = join(root, 'workspace');
  await mkdir(workspace);
  t.after(() => rm(root, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 }));
  return { root, workspace, config: join(root, 'config'), data: join(root, 'data') };
}

/**
 * Spawns the CLI exactly like a user would and resolves `{ code, stdout, stderr }` on exit.
 * A watchdog kills the child so a hung scenario fails instead of stalling the suite.
 */
export function runGoon(t, f, args, options = {}) {
  const env = { ...process.env, XDG_CONFIG_HOME: f.config, XDG_DATA_HOME: f.data };
  for (const key of STRIPPED_ENV) delete env[key];
  Object.assign(env, options.env ?? {});
  const child = spawn(process.execPath, [BIN, ...args], {
    cwd: f.workspace,
    env,
    stdio: ['pipe', 'pipe', 'pipe'],
  });
  let stdout = '';
  let stderr = '';
  child.stdout.on('data', chunk => { stdout += chunk; });
  child.stderr.on('data', chunk => { stderr += chunk; });
  const watchdog = setTimeout(() => child.kill('SIGKILL'), 120_000);
  const done = new Promise((resolveRun, reject) => {
    child.once('error', reject);
    child.once('close', (code, signal) => {
      clearTimeout(watchdog);
      resolveRun({ code, signal, stdout, stderr });
    });
  });
  child.stdin.end(options.input ?? '');
  return done;
}

/** Writes a stub fixture (src/stub-fixtures.ts schema) into the scenario root. */
export async function writeFixture(f, name, fixture) {
  const path = join(f.root, name);
  await writeFile(path, `${JSON.stringify(fixture, null, 2)}\n`, 'utf8');
  return path;
}

export async function writeWorkspaceFile(f, name, content) {
  const path = join(f.workspace, name);
  await writeFile(path, content, 'utf8');
  return path;
}

/** Parses one JSON event per line (`--json` stdout or a `.events.jsonl` log). */
export function jsonLines(text) {
  return text.split('\n').filter(Boolean).map(line => JSON.parse(line));
}

export function sessionsDir(f) {
  return join(f.data, 'goon', 'sessions');
}

/** Event-type sequence with per-call `context_budget` noise removed. */
export function eventTypes(events) {
  return events.filter(event => event.type !== 'context_budget').map(event => event.type);
}
