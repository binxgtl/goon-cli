import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile, readdir } from 'node:fs/promises';
import { join } from 'node:path';
import { fixture, provider, sse, cli } from './helpers.mjs';

const eventsOf = result => result.stdout.trim().split('\n').map(JSON.parse);
function failedAnswer(result) {
  assert.equal(result.code, 1, result.stdout + result.stderr);
  const events = eventsOf(result);
  assert(!events.some(e => e.type === 'done'), 'missing terminal answer cannot emit successful done');
  assert.match(events.at(-1).message, /without a visible answer/);
  return events;
}

for (const [name, reply] of [
  ['empty', {}],
  ['whitespace', { text: ' \n\t' }],
  ['native reasoning only', { reasoning: 'private thought' }],
]) {
  test(`rejects ${name} terminal output with no tools`, async t => {
    const f = await fixture(t), p = await provider(t, (_, response) => sse(response, reply));
    const events = failedAnswer(await cli(f, p.url, ['--json', '-p', 'answer']).done);
    assert.equal(p.requests.length, 1, 'no automatic paid retries for missing answers');
    const root = join(f.data, 'goon/sessions');
    assert(!(await readdir(root)).some(n => n.endsWith('.lock')));
    const stored = JSON.parse(await readFile(join(root, `${events[0].id}.json`), 'utf8'));
    assert.equal(stored.tokens, 110, 'usage survives a failed terminal answer');
  });
}

test('opaque gateway reasoning with no reasoning delta cannot bypass missing-answer validation', async t => {
  const f = await fixture(t);
  const p = await provider(t, (_, response) => {
    response.writeHead(200, { 'content-type': 'text/event-stream' });
    response.write(`data: ${JSON.stringify({ id: 'opaque', choices: [{ index: 0, delta: { role: 'assistant', content: null,
      reasoning_details: [{ type: 'reasoning.encrypted', data: 'opaque-private' }] }, finish_reason: null }] })}\n\n`);
    response.end(`data: ${JSON.stringify({ id: 'opaque', choices: [{ index: 0, delta: {}, finish_reason: 'stop' }], usage: { prompt_tokens: 100, completion_tokens: 10, total_tokens: 110 } })}\n\ndata: [DONE]\n\n`);
  });
  const result = await cli(f, p.url, ['--json', '--show-reasoning', '-p', 'answer']).done;
  const events = failedAnswer(result);
  assert(!events.some(e => e.type === 'reasoning'));
  assert(!result.stdout.includes('opaque-private'));
});

test('earlier tool narration is not a terminal answer; completed mutation is preserved and not replayed on resume', async t => {
  const f = await fixture(t);
  const p = await provider(t, (_, response, n) => {
    if (n === 1) return sse(response, { text: 'I will write the file.', calls: [{ id: 'write-once', name: 'write_file', input: { path: 'once.txt', content: 'done once' } }] });
    return sse(response, n === 2 ? {} : { text: 'The file was written and verified.' });
  });
  const first = await cli(f, p.url, ['--json', '--yes', '-p', 'write']).done;
  const events = failedAnswer(first), id = events[0].id;
  assert(events.some(e => e.type === 'text' && e.text.includes('I ')));
  assert.equal(await readFile(join(f.cwd, 'once.txt'), 'utf8'), 'done once');
  const root = join(f.data, 'goon/sessions');
  const session = JSON.parse(await readFile(join(root, `${id}.json`), 'utf8'));
  assert(session.messages.some(m => m.role === 'tool' && m.content.some(p => p.toolCallId === 'write-once')));
  const resumed = await cli(f, p.url, ['--json', '--resume', id, '-p', 'report what happened']).done;
  assert.equal(resumed.code, 0, resumed.stdout + resumed.stderr);
  assert.equal(eventsOf(resumed).at(-1).text, 'The file was written and verified.');
  assert(!eventsOf(resumed).some(e => e.type === 'tool'), 'resume must not repeat completed side effects');
  assert.equal(p.requests.length, 3);
});

