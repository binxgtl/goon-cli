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

test('Codex TUI renders Markdown bullets without false-positive red diff coloring, coalesces Explored cells, and streams tool_stream status', async t => {
  const tty = terminal(t);
  tty.output.columns = 100;
  tty.output.rows = 30;
  tty.ui.setStatus('Running');

  // 1. Live tool_stream event updates status bar immediately
  tty.ui.event({ type: 'tool_stream', id: 'tc1', name: 'write_file', state: 'delta', preview: 'src/app.ts' });
  await new Promise(resolve => setTimeout(resolve, 25));
  assert(tty.written.includes('\x1b[?2026h'), 'Should wrap frames in DEC Mode 2026 synchronized output');
  assert(tty.written.includes('Streaming write_file: src/app.ts'), 'Should display live tool_stream progress');

  // 2. Coalesced Explored cell for consecutive read/list/search tools
  tty.ui.event({
    type: 'tool',
    id: 'r1',
    name: 'read_file',
    state: 'done',
    meta: { kind: 'read', summary: 'src/index.ts' },
  });
  tty.ui.event({
    type: 'tool',
    id: 's1',
    name: 'search',
    state: 'done',
    meta: { kind: 'search', summary: 'createModel in src' },
  });

  // 3. Ran cell and Edited cell with (+A -R) diff
  tty.ui.event({
    type: 'tool',
    id: 'b1',
    name: 'bash',
    state: 'done',
    meta: { kind: 'exec', summary: 'npm test', durationMs: 1200, outputPreview: ['PASS all tests'], omittedLines: 0 },
  });
  tty.ui.event({
    type: 'tool',
    id: 'e1',
    name: 'edit_file',
    state: 'done',
    meta: {
      kind: 'edit',
      verb: 'Edited',
      summary: 'src/app.ts',
      added: 1,
      removed: 1,
      diff: '--- a/src/app.ts\n+++ b/src/app.ts\n@@ -1,1 +1,1 @@\n-const oldVal = 1;\n+const newVal = 2;',
    },
  });

  // 4. Markdown answer with bullet lists (- item) must NOT be colored red (\x1b[31m)
  tty.ui.event({
    type: 'text',
    text: '# Summary\n- **Todo list:** completed\n- `inline_code` works\n',
  });
  await new Promise(resolve => setTimeout(resolve, 30));

  const lastFrame = tty.written.split('\x1b[H\x1b[2J').at(-1);
  assert.match(lastFrame, /Explored/);
  assert.match(lastFrame, /Read.*src\/index\.ts/);
  assert.match(lastFrame, /Search.*createModel in src/);
  assert.match(lastFrame, /Ran/);
  assert.match(lastFrame, /Edited/);
  assert.match(lastFrame, /• .*Todo list:.* completed/);

  // Verify the bullet list line is NOT colored red (\x1b[31m), while the real diff deletion line IS colored red
  const lines = lastFrame.split('\r\n');
  const todoBulletLine = lines.find(l => l.includes('Todo list:'));
  assert(todoBulletLine, 'Should find rendered Todo list bullet line');
  assert(!todoBulletLine.includes('\x1b[31m'), 'Markdown bullet list item must NOT be colored red as a diff deletion');

  const diffRemovedLine = lines.find(l => l.includes('const oldVal = 1;'));
  assert(diffRemovedLine, 'Should find rendered diff deletion line');
  assert(diffRemovedLine.includes('\x1b[31m'), 'Real diff deletion line must be colored red');
});

