import test from 'node:test';
import assert from 'node:assert/strict';
import { readdir, readFile } from 'node:fs/promises';
import { join } from 'node:path';
import { setTimeout } from 'node:timers/promises';
import { Harness } from '../dist/harness.js';
import { SessionStore } from '../dist/session.js';
import { Subagents } from '../dist/subagents.js';
import { ToolRegistry } from '../dist/tools.js';
import { fixture, provider, sse, cli } from './helpers.mjs';

const isChild = body => body.messages.some(m => typeof m.content === 'string' && m.content.startsWith('You are a delegated agent.'));
const configFor = (f, p, extra = {}) => ({ cwd: f.cwd, dataDir: f.data, provider: 'openai-compatible', model: 'test-model', baseUrl: p.url,
  apiKey: 'fake', contextWindow: 128000, maxOutputTokens: 8192, timeoutMs: 3000, runTokenBudget: 100000, ...extra });
async function harness(t, f, p, approve, extra) {
  const config = configFor(f, p, extra);
  const store = await SessionStore.open(join(f.data, 'sessions'), config); t.after(() => store.close());
  const tools = new ToolRegistry(f.cwd, approve);
  new Subagents(config, store, tools).register();
  return { store, run: signal => new Harness(config, store, tools).run('parent-private-context', signal ?? new AbortController().signal) };
}

test('real CLI delegates isolated research contexts, caps concurrency at two and saves child sessions', async t => {
  const f = await fixture(t);
  let active = 0, maximum = 0, children = 0;
  const p = await provider(t, async (body, response) => {
    if (isChild(body)) {
      children++; active++; maximum = Math.max(maximum, active);
      assert(!JSON.stringify(body.messages).includes('parent-private-context'));
      assert.deepEqual(body.tools.map(t => t.function.name).sort(), ['list_files', 'read_file', 'search']);
      await setTimeout(60); active--; return sse(response, { text: `Report ${children}` });
    }
    if (!body.messages.some(m => m.role === 'tool')) return sse(response, { calls: [{ id: 'delegate1', name: 'delegate_task', input: { tasks: ['Inspect alpha', 'Inspect beta', 'Inspect gamma'] } }] });
    assert.match(body.messages.at(-1).content, /Session .*\nTask: Inspect alpha/);
    assert.match(body.messages.at(-1).content, /Inspect gamma/);
    return sse(response, { text: 'Reports combined.' });
  });
  const result = await cli(f, p.url, ['--read-only', '--json', '-p', 'parent-private-context']).done;
  assert.equal(result.code, 0, result.stdout + result.stderr);
  assert.equal(maximum, 2); assert.equal(children, 3);
  const events = result.stdout.trim().split('\n').map(JSON.parse), parent = events[0].id;
  assert.equal(events.filter(e => e.type === 'subagent' && e.state === 'done').length, 3);
  const names = await readdir(join(f.data, 'goon/subagents', parent));
  assert.equal(names.filter(n => n.endsWith('.json')).length, 3);
  assert(!names.some(n => n.endsWith('.lock')));
  assert.equal(events.at(-1).text, 'Reports combined.');
});

test('subagent write denial propagates to stop the parent and leaves no side effect', async t => {
  const f = await fixture(t), approvals = [];
  const p = await provider(t, (body, response) => {
    if (isChild(body)) return sse(response, { calls: [{ id: 'write-child', name: 'write_file', input: { path: 'denied.txt', content: 'no' } }] });
    return sse(response, { calls: [{ id: 'edit-agent', name: 'delegate_edit', input: { tasks: ['Write denied.txt'] } }] });
  });
  const h = await harness(t, f, p, async request => { approvals.push(request.tool); return request.tool === 'delegate_edit'; });
  await h.run();
  assert.deepEqual(approvals, ['delegate_edit', 'write_file']);
  assert.equal(p.requests.length, 2);
  assert.equal(h.store.session.messages.at(-1).content[0].output.type, 'error-text');
  await assert.rejects(readFile(join(f.cwd, 'denied.txt')), { code: 'ENOENT' });
  assert(!(await readdir(join(f.data, 'subagents', h.store.session.id))).some(n => n.endsWith('.lock')));
});

