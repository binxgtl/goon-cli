import test from 'node:test';
import assert from 'node:assert/strict';
import { PassThrough } from 'node:stream';
import { TerminalUI, terminalText, wrapTerminal } from '../dist/tui.js';

function terminal(t) {
  const input = new PassThrough(), output = new PassThrough();
  input.isRaw = false; input.setRawMode = value => { input.isRaw = value; };
  output.columns = 40; output.rows = 12;
  let written = '', cancelled = 0;
  output.on('data', chunk => { written += chunk; });
  const ui = new TerminalUI('test-model', () => { cancelled++; }, input, output);
  t.after(() => ui.close());
  const key = (text, name, extra = {}) => input.emit('keypress', text, { name, ...extra });
  return { ui, input, output, key, get written() { return written; }, get cancelled() { return cancelled; } };
}

test('TUI edits Unicode input, recalls history and restores terminal state on close', async t => {
  const tty = terminal(t);
  assert.equal(tty.input.isRaw, true);
  const first = tty.ui.question('goon> ', undefined, true);
  tty.key('xin chào', undefined); tty.key('', 'left'); tty.key('', 'backspace'); tty.key('a', 'a'); tty.key('', 'return');
  assert.equal(await first, 'xin chao');
  const second = tty.ui.question('goon> ', undefined, true);
  tty.key('', 'up'); tty.key('', 'return'); assert.equal(await second, 'xin chao');
  tty.ui.close(); tty.ui.close();
  assert.equal(tty.input.isRaw, false);
  assert.equal(tty.input.listenerCount('keypress'), 0);
  assert.equal(tty.output.listenerCount('resize'), 0);
  assert(tty.written.includes('\x1b[?1049h'));
  assert(tty.written.endsWith('\x1b[?25h\x1b[?1049l'));
});

test('TUI cancels approvals, handles Ctrl+C and redraws at narrow terminal sizes', async t => {
  const tty = terminal(t), controller = new AbortController();
  const approval = tty.ui.question('[y/N] ', controller.signal);
  controller.abort(); await assert.rejects(approval, /cancelled/);
  tty.key('', 'c', { ctrl: true }); assert.equal(tty.cancelled, 1);
  tty.ui.event({ type: 'text', text: 'hello\nworld\x1b[2J' });
  tty.ui.event({ type: 'tool', name: 'read_file', id: 'one', state: 'start' });
  tty.ui.event({ type: 'subagent', id: 'child', task: 'Inspect', state: 'start' });
  tty.ui.event({ type: 'subagent', id: 'child', state: 'progress', event: { type: 'usage', total: 15 } });
  tty.output.columns = 4; tty.output.rows = 8; tty.output.emit('resize');
  const prompt = tty.ui.question(); tty.key('abc', undefined);
  await new Promise(resolve => setTimeout(resolve, 40));
  tty.key('', 'return'); assert.equal(await prompt, 'abc');
  tty.ui.close();
});

test('terminal rendering strips complete escape sequences and wraps wide characters', () => {
  assert.equal(terminalText('safe\x1b[2Jtext\x1b]52;c;c2VjcmV0\x07end'), 'safetextend');
  assert.deepEqual(wrapTerminal('ab界c', 4), ['ab界', 'c']);
});

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

test('TUI separates opt-in reasoning from answers and consumes compact child progress', async t => {
  const tty = terminal(t); tty.output.columns = 100; tty.output.rows = 24;
  tty.ui.event({ type: 'reasoning', source: 'provider', text: 'native detail' });
  tty.ui.event({ type: 'reasoning', source: 'inline', text: 'thinking ' });
  tty.ui.event({ type: 'reasoning', source: 'inline', text: 'detail' });
  tty.ui.event({ type: 'text', text: 'Visible answer.' });
  tty.ui.event({ type: 'subagent', id: 'child123', state: 'start', task: 'Inspect one file' });
  tty.ui.event({ type: 'subagent', id: 'child123', state: 'progress', event: { type: 'usage', input: 10, output: 5, total: 15 } });
  tty.ui.event({ type: 'subagent', id: 'child123', state: 'progress', event: { type: 'tool', name: 'read_file', id: 'r1', state: 'done' } });
  tty.ui.event({ type: 'subagent', id: 'child123', state: 'done' });
  await new Promise(resolve => setTimeout(resolve, 40));
  const frame = tty.written.split('\x1b[H\x1b[2J').at(-1);
  assert.equal(frame.split('[reasoning · inline]').length - 1, 1);
  assert.equal(frame.split('[reasoning · provider]').length - 1, 1);
  assert.match(frame, /\[answer\]/); assert.match(frame, /Visible answer/);
  assert.match(frame, /agent child123: done/); assert.match(frame, /15 tokens/);
  assert.equal(frame.split('Inspect one file').length - 1, 1);
});