test('parent needs its own terminal answer after successful child completion', async t => {
  const f = await fixture(t);
  const p = await provider(t, (body, response) => {
    if (body.messages.some(m => typeof m.content === 'string' && m.content.startsWith('You are a delegated agent.'))) return sse(response, { text: 'child report' });
    if (!body.messages.some(m => m.role === 'tool')) return sse(response, { calls: [{ id: 'delegate', name: 'delegate_task', input: { tasks: ['inspect'] } }] });
    return sse(response);
  });
  const events = failedAnswer(await cli(f, p.url, ['--read-only', '--json', '-p', 'delegate']).done);
  assert.equal(events.filter(e => e.type === 'subagent' && e.state === 'done').length, 1);
  assert.equal(events.at(-1).type, 'error');
});

test('empty child terminal output produces child error, never child done', async t => {
  const f = await fixture(t);
  const p = await provider(t, (body, response) => {
    if (body.messages.some(m => typeof m.content === 'string' && m.content.startsWith('You are a delegated agent.'))) return sse(response);
    if (!body.messages.some(m => m.role === 'tool')) return sse(response, { calls: [{ id: 'delegate', name: 'delegate_task', input: { tasks: ['inspect'] } }] });
    assert.match(body.messages.at(-1).content, /Task failed:.*without a visible answer/);
    return sse(response, { text: 'The child failed to return an answer.' });
  });
  const result = await cli(f, p.url, ['--read-only', '--json', '-p', 'delegate']).done;
  assert.equal(result.code, 0, result.stdout + result.stderr);
  const events = eventsOf(result);
  assert.equal(events.filter(e => e.type === 'subagent' && e.state === 'error').length, 1);
  assert(!events.some(e => e.type === 'subagent' && e.state === 'done'));
  assert.equal(events.at(-1).type, 'done');
});

test('Anthropic redacted thinking without text deltas fails while preserving SDK replay metadata', async t => {
  const f = await fixture(t);
  const p = await provider(t, (_, response) => {
    response.writeHead(200, { 'content-type': 'text/event-stream' });
    const send = (type, data) => response.write(`event: ${type}\ndata: ${JSON.stringify({ type, ...data })}\n\n`);
    send('message_start', { message: { id: 'redacted', type: 'message', role: 'assistant', model: 'test-model', content: [], stop_reason: null, stop_sequence: null, usage: { input_tokens: 10, output_tokens: 0 } } });
    send('content_block_start', { index: 0, content_block: { type: 'redacted_thinking', data: 'redacted-private-fixture' } });
    send('content_block_stop', { index: 0 });
    send('message_delta', { delta: { stop_reason: 'end_turn', stop_sequence: null }, usage: { output_tokens: 5 } });
    send('message_stop', {}); response.end();
  });
  const result = await cli(f, p.url, ['--provider', 'anthropic', '--json', '--show-reasoning', '-p', 'answer']).done;
  const events = failedAnswer(result);
  assert(!events.some(e => e.type === 'reasoning'));
  assert(!result.stdout.includes('redacted-private-fixture'));
  const raw = await readFile(join(f.data, 'goon/sessions', `${events[0].id}.json`), 'utf8');
  assert(raw.includes('redacted-private-fixture'));
});

test('explicit permission denial retains its non-success stopped outcome even with no answer', async t => {
  const f = await fixture(t);
  const p = await provider(t, (_, response) => sse(response, { calls: [{ id: 'deny', name: 'write_file', input: { path: 'denied.txt', content: 'no' } }] }));
  const result = await cli(f, p.url, ['--json', '-p', 'write']).done;
  assert.equal(result.code, 2);
  const events = eventsOf(result);
  assert(events.some(e => e.type === 'stopped'));
  assert.equal(events.at(-1).denied, true);
  assert.equal(p.requests.length, 1);
  await assert.rejects(readFile(join(f.cwd, 'denied.txt')), { code: 'ENOENT' });
});