test('writable delegated tasks run sequentially and each child mutation uses the parent approval policy', async t => {
  const f = await fixture(t), approvals = [];
  let active = 0, maximum = 0;
  const p = await provider(t, async (body, response) => {
    if (isChild(body)) {
      assert(!body.tools.some(t => t.function.name.startsWith('delegate_')));
      const task = body.messages.find(m => m.role === 'user').content.split('\n').at(-1);
      if (!body.messages.some(m => m.role === 'tool')) {
        active++; maximum = Math.max(maximum, active); await setTimeout(30);
        return sse(response, { calls: [{ id: 'write-child', name: 'write_file', input: { path: task, content: task } }] });
      }
      active--; return sse(response, { text: `Created ${task}` });
    }
    if (!body.messages.some(m => m.role === 'tool')) return sse(response, { calls: [{ id: 'edit-agent', name: 'delegate_edit', input: { tasks: ['a.txt', 'b.txt'] } }] });
    return sse(response, { text: 'Finished edits.' });
  });
  const h = await harness(t, f, p, async request => { approvals.push(request.tool); return true; });
  assert.equal(await h.run(), 'Finished edits.');
  assert.equal(maximum, 1);
  assert.deepEqual(approvals, ['delegate_edit', 'write_file', 'write_file']);
  assert.equal(await readFile(join(f.cwd, 'a.txt'), 'utf8'), 'a.txt');
  assert.equal(await readFile(join(f.cwd, 'b.txt'), 'utf8'), 'b.txt');
});

test('child usage consumes the shared parent run budget', async t => {
  const f = await fixture(t);
  const p = await provider(t, (body, response) => {
    if (isChild(body)) return sse(response, { text: 'Child finished.' });
    return sse(response, { calls: [{ id: 'delegate1', name: 'delegate_task', input: { tasks: ['Inspect files'] } }] });
  });
  const h = await harness(t, f, p, async () => true, { runTokenBudget: 200 });
  await assert.rejects(h.run(), /token budget reached/);
  assert.equal(p.requests.length, 2, 'no parent continuation after parent+child usage exceeds budget');
});

test('SIGINT cancels and drains active subagents and releases all locks', async t => {
  const f = await fixture(t);
  let ready, count = 0;
  const started = new Promise(resolve => { ready = resolve; });
  const p = await provider(t, (body, response) => {
    if (!isChild(body)) return sse(response, { calls: [{ id: 'delegate1', name: 'delegate_task', input: { tasks: ['Wait one', 'Wait two'] } }] });
    response.writeHead(200, { 'content-type': 'text/event-stream' }); response.write(': waiting\n\n');
    if (++count === 2) ready();
  });
  const running = cli(f, p.url, ['--read-only', '--json', '-p', 'delegate']);
  await started; running.child.kill('SIGINT');
  const result = await running.done;
  assert.equal(result.code, 130, result.stdout + result.stderr);
  const parent = JSON.parse(result.stdout.split('\n')[0]).id;
  assert(!(await readdir(join(f.data, 'goon/subagents', parent))).some(n => n.endsWith('.lock')));
  assert(!(await readdir(join(f.data, 'goon/sessions'))).some(n => n.endsWith('.lock')));
  const session = JSON.parse(await readFile(join(f.data, 'goon/sessions', `${parent}.json`), 'utf8'));
  assert.equal(session.messages.at(-1).content[0].toolCallId, 'delegate1');
});

test('--no-subagents removes both delegation tools', async t => {
  const f = await fixture(t);
  const p = await provider(t, (body, response) => {
    assert.deepEqual(body.tools.map(t => t.function.name).sort(), ['list_files', 'read_file', 'search']);
    sse(response, { text: 'Direct run.' });
  });
  const result = await cli(f, p.url, ['--no-subagents', '--read-only', '-p', 'inspect']).done;
  assert.equal(result.code, 0, result.stderr);
});
