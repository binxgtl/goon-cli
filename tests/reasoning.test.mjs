import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile, writeFile } from 'node:fs/promises';
import { join } from 'node:path';
import { AnswerProjection, visibleAnswer } from '../dist/reasoning.js';
import { fixture, provider, sse, cli } from './helpers.mjs';

function project(chunks, policy) {
  const projection = new AnswerProjection(policy);
  const parts = chunks.flatMap(text => projection.push(text)); parts.push(...projection.finish());
  return { answer: parts.filter(p => p.type === 'text').map(p => p.text).join(''), reasoning: parts.filter(p => p.type === 'reasoning').map(p => p.text).join('') };
}

for (const [name, input, answer, reasoning] of [
  ['split markers and Unicode', '\n<think>ẩn 🐧\nsecond line</think>Answer ✓', 'Answer ✓', 'ẩn 🐧\nsecond line'],
  ['multiple leading blocks', '<think>one</think>\n<THINK>two</THINK>\nAnswer', '\nAnswer', 'onetwo'],
  ['nested blocks', '<think>outer<think>inner</think>end</think>Answer', 'Answer', 'outerinnerend'],
  ['unclosed block', '<think>private</thi', '', 'private</thi'],
  ['empty reasoning', '<think></think>Answer', 'Answer', ''],
  ['literal code fence', '```xml\n<think>literal</think>\n```', '```xml\n<think>literal</think>\n```', ''],
  ['literal later in answer', 'Use `<think>...</think>` literally.', 'Use `<think>...</think>` literally.', ''],
  ['partial opening at clean end', '  <thi', '  <thi', ''],
  ['no markers, exact whitespace', '\n  ordinary < than\n', '\n  ordinary < than\n', ''],
]) {
  test(`inline reasoning projection: ${name}, invariant across every chunk boundary`, () => {
    for (let cut = 0; cut <= input.length; cut++) assert.deepEqual(project([input.slice(0, cut), input.slice(cut)]), { answer, reasoning });
    assert.deepEqual(project([...input]), { answer, reasoning });
  });
}

test('inline extraction off preserves literal leading tags', () => {
  const raw = '<think>literal document element</think> body';
  assert.equal(visibleAnswer(raw, 'off'), raw);
});

test('large token-sized reasoning stream is discarded without losing the final answer', () => {
  const p = new AnswerProjection();
  assert.deepEqual(p.push('<think>'), []);
  for (let i = 0; i < 20_000; i++) assert.deepEqual(p.push('x'), [{ type: 'reasoning', text: 'x' }]);
  assert.deepEqual(p.push('</think>done'), [{ type: 'text', text: 'done' }]);
});

test('real CLI hides split inline/native reasoning and preserves original provider history through tools and resume', async t => {
  const f = await fixture(t);
  await writeFile(join(f.cwd, 'input.txt'), 'data');
  const raw = '<think>inline-private</think>Inspecting.';
  const p = await provider(t, (body, response, n) => {
    if (n === 1) return sse(response, { textChunks: [...raw], reasoning: 'native-private', calls: [{ id: 'read1', name: 'read_file', input: { path: 'input.txt' } }] });
    const original = body.messages.find(m => m.role === 'assistant' && m.tool_calls);
    assert.equal(original.content, raw);
    assert.equal(original.reasoning_content, 'native-private');
    return sse(response, { textChunks: [...'<think>final-private</think>Done.'] });
  });
  const first = await cli(f, p.url, ['--json', '-p', 'inspect']).done;
  assert.equal(first.code, 0, first.stderr + first.stdout);
  assert(!first.stdout.includes('private'));
  const events = first.stdout.trim().split('\n').map(JSON.parse);
  assert.equal(events.at(-1).text, 'Inspecting.Done.');
  assert.equal(events.filter(e => e.type === 'reasoning').length, 0);
  const id = events[0].id;
  const stored = await readFile(join(f.data, 'goon/sessions', `${id}.json`), 'utf8');
  assert(stored.includes(raw)); assert(stored.includes('native-private'));
  const resumed = await cli(f, p.url, ['--json', '--resume', id, '-p', 'continue']).done;
  assert.equal(resumed.code, 0, resumed.stderr + resumed.stdout);
  assert(!resumed.stdout.includes('private'));
});

