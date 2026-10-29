import test from 'node:test';
import assert from 'node:assert/strict';
import { readdir, readFile } from 'node:fs/promises';
import { join } from 'node:path';
import { setTimeout } from 'node:timers/promises';
import { fixture, provider, sse, cli } from './helpers.mjs';

const isChild = body => body.messages.some(m => typeof m.content === 'string' && m.content.startsWith('You are a delegated agent.'));
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

test('JSON child progress is independent of text token count and carries task only once', async t => {
  const f = await fixture(t);
  const tasks = ['alpha', 'beta'].map(name => `Inspect ${name}: ${'context '.repeat(150)}`.trim());
  const p = await provider(t, (body, response) => {
    if (isChild(body)) {
      if (!body.messages.some(m => m.role === 'tool')) return sse(response, {
        reasoning: 'native child reasoning', textChunks: [...'PREAMBLE'.repeat(100)],
        calls: [{ id: 'child-list', name: 'list_files', input: {} }],
      });
      return sse(response, { reasoning: 'more native reasoning', textChunks: [...'<think>child private</think>FINAL_REPORT'] });
    }
    if (!body.messages.some(m => m.role === 'tool')) return sse(response, { calls: [{ id: 'delegate-many', name: 'delegate_task', input: { tasks } }] });
    assert(!body.messages.at(-1).content.includes('PREAMBLE'));
    assert(!body.messages.at(-1).content.includes('child private'));
    assert.equal(body.messages.at(-1).content.split('FINAL_REPORT').length - 1, 2);
    return sse(response, { text: 'Reports combined.' });
  });
  const result = await cli(f, p.url, ['--show-reasoning', '--read-only', '--json', '-p', 'delegate']).done;
  assert.equal(result.code, 0, result.stdout + result.stderr);
  const events = result.stdout.trim().split('\n').map(JSON.parse);
  const children = events.filter(e => e.type === 'subagent');
  assert.equal(children.length, 12, 'two children: start, two usages, two tool states, terminal; no token mirrors');
  assert(!JSON.stringify(children).includes('PREAMBLE'));
  assert(!JSON.stringify(children).includes('child private'));
  const ids = [...new Set(children.map(e => e.id))]; assert.equal(ids.length, 2);
  for (const id of ids) {
    const own = children.filter(e => e.id === id);
    assert.deepEqual(own.map(e => e.state), ['start', 'progress', 'progress', 'progress', 'progress', 'done']);
    assert(tasks.includes(own[0].task));
    assert(own.slice(1).every(e => !('task' in e)));
    assert.deepEqual(own.filter(e => e.state === 'progress').map(e => e.event.type), ['usage', 'tool', 'tool', 'usage']);
    const root = join(f.data, 'goon/subagents', events[0].id);
    const transcript = await readFile(join(root, `${id}.json`), 'utf8');
    assert(transcript.includes('PREAMBLE')); assert(transcript.includes('child private'));
  }
});
