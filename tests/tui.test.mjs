// End-to-end terminal tests against the real binary. The TUI widget tests live in src/tui/tests.rs.
import test from 'node:test';
import assert from 'node:assert/strict';
import { fixture, provider, sse, cli } from './helpers.mjs';

test('real TUI pseudo-terminal handles status, resize, approval and clean exit', async t => {
  if (process.platform === 'win32') { t.skip('POSIX pseudo-terminal test'); return; }
  const { spawn } = await import('node:child_process');
  const { resolve, join } = await import('node:path');
  const { readFile, readdir } = await import('node:fs/promises');
  const { fixture, provider, sse } = await import('./helpers.mjs');
  const f = await fixture(t);
  const p = await provider(t, (_, response, n) => n === 1 ? sse(response, {
    calls: [{ id: 'ui-write', name: 'write_file', input: { path: 'ui.txt', content: 'approved from TUI' } }],
  }) : sse(response, { text: 'Created by TUI.' }));
  const child = spawn('python3', [resolve('tests/fixtures/terminal.py'), process.execPath, resolve(process.env.GOON_TEST_BIN ?? 'bin/goon.mjs'),
    '--cwd', f.cwd, '--model', 'test-model', '--base-url', p.url], {
    env: { ...process.env, GOON_API_KEY: 'tui-test-key', XDG_CONFIG_HOME: f.config, XDG_DATA_HOME: f.data },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  t.after(() => child.kill('SIGKILL'));
  let stdout = '', stderr = '';
  child.stdout.on('data', c => { stdout += c; }); child.stderr.on('data', c => { stderr += c; });
  const result = await new Promise((resolve, reject) => { child.on('error', reject); child.on('close', code => resolve(code)); });
  assert.equal(result, 0, stderr + stdout);
  assert.deepEqual(JSON.parse(stdout), { code: 0, entered: true, restored: true, canonical: true });
  assert.equal(await readFile(join(f.cwd, 'ui.txt'), 'utf8'), 'approved from TUI');
  assert(!(await readdir(join(f.data, 'goon/sessions'))).some(n => n.endsWith('.lock')));
});

test('an active goal keeps the CLI going with a continuation turn until update_goal(complete)', async t => {
  const f = await fixture(t);
  let step = 0;
  let sawContinuation = false;
  const p = await provider(t, (body, response) => {
    step++;
    const lastUser = [...(body.messages ?? [])].reverse().find(m => m.role === 'user');
    const lastText = typeof lastUser?.content === 'string' ? lastUser.content : JSON.stringify(lastUser?.content ?? '');
    if (lastText.includes('<codex_internal_context source="goal">')) sawContinuation = true;
    if (step === 1) return sse(response, { calls: [{ id: 'cg_cli', name: 'create_goal', input: { objective: 'Autonomous multi-turn goal' } }] });
    if (step === 2) return sse(response, { text: 'Step 1 progress done; continuing next turn.' });
    if (step === 3) return sse(response, { calls: [{ id: 'ug_cli', name: 'update_goal', input: { status: 'complete' } }] });
    return sse(response, { text: 'All requirements verified and goal completed.' });
  });
  const { code, stdout, stderr } = await cli(f, p.url, ['--provider', 'openai-compatible', '--yes', '--no-mcp', '--no-subagents', '--json', '-p', 'Start autonomous goal']).done;
  assert.equal(code, 0, stderr + stdout);
  assert.equal(step, 4, 'Turn 1 (2 model calls) + the automatic continuation turn (2 model calls)');
  assert.equal(sawContinuation, true, 'the continuation turn carries the goal continuation context');
});