test('--show-reasoning uses separate JSON events and leaves done.text answer-only', async t => {
  const f = await fixture(t);
  const p = await provider(t, (_, response) => sse(response, { reasoning: 'native detail', textChunks: [...'<think>inline detail</think>Answer'] }));
  const result = await cli(f, p.url, ['--json', '--show-reasoning', '-p', 'inspect']).done;
  assert.equal(result.code, 0, result.stderr);
  const events = result.stdout.trim().split('\n').map(JSON.parse);
  assert.equal(events.filter(e => e.type === 'reasoning' && e.source === 'inline').map(e => e.text).join(''), 'inline detail');
  assert.equal(events.filter(e => e.type === 'reasoning' && e.source === 'provider').map(e => e.text).join(''), 'native detail');
  assert.equal(events.filter(e => e.type === 'text').map(e => e.text).join(''), 'Answer');
  assert.equal(events.at(-1).text, 'Answer');
  const plain = await cli(f, p.url, ['--show-reasoning', '-p', 'inspect']).done;
  assert.equal(plain.stdout, 'Answer\n');
  assert.match(plain.stderr, /\[reasoning · provider\]/); assert.match(plain.stderr, /\[reasoning · inline\]/); assert.match(plain.stderr, /inline detail/);
});

test('--inline-reasoning off preserves a literal leading element', async t => {
  const f = await fixture(t), raw = '<think>literal</think> document';
  const p = await provider(t, (_, response) => sse(response, { text: raw }));
  const result = await cli(f, p.url, ['--inline-reasoning', 'off', '-p', 'return document']).done;
  assert.equal(result.code, 0, result.stderr); assert.equal(result.stdout, raw + '\n');
});

test('reasoning-only and truncated streams never leak reasoning into text or execute tools', async t => {
  const f = await fixture(t);
  let kind = 'only';
  const p = await provider(t, (_, response) => sse(response, { textChunks: [...'<think>private payload</thi'],
    ...(kind === 'length' ? { finishReason: 'length', calls: [{ id: 'no-write', name: 'write_file', input: { path: 'never.txt', content: 'no' } }] } : {}) }));
  const only = await cli(f, p.url, ['--json', '--yes', '-p', 'answer']).done;
  assert.equal(only.code, 1); assert.match(only.stdout, /without a visible answer/); assert(!only.stdout.includes('private payload'));
  kind = 'length';
  const length = await cli(f, p.url, ['--json', '--yes', '-p', 'write']).done;
  assert.equal(length.code, 1); assert.match(length.stdout, /output limit/); assert(!length.stdout.includes('private payload'));
  await assert.rejects(readFile(join(f.cwd, 'never.txt')), { code: 'ENOENT' });
});

test('cancellation during inline reasoning does not flush a hidden fragment', async t => {
  const f = await fixture(t);
  let ready; const started = new Promise(resolve => { ready = resolve; });
  const p = await provider(t, (_, response) => {
    response.writeHead(200, { 'content-type': 'text/event-stream' });
    response.write(`data: ${JSON.stringify({ id: 'wait', choices: [{ index: 0, delta: { content: '<think>cancel-private</thi' }, finish_reason: null }] })}\n\n`);
    ready();
  });
  const running = cli(f, p.url, ['--json', '-p', 'wait']);
  await started; running.child.kill('SIGINT');
  const result = await running.done;
  assert.equal(result.code, 130, result.stderr + result.stdout); assert(!result.stdout.includes('cancel-private'));
});

test('Anthropic thinking signatures and raw inline text survive the next tool request', async t => {
  const f = await fixture(t), raw = '<think>inline private</think>Inspecting.';
  const p = await provider(t, (body, response, n) => {
    response.writeHead(200, { 'content-type': 'text/event-stream' });
    const send = (type, data) => response.write(`event: ${type}\ndata: ${JSON.stringify({ type, ...data })}\n\n`);
    send('message_start', { message: { id: `msg_${n}`, type: 'message', role: 'assistant', model: 'test-model', content: [], stop_reason: null, stop_sequence: null, usage: { input_tokens: 10, output_tokens: 0 } } });
    if (n === 1) {
      send('content_block_start', { index: 0, content_block: { type: 'thinking', thinking: '' } });
      send('content_block_delta', { index: 0, delta: { type: 'thinking_delta', thinking: 'native private' } });
      send('content_block_delta', { index: 0, delta: { type: 'signature_delta', signature: 'signature-fixture-unchanged' } });
      send('content_block_stop', { index: 0 });
      send('content_block_start', { index: 1, content_block: { type: 'text', text: '' } });
      for (const text of raw) send('content_block_delta', { index: 1, delta: { type: 'text_delta', text } });
      send('content_block_stop', { index: 1 });
      send('content_block_start', { index: 2, content_block: { type: 'tool_use', id: 'list-signed', name: 'list_files', input: {} } });
      send('content_block_stop', { index: 2 });
    } else {
      const original = body.messages.find(m => m.role === 'assistant');
      assert.deepEqual(original.content.find(p => p.type === 'thinking'), { type: 'thinking', thinking: 'native private', signature: 'signature-fixture-unchanged' });
      assert.equal(original.content.find(p => p.type === 'text').text, raw);
      send('content_block_start', { index: 0, content_block: { type: 'text', text: '' } });
      send('content_block_delta', { index: 0, delta: { type: 'text_delta', text: 'Done.' } });
      send('content_block_stop', { index: 0 });
    }
    send('message_delta', { delta: { stop_reason: n === 1 ? 'tool_use' : 'end_turn', stop_sequence: null }, usage: { output_tokens: 20 } });
    send('message_stop', {}); response.end();
  });
  const result = await cli(f, p.url, ['--provider', 'anthropic', '--json', '-p', 'inspect']).done;
  assert.equal(result.code, 0, result.stderr + result.stdout);
  assert(!result.stdout.includes('private')); assert(!result.stdout.includes('signature-fixture'));
  const events = result.stdout.trim().split('\n').map(JSON.parse);
  assert.equal(events.at(-1).text, 'Inspecting.Done.');
  const transcript = await readFile(join(f.data, 'goon/sessions', `${events[0].id}.json`), 'utf8');
  assert(transcript.includes('signature-fixture-unchanged')); assert(transcript.includes(raw));
});