test('Codex TUI collapses pasted ScreenClip image paths into [Image #1], prevents line-wrap cursor drift, and scrolls with mouse wheel and Up/Down', async t => {
  const { fixture } = await import('./helpers.mjs');
  const { mkdir, writeFile } = await import('node:fs/promises');
  const { join } = await import('node:path');
  const { stripVTControlCharacters } = await import('node:util');
  const { WorkspaceFiles, expandFileMentions } = await import('../dist/index.js');

  const f = await fixture(t);
  await mkdir(f.data, { recursive: true });
  const pngBytes = Buffer.from(
    'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==',
    'base64',
  );
  // Create an image file with a Windows ScreenClip-style GUID filename in f.data (outside workspace f.cwd!)
  const externalImgPath = join(f.data, '{25D1BD96-7B57-4392-BBF0-17E774BEEFF2}.png');
  await writeFile(externalImgPath, pngBytes);

  const tty = terminal(t);
  tty.output.columns = 80;
  tty.output.rows = 16;

  // 1. Emit 25 lines of long reasoning & answer text to overflow the 16-row viewport
  tty.ui.event({
    type: 'reasoning',
    source: 'provider',
    text: ('Long reasoning line that previously wrapped after adding the 4-char prefix. ').repeat(6) + '\n',
  });
  for (let i = 1; i <= 15; i++) {
    tty.ui.event({ type: 'text', text: `Line ${i} of assistant answer in transcript.\n` });
  }
  await new Promise(resolve => setTimeout(resolve, 25));

  // Verify NO rendered line in the frame exceeds columns - 1 (79 visible chars)
  const rawLastFrame = tty.written.split('\x1b[H\x1b[2J').at(-1);
  const frameAfterStream = rawLastFrame.endsWith('\x1b[?2026l')
    ? rawLastFrame.slice(0, -'\x1b[?2026l'.length)
    : rawLastFrame;
  for (const rawLine of frameAfterStream.split('\r\n')) {
    const cleanLine = stripVTControlCharacters(rawLine.split('\r')[0]);
    assert(
      cleanLine.length <= 79,
      `Rendered line exceeded 79 visible columns (${cleanLine.length}): ${JSON.stringify(cleanLine)}`,
    );
  }

  // 2. Start prompt question and test Up/Down & SGR Mouse Wheel scrollback when transcript overflows
  const q = tty.ui.question('goon> ', undefined, true);
  await new Promise(resolve => setTimeout(resolve, 25));

  // Press Up arrow on empty input -> scrolls up in transcript and displays "↑ Scrolled up" banner
  tty.key('', 'up');
  await new Promise(resolve => setTimeout(resolve, 25));
  let latestFrame = tty.written.split('\x1b[H\x1b[2J').at(-1);
  assert.match(latestFrame, /↑ Scrolled up/);

  // SGR mouse wheel up (\x1b[<64;10;10M) scrolls further up without leaking "64;10;10M" into input
  tty.key(undefined, 'undefined', { sequence: '\x1b[<' });
  for (const ch of '64;10;10M') tty.key(ch, ch);
  await new Promise(resolve => setTimeout(resolve, 25));

  // Press Escape while scrolled up -> returns to bottom (scroll = 0)
  tty.key('', 'escape');
  await new Promise(resolve => setTimeout(resolve, 25));
  latestFrame = tty.written.split('\x1b[H\x1b[2J').at(-1);
  assert(!latestFrame.includes('↑ Scrolled up'), 'Escape should return scroll position to bottom');

  // 3. Simulate pasting the external ScreenClip image path via bracketed paste
  tty.key(undefined, 'paste-start', { sequence: '\x1b[200~' });
  tty.key(externalImgPath, undefined);
  tty.key(undefined, 'paste-end', { sequence: '\x1b[201~' });
  await new Promise(resolve => setTimeout(resolve, 25));

  latestFrame = tty.written.split('\x1b[H\x1b[2J').at(-1);
  // Composer shows [Image #1] pill and Attached status bar instead of raw 100+ char path
  assert.match(latestFrame, /\[Image #1\]/);
  assert.match(latestFrame, /📎 Attached \[Image #1\]/);
  // Cursor is positioned on the prompt line itself via \r\x1b[<col>G\x1b[?25h
  assert(latestFrame.includes('\r\x1b[') && latestFrame.includes('G\x1b[?25h'));

  // Submit the pasted image
  tty.key('', 'return');
  const submitted = await q;
  assert.equal(submitted.trim(), `@"${externalImgPath}"`);

  // 4. Verify expandFileMentions loads both @"<externalImgPath>" AND bare pasted externalImgPath (without @)
  const wsFiles = new WorkspaceFiles(f.cwd);
  const fromMention = await expandFileMentions(submitted, wsFiles, { model: 'space-bunny' });
  assert.equal(fromMention.imageParts.length, 1);
  assert.equal(fromMention.imageParts[0].mediaType, 'image/png');

  const fromBarePaste = await expandFileMentions(`Please inspect ${externalImgPath}`, wsFiles, { model: 'space-bunny' });
  assert.equal(fromBarePaste.imageParts.length, 1);
  assert.equal(fromBarePaste.imageParts[0].mediaType, 'image/png');
  assert.match(fromBarePaste.text, /\[Image #1: \{25D1BD96-7B57-4392-BBF0-17E774BEEFF2\}\.png\]/);
});



