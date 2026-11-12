// End-to-end terminal tests against the real binary. The TUI widget tests live in src/tui/tests.rs.
import test from 'node:test';
import assert from 'node:assert/strict';
import { fixture, provider, sse, cli } from './helpers.mjs';

for (const inline of [false, true]) {
  test(`real ${inline ? 'inline' : 'fullscreen'} TUI pseudo-terminal handles status, resize, approval and clean exit`, { timeout: 60_000 }, async t => {
    const windows = process.platform === 'win32';
    const { spawn } = await import('node:child_process');
    const { resolve, join } = await import('node:path');
    const { readFile, readdir, writeFile } = await import('node:fs/promises');
    const f = await fixture(t);
    const editor = join(f.cwd, 'fake editor.mjs');
    await writeFile(editor, "import fs from 'node:fs'; const path = process.argv[2]; fs.writeFileSync(path + '.seed', fs.readFileSync(path)); fs.writeFileSync(path, 'editor-restored-marker\\n');");
    let calls = 0;
    const p = await provider(t, (_, response) => {
      calls++;
      if (windows && calls === 1) return sse(response, { calls: [{ id: 'questions', name: 'request_user_input', input: { questions: [
        { id: 'first', header: 'First', question: 'First choice?', options: [{ label: 'A' }, { label: 'B' }] },
        { id: 'second', header: 'Second', question: 'Second choice?', options: [{ label: 'C' }, { label: 'D' }] },
      ] } }] });
      if (calls === (windows ? 2 : 1)) return sse(response, { calls: [{ id: 'ui-write', name: 'write_file', input: { path: 'ui.txt', content: 'approved from TUI' } }] });
      return sse(response, { text: 'Created by TUI.' });
    });
    const [handleCompletion] = p.server.listeners('request');
    p.server.removeAllListeners('request');
    p.server.on('request', (request, response) => {
      if (request.method === 'GET' && request.url === '/v1/models') {
        response.writeHead(200, { 'content-type': 'application/json' });
        response.end(JSON.stringify({ data: [{ id: 'test-model' }, { id: 'other-model' }] }));
      } else {
        handleCompletion(request, response);
      }
    });
    const child = spawn(windows ? 'python' : 'python3', [resolve(windows ? 'tests/fixtures/terminal-windows.py' : 'tests/fixtures/terminal.py'), process.execPath, resolve(process.env.GOON_TEST_BIN ?? 'bin/goon.mjs'),
      '--cwd', f.cwd, '--model', 'test-model', '--base-url', p.url, ...(inline ? ['--no-alt-screen'] : [])], {
      env: { ...process.env, GOON_API_KEY: 'tui-test-key', XDG_CONFIG_HOME: f.config, XDG_DATA_HOME: f.data, VISUAL: `"${process.execPath}" "${editor}"` },
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    t.after(() => child.kill('SIGKILL'));
    let stdout = '', stderr = '';
    child.stdout.on('data', c => { stdout += c; }); child.stderr.on('data', c => { stderr += c; });
    const result = await new Promise((resolve, reject) => { child.on('error', reject); child.on('close', code => resolve(code)); });
    assert.equal(result, 0, stderr + stdout);
    const terminal = JSON.parse(stdout);
    assert.equal(terminal.code, 0);
    assert.equal(terminal.entered, !inline);
    if (windows) {
      assert.equal(terminal.left, !inline);
      assert.equal(terminal.paste_disabled, true);
    } else {
      assert.equal(terminal.restored, !inline);
      assert.equal(terminal.canonical, true);
    }
    assert.equal(await readFile(join(f.cwd, 'ui.txt'), 'utf8'), 'approved from TUI');
    if (windows) {
      const { existsSync } = await import('node:fs');
      assert.equal(existsSync(join(f.config, 'goon/config.json')), false, 'cancelling model/effort must not save staged settings');
    }
    const editorDir = join(f.data, 'goon/editor');
    const files = await readdir(editorDir);
    assert.equal(files.filter(name => name.endsWith('.md')).length, 0, 'temporary draft removed after editor');
    const seed = await readFile(join(editorDir, files.find(name => name.endsWith('.seed'))), 'utf8');
    assert(seed.includes('editor seed one\neditor seed two\neditor seed three'), 'external editor receives expanded paste, not placeholders');
    assert(!(await readdir(join(f.data, 'goon/sessions'))).some(n => n.endsWith('.lock')));
  });
}

for (const mode of ['read', 'terminate']) {
  test(`POSIX external editor owns foreground tty and ${mode === 'read' ? 'returns draft' : 'terminates descendants'}`, { skip: process.platform === 'win32', timeout: 30_000 }, async t => {
    const { spawn } = await import('node:child_process');
    const { resolve } = await import('node:path');
    const { ensureBinary } = await import('../scripts/install.mjs');
    const f = await fixture(t);
    const binary = await ensureBinary(resolve('.'));
    const child = spawn('python3', [resolve('tests/fixtures/editor-posix.py'), binary, f.cwd, mode], {
      env: { ...process.env, XDG_CONFIG_HOME: f.config, XDG_DATA_HOME: f.data }, stdio: ['ignore', 'pipe', 'pipe'],
    });
    t.after(() => child.kill('SIGKILL'));
    let stdout = '', stderr = '';
    child.stdout.on('data', c => { stdout += c; }); child.stderr.on('data', c => { stderr += c; });
    const code = await new Promise((resolve, reject) => { child.on('error', reject); child.on('close', resolve); });
    assert.equal(code, 0, stderr + stdout);
    const result = JSON.parse(stdout);
    assert.equal(result.code, mode === 'read' ? 0 : 143);
    assert.equal(result.foreground_restored, true);
    assert.equal(result.tree_stopped, true);
  });
}

for (const mode of ['fullscreen', 'inline', 'terminate']) {
  test(`Linux Ctrl+Z suspend/${mode} restores terminal ownership`, { skip: process.platform !== 'linux', timeout: 30_000 }, async t => {
    const { spawn } = await import('node:child_process');
    const { resolve } = await import('node:path');
    const { ensureBinary } = await import('../scripts/install.mjs');
    const f = await fixture(t);
    const binary = await ensureBinary(resolve('.'));
    const child = spawn('python3', [resolve('tests/fixtures/suspend-posix.py'), binary, f.cwd, mode], {
      env: { ...process.env, XDG_CONFIG_HOME: f.config, XDG_DATA_HOME: f.data }, stdio: ['ignore', 'pipe', 'pipe'],
    });
    t.after(() => child.kill('SIGKILL'));
    let stdout = '', stderr = '';
    child.stdout.on('data', c => { stdout += c; }); child.stderr.on('data', c => { stderr += c; });
    const code = await new Promise((resolve, reject) => { child.on('error', reject); child.on('close', resolve); });
    assert.equal(code, 0, stderr + stdout);
    const result = JSON.parse(stdout);
    assert.equal(result.code, mode === 'terminate' ? 143 : 0);
    assert.equal(result.suspended, true);
    assert.equal(result.canonical, true);
  });
}

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

test('pair_mobile.html allows pinch-zoom, uses 16px auto-growing textarea, scrubs ?t= token into sessionStorage, and honors approval flags', async () => {
  const { readFile } = await import('node:fs/promises');
  const { resolve } = await import('node:path');
  const vm = await import('node:vm');

  const html = await readFile(resolve('src/pair_mobile.html'), 'utf8');
  const viewportMatch = html.match(/<meta\s+name="viewport"\s+content="([^"]+)"/i);
  assert(viewportMatch, 'viewport meta tag must exist');
  assert(!viewportMatch[1].includes('user-scalable=no'), 'mobile viewport must not disable pinch-zoom');
  assert(!viewportMatch[1].includes('maximum-scale=1'), 'mobile viewport must not lock maximum-scale=1');
  assert(viewportMatch[1].includes('viewport-fit=cover'), 'mobile viewport should support notched safe areas');
  assert(/<textarea\s+id="promptInput"[^>]*rows="1"/i.test(html), '#promptInput must be a multi-line <textarea rows="1">');
  assert(/textarea#promptInput\s*\{[^}]*font-size:\s*16px/s.test(html), '#promptInput must have font-size >= 16px to prevent iOS auto-zoom');

  const scriptMatch = html.match(/<script>([\s\S]*?)<\/script>/i);
  assert(scriptMatch, 'inline script must exist');

  function makeElement(tag, id = '') {
    const listeners = new Map();
    return {
      tagName: tag.toUpperCase(),
      id,
      value: '',
      textContent: '',
      className: '',
      dataset: {},
      style: {},
      scrollHeight: 48,
      scrollTop: 0,
      clientHeight: 400,
      children: [],
      classList: { toggle() {} },
      appendChild(child) {
        this.children.push(child);
        return child;
      },
      addEventListener(type, fn) {
        const list = listeners.get(type) || [];
        list.push(fn);
        listeners.set(type, list);
      },
      dispatch(type, ev) {
        for (const fn of listeners.get(type) || []) fn(ev);
      },
    };
  }

  function runSandbox(initialSearch, preStored = {}) {
    const storage = new Map(Object.entries(preStored));
    const fetches = [];
    const sseUrls = [];
    let replacedUrl = null;
    let sseInstance = null;
    const byId = new Map([
      ['log', makeElement('div', 'log')],
      ['meta', makeElement('div', 'meta')],
      ['statusBadge', makeElement('div', 'statusBadge')],
      ['promptInput', makeElement('textarea', 'promptInput')],
      ['stopBtn', makeElement('button', 'stopBtn')],
      ['composer', makeElement('form', 'composer')],
    ]);
    const sandbox = {
      URLSearchParams,
      encodeURIComponent,
      JSON,
      Math,
      Array,
      location: { search: initialSearch, pathname: '/', hash: '' },
      sessionStorage: {
        getItem: k => (storage.has(k) ? storage.get(k) : null),
        setItem: (k, v) => storage.set(k, String(v)),
      },
      history: {
        replaceState(_state, _title, url) {
          replacedUrl = url;
        },
      },
      document: {
        getElementById: id => byId.get(id) || null,
        createElement: tag => makeElement(tag),
        querySelectorAll: () => [],
      },
      fetch: async (url, opts = {}) => {
        fetches.push({ url, opts });
        return { ok: true, json: async () => ({ state: { model: 'm', cwd: '/app', status: 'Ready', mode: 'ask' }, entries: [], pendingApprovals: [] }) };
      },
      EventSource: class {
        constructor(url) {
          sseUrls.push(url);
          sseInstance = this;
        }
      },
      setInterval: () => 1,
    };
    vm.runInNewContext(scriptMatch[1], sandbox);
    return { storage, fetches, sseUrls, replacedUrl, byId, getSse: () => sseInstance };
  }

  const first = runSandbox('?t=pair-tok-999');
  assert.equal(first.storage.get('goon_pair_token'), 'pair-tok-999');
  assert.equal(first.replacedUrl, '/', 'token must be scrubbed from address bar via history.replaceState');
  assert.equal(first.sseUrls[0], '/api/events?t=pair-tok-999');
  assert.equal(first.fetches[0].url, '/api/state?since=0', 'fetch URL must not leak ?t= query parameter');
  assert.equal(first.fetches[0].opts.headers.Authorization, 'Bearer pair-tok-999');

  // Reloading after URL scrub recovers token from sessionStorage
  const reloaded = runSandbox('', { goon_pair_token: 'pair-tok-999' });
  assert.equal(reloaded.fetches[0].opts.headers.Authorization, 'Bearer pair-tok-999');

  // Shift+Enter and IME composition do not submit; plain Enter submits via Bearer header
  const promptEl = reloaded.byId.get('promptInput');
  promptEl.value = 'line 1\nline 2';
  let prevented = false;
  promptEl.dispatch('keydown', { key: 'Enter', shiftKey: true, isComposing: false, keyCode: 13, preventDefault() { prevented = true; } });
  assert.equal(prevented, false, 'Shift+Enter must insert newline without submitting');
  promptEl.dispatch('keydown', { key: 'Enter', shiftKey: false, isComposing: true, keyCode: 229, preventDefault() { prevented = true; } });
  assert.equal(prevented, false, 'IME composition Enter must not submit prematurely');
  promptEl.dispatch('keydown', { key: 'Enter', shiftKey: false, isComposing: false, keyCode: 13, preventDefault() { prevented = true; } });
  assert.equal(prevented, true, 'plain Enter submits composer');
  await new Promise(r => setImmediate(r));
  const promptPost = reloaded.fetches.find(f => f.url === '/api/prompt');
  assert(promptPost, 'expected POST /api/prompt');
  assert.equal(promptPost.opts.headers.Authorization, 'Bearer pair-tok-999');
  assert.equal(JSON.parse(promptPost.opts.body).prompt, 'line 1\nline 2');

  // Approval card respects canAlways: false and custom alwaysLabel
  const sseMsg = reloaded.getSse();
  sseMsg.onmessage({ data: JSON.stringify({ type: 'approval', id: 'ap1', tool: 'bash', preview: 'rm -rf build', canAlways: false }) });
  sseMsg.onmessage({ data: JSON.stringify({ type: 'approval', id: 'ap2', tool: 'bash', preview: 'cargo test', canAlways: true, alwaysLabel: 'Always allow cargo' }) });
  const cards = reloaded.byId.get('log').children;
  const labels1 = cards[0].children[1].children.map(b => b.textContent);
  const labels2 = cards[1].children[1].children.map(b => b.textContent);
  assert.deepEqual(labels1, ['Allow', 'Deny']);
  assert.deepEqual(labels2, ['Allow', 'Always allow cargo', 'Deny']);
});

test('setup wizard accepts preset name and isolates per-provider credentials while NO_COLOR suppresses ANSI escapes', async t => {
  const { readFile } = await import('node:fs/promises');
  const { join } = await import('node:path');
  const f = await fixture(t);
  const p = await provider(t, (_, response) => sse(response, { text: 'plain ok' }));
  const [handleCompletion] = p.server.listeners('request');
  p.server.removeAllListeners('request');
  p.server.on('request', (request, response) => {
    if (request.method === 'GET' && request.url === '/v1/models') {
      response.writeHead(200, { 'content-type': 'application/json' });
      response.end(JSON.stringify({ data: [{ id: 'openai/gpt-5' }] }));
    } else {
      handleCompletion(request, response);
    }
  });

  // Piped `goon setup` choosing preset by name ("openrouter") and validating custom effort retry
  const setupRes = await cli(f, p.url, ['setup'], {
    input: `not-a-preset\nopenrouter\n${p.url}\nsk-or-e2e-key\n1\nmeduim\nhigh\n`,
  }).done;
  assert.equal(setupRes.code, 0, setupRes.stderr + setupRes.stdout);
  assert(setupRes.stderr.includes('https://openrouter.ai/api/v1'), setupRes.stderr);
  const savedCfg = JSON.parse(await readFile(join(f.config, 'goon/config.json'), 'utf8'));
  assert.equal(savedCfg.provider, 'openai-compatible');
  assert.equal(savedCfg.baseUrl, p.url);
  assert.equal(savedCfg.keyEnv, 'OPENROUTER_API_KEY');
  assert.equal(savedCfg.model, 'openai/gpt-5');
  assert.equal(savedCfg.effort, 'high');
  const savedCreds = JSON.parse(await readFile(join(f.config, 'goon/credentials.json'), 'utf8'));
  assert.equal(savedCreds.OPENROUTER_API_KEY, 'sk-or-e2e-key');

  // NO_COLOR=1 one-shot run emits zero SGR escape codes
  const noColorRes = await cli(f, p.url, ['--provider', 'openai-compatible', '--yes', '--no-mcp', '--no-subagents', '-p', 'ping'], {
    env: { NO_COLOR: '1' },
  }).done;
  assert.equal(noColorRes.code, 0, noColorRes.stderr + noColorRes.stdout);
  const esc = String.fromCharCode(0x1b);
  assert(!noColorRes.stdout.includes(`${esc}[`), `NO_COLOR stdout contained CSI: ${JSON.stringify(noColorRes.stdout)}`);
  assert(!noColorRes.stderr.includes(`${esc}[`), `NO_COLOR stderr contained CSI: ${JSON.stringify(noColorRes.stderr)}`);
});
