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