// Mock of a DeepSeek/GLM-style thinking endpoint: every assistant message replayed in thinking mode must
// carry `reasoning_content`, otherwise HTTP 400 with the upstream error text the user hit.
function thinkingModeViolation(body) {
  const bad = body.messages.findIndex(m => m.role === 'assistant' && typeof m.reasoning_content !== 'string');
  return bad === -1 ? undefined : bad;
}
function rejectLikeThinkingEndpoint(response) {
  response.writeHead(400, { 'content-type': 'application/json' });
  response.end(JSON.stringify({ error: { type: 'invalid_request_error', message: 'The reasoning_content in the thinking mode must be passed back to the API.' } }));
}

test('thinking mode: real reasoning is replayed verbatim and reasoning-less tool steps still carry reasoning_content', async t => {
  const f = await fixture(t);
  await writeFile(join(f.cwd, 'a.txt'), 'A');
  await writeFile(join(f.cwd, 'b.txt'), 'B');
  const p = await provider(t, (body, response, n) => {
    if (thinkingModeViolation(body) !== undefined) return rejectLikeThinkingEndpoint(response);
    // Step 1 thinks, step 2 calls a tool without emitting any reasoning (seen with GLM/DeepSeek quick steps).
    if (n === 1) return sse(response, { reasoning: 'Need a.txt first, then b.txt.', calls: [{ id: 'r1', name: 'read_file', input: { path: 'a.txt' } }] });
    if (n === 2) return sse(response, { calls: [{ id: 'r2', name: 'read_file', input: { path: 'b.txt' } }] });
    const assistants = body.messages.filter(m => m.role === 'assistant');
    assert.equal(assistants.length, 2);
    // The model's own chain of thought comes back unchanged — never a placeholder like "Thinking complete."
    assert.equal(assistants[0].reasoning_content, 'Need a.txt first, then b.txt.');
    assert.equal(assistants[1].reasoning_content, '');
    return sse(response, { text: 'Read both.' });
  });
  const run = await cli(f, p.url, ['--json', '-p', 'read both files']).done;
  assert.equal(run.code, 0, run.stderr + run.stdout);
  assert.equal(p.requests.length, 3);
  const stored = await readFile(join(f.data, 'goon/sessions', `${JSON.parse(run.stdout.split('\n')[0]).id}.json`), 'utf8');
  assert(!stored.includes('reasoning_content'), 'the empty marker is request-only; stored history stays the provider original');
});

test('non-thinking endpoints never receive a reasoning_content field', async t => {
  const f = await fixture(t);
  await writeFile(join(f.cwd, 'a.txt'), 'A');
  const p = await provider(t, (body, response, n) => {
    if (n === 1) return sse(response, { calls: [{ id: 'r1', name: 'read_file', input: { path: 'a.txt' } }] });
    const assistant = body.messages.find(m => m.role === 'assistant');
    assert(!('reasoning_content' in assistant), 'no reasoning was ever produced, so none is sent');
    return sse(response, { text: 'Done.' });
  });
  const run = await cli(f, p.url, ['--json', '-p', 'read a']).done;
  assert.equal(run.code, 0, run.stderr + run.stdout);
});

test('thinking mode with explicit --effort attaches empty reasoning_content even when step 1 emitted no reasoning', async t => {
  const f = await fixture(t);
  await writeFile(join(f.cwd, 'a.txt'), 'A');
  const p = await provider(t, (body, response, n) => {
    if (thinkingModeViolation(body) !== undefined) return rejectLikeThinkingEndpoint(response);
    if (n === 1) return sse(response, { calls: [{ id: 'r1', name: 'read_file', input: { path: 'a.txt' } }] });
    const assistant = body.messages.find(m => m.role === 'assistant');
    assert.equal(assistant.reasoning_content, '');
    return sse(response, { reasoning: 'Checked a.txt.', text: 'Done.' });
  });
  const run = await cli(f, p.url, ['--json', '--effort', 'high', '-p', 'read a']).done;
  assert.equal(run.code, 0, run.stderr + run.stdout);
});

