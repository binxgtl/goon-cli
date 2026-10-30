// Regression tests for the security/bug audit (see docs/AUDIT.md).
import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdir, readFile, writeFile, stat } from 'node:fs/promises';
import { join, resolve } from 'node:path';
import { fixture, provider, sse, cli } from './helpers.mjs';

// 1x1 transparent PNG
const PNG = Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkYAAAAAYAAjCB0C8AAAAASUVORK5CYII=', 'base64');

test('a real image attachment is sent once as base64 and does not trigger compaction', async t => {
  const f = await fixture(t);
  // 300 KB image: as a numeric JSON array it used to count as ~500k tokens.
  const image = Buffer.concat([PNG, Buffer.alloc(300_000, 1)]);
  await writeFile(join(f.cwd, 'shot.png'), image);
  const p = await provider(t, (body, response) => {
    const content = body.messages.find(m => m.role === 'user').content;
    assert(Array.isArray(content));
    const part = content.find(c => c.type === 'image_url');
    assert(part.image_url.url.startsWith('data:image/png;base64,'));
    sse(response, { text: 'An image.' });
  });
  const result = await cli(f, p.url, ['--json', '-p', 'describe @shot.png'], { env: {} }).done;
  assert.equal(result.code, 0, result.stderr + result.stdout);
  assert.equal(p.requests.length, 1, 'no compaction request');
  const events = result.stdout.trim().split('\n').map(line => JSON.parse(line));
  const id = events[0].id;
  const raw = await readFile(join(f.data, 'goon/sessions', `${id}.json`), 'utf8');
  assert(raw.length < image.length * 1.5, `session file is ${raw.length} bytes`);
  const resumed = await cli(f, p.url, ['--json', '--resume', id, '-p', 'again']).done;
  assert.equal(resumed.code, 0, resumed.stderr);
});

test('real TUI: approvals come back after switching from --yes (auto) to ask mode', async t => {
  if (process.platform === 'win32') { t.skip('POSIX pseudo-terminal test'); return; }
  const { spawn } = await import('node:child_process');
  const f = await fixture(t);
  const p = await provider(t, (_, response, n) => n === 1 ? sse(response, {
    calls: [{ id: 'w1', name: 'write_file', input: { path: 'guarded.txt', content: 'should need approval' } }],
  }) : sse(response, { text: 'Finished.' }));
  const child = spawn('python3', [resolve('tests/fixtures/terminal-mode-switch.py'), process.execPath, resolve(process.env.GOON_TEST_BIN ?? 'bin/goon.mjs'),
    '--cwd', f.cwd, '--model', 'test-model', '--base-url', p.url, '--yes'], {
    env: { ...process.env, GOON_API_KEY: 'tui-test-key', XDG_CONFIG_HOME: f.config, XDG_DATA_HOME: f.data, GOON_NOTIFY: '0' },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  t.after(() => child.kill('SIGKILL'));
  let stdout = '', stderr = '';
  child.stdout.on('data', c => { stdout += c; }); child.stderr.on('data', c => { stderr += c; });
  const code = await new Promise((resolve, reject) => { child.on('error', reject); child.on('close', resolve); });
  assert.equal(code, 0, stderr + stdout);
  assert.deepEqual(JSON.parse(stdout), { code: 0, asked: true });
  await assert.rejects(stat(join(f.cwd, 'guarded.txt')), 'denied write must not happen');
});

test('resuming a session by ID or with `goon resume` from another folder switches to its workspace', async t => {
  const { spawn } = await import('node:child_process');
  const { realpath } = await import('node:fs/promises');
  const f = await fixture(t);
  const p = await provider(t, (_, response) => sse(response, { text: 'ok' }));
  const first = await cli(f, p.url, ['--json', '-p', 'start work']).done;
  assert.equal(first.code, 0, first.stderr);
  const id = JSON.parse(first.stdout.split('\n')[0]).id;
  const elsewhere = join(f.root, 'elsewhere');
  await mkdir(elsewhere);
  const run = (args, input = '') => new Promise((resolveRun, reject) => {
    const child = spawn(process.execPath, [resolve(process.env.GOON_TEST_BIN ?? 'bin/goon.mjs'), '--base-url', p.url, '--model', 'test-model', ...args], {
      cwd: elsewhere,
      env: { ...process.env, XDG_CONFIG_HOME: f.config, XDG_DATA_HOME: f.data, GOON_API_KEY: 'test-secret-never-echo' },
      stdio: ['pipe', 'pipe', 'pipe'],
    });
    let stdout = '', stderr = '';
    child.stdout.on('data', c => { stdout += c; });
    child.stderr.on('data', c => { stderr += c; });
    child.on('error', reject);
    child.on('close', code => resolveRun({ code, stdout, stderr }));
    child.stdin.end(input);
  });
  const byId = await run(['--json', '--resume', id.slice(0, 8), '-p', 'continue']);
  assert.equal(byId.code, 0, byId.stderr);
  const started = JSON.parse(byId.stdout.split('\n')[0]);
  assert.equal(started.id, id);
  assert.equal(started.cwd, await realpath(f.cwd));
  // `goon resume` with a single saved session resumes it directly, whatever the current folder.
  const plain = await run(['--json', 'resume'], 'one more');
  assert.equal(plain.code, 0, plain.stderr);
  assert.equal(JSON.parse(plain.stdout.split('\n')[0]).id, id);
});