test('TUI supports slash-command Tab completion, Shift+Tab mode toggle, Alt+. effort cycling, and dynamic setup wizard with zero hardcoded models', async t => {
  const tty = terminal(t);
  let modeToggles = 0;
  const efforts = [];
  tty.ui.onToggleMode = () => { modeToggles++; tty.ui.setMode('auto'); };
  tty.ui.onCycleEffort = dir => { efforts.push(dir); tty.ui.setEffort('xhigh'); };
  const q = tty.ui.question('goon> ', undefined, true);
  tty.key('', 'tab', { shift: true });
  assert.equal(modeToggles, 1);
  tty.key('.', '.', { meta: true });
  assert.deepEqual(efforts, [1]);
  tty.key('/eff', undefined);
  tty.key('', 'tab');
  tty.key('', 'return');
  assert.equal((await q).trim(), '/effort');

  const { fixture } = await import('./helpers.mjs');
  const { readFile } = await import('node:fs/promises');
  const { join } = await import('node:path');
  const { runSetupWizard, resolveRuntime, PROVIDER_PRESETS, EFFORT_LEVELS } = await import('../dist/index.js');
  assert.deepEqual(EFFORT_LEVELS, ['low', 'medium', 'high', 'xhigh', 'max']);
  for (const preset of PROVIDER_PRESETS) {
    assert(!('models' in preset), `Preset ${preset.id} must not hardcode models`);
  }

  const f = await fixture(t);
  const oldConfigHome = process.env.XDG_CONFIG_HOME;
  const oldKey = process.env.GOON_API_KEY;
  process.env.XDG_CONFIG_HOME = f.config;
  delete process.env.GOON_API_KEY;
  t.after(() => {
    if (oldConfigHome === undefined) delete process.env.XDG_CONFIG_HOME;
    else process.env.XDG_CONFIG_HOME = oldConfigHome;
    if (oldKey !== undefined) process.env.GOON_API_KEY = oldKey;
  });

  // Step 1: provider=4 (OpenRouter), Step 2: URL, Step 3: API Key, Step 4: pick discovered model #1, Step 5: effort=xhigh
  const answers = ['4', 'https://openrouter.ai/api/v1', 'sk-or-wizard-secret-key', '1', 'xhigh'];
  let outputLog = '';
  const updated = await runSetupWizard(undefined, {
    write: text => { outputLog += text; },
    ask: async () => answers.shift() ?? '',
    discover: async (url, key) => {
      assert.equal(url, 'https://openrouter.ai/api/v1');
      assert.equal(key, 'sk-or-wizard-secret-key');
      return [{ id: 'dynamic/discovered-model-v1', contextWindow: 200_000 }];
    },
  });
  assert.equal(updated.provider, 'openai-compatible');
  assert.equal(updated.baseUrl, 'https://openrouter.ai/api/v1');
  assert.equal(updated.model, 'dynamic/discovered-model-v1');
  assert.equal(updated.effort, 'xhigh');
  assert.match(outputLog, /Setup Saved/);

  const rawConfig = await readFile(join(f.config, 'goon', 'config.json'), 'utf8');
  assert(!rawConfig.includes('sk-or-wizard-secret-key'));
  const runtime = await resolveRuntime(updated, f.cwd);
  assert.equal(runtime.apiKey, 'sk-or-wizard-secret-key');
  assert.equal(runtime.effort, 'xhigh');
});

