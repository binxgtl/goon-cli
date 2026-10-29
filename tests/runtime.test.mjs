import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile, readdir } from 'node:fs/promises';
import { join } from 'node:path';
import { fixture, provider, sse, cli } from './helpers.mjs';

test('real CLI streams tool arguments, writes, resumes and preserves reasoning through edit', async t => {
  const f = await fixture(t);
  const p = await provider(t, (body, response, n) => {
    if (n === 1) return sse(response, { reasoning: 'Inspect and make the requested change.', calls: [{ id: 'write1', name: 'write_file', input: { path: 'hello.txt', content: 'hello\n' } }] });
    if (n === 2) {
      assert.equal(body.messages.at(-1).role, 'tool');
      assert.equal(body.messages.find(m => m.tool_calls)?.reasoning_content, 'Inspect and make the requested change.');
      return sse(response, { text: 'Created hello.txt.' });
    }
    if (n === 3) {
      assert(body.messages.some(m => m.content === 'Created hello.txt.'));
      return sse(response, { calls: [{ id: 'read1', name: 'read_file', input: { path: 'hello.txt' } }] });
    }
    if (n === 4) return sse(response, { calls: [{ id: 'edit1', name: 'edit_file', input: { path: 'hello.txt', oldText: 'hello', newText: 'goodbye' } }] });
    return sse(response, { text: 'Edited hello.txt.' });
  });
  const first = await cli(f, p.url, ['--yes', '--json', '-p', 'Create hello.txt']).done;
  assert.equal(first.code, 0, first.stderr + first.stdout);
  const events = first.stdout.trim().split('\n').map(JSON.parse);
  assert.equal(events[0].type, 'session');
  assert.equal(events.at(-1).text, 'Created hello.txt.');
  assert.equal(await readFile(join(f.cwd, 'hello.txt'), 'utf8'), 'hello\n');
  const second = await cli(f, p.url, ['--yes', '--resume', events[0].id, '-p', 'Change hello to goodbye']).done;
  assert.equal(second.code, 0, second.stderr);
  assert.equal(await readFile(join(f.cwd, 'hello.txt'), 'utf8'), 'goodbye\n');
  assert.equal(p.requests.length, 5);
  const session = JSON.parse(await readFile(join(f.data, 'goon/sessions', `${events[0].id}.json`), 'utf8'));
  assert.equal(session.tokens, 550);
  assert(!JSON.stringify(session).includes('test-secret-never-echo'));
  assert.deepEqual((await readdir(join(f.data, 'goon/sessions'))).filter(n => n.endsWith('.lock')), []);
});

test('noninteractive mutations fail closed and end the run', async t => {
  const f = await fixture(t);
  const p = await provider(t, (_, response) => sse(response, { calls: [{ id: 'deny1', name: 'write_file', input: { path: 'denied.txt', content: 'no' } }] }));
  const result = await cli(f, p.url, ['-p', 'write a file']).done;
  assert.equal(result.code, 2, result.stderr);
  assert.equal(p.requests.length, 1);
  await assert.rejects(readFile(join(f.cwd, 'denied.txt')), { code: 'ENOENT' });
});

test('read-only omits bash, edit and write from the model tool surface', async t => {
  const f = await fixture(t);
  const p = await provider(t, (body, response) => {
    assert.deepEqual(body.tools.map(t => t.function.name).sort(), ['delegate_task', 'list_files', 'read_file', 'search']);
    return sse(response, { text: 'Read-only ready.' });
  });
  const result = await cli(f, p.url, ['--read-only', '-p', 'inspect']).done;
  assert.equal(result.code, 0, result.stderr);
});

test('authentication errors fail without leaking the key or retrying 401', async t => {
  const f = await fixture(t);
  const p = await provider(t, (_, response) => {
    response.writeHead(401, { 'content-type': 'application/json' });
    response.end(JSON.stringify({ error: { message: 'Invalid key test-secret-never-echo', type: 'authentication_error' } }));
  });
  const result = await cli(f, p.url, ['-p', 'hello']).done;
  assert.equal(result.code, 1);
  assert(!result.stdout.includes('test-secret-never-echo'));
  assert(!result.stderr.includes('test-secret-never-echo'));
  assert.equal(p.requests.length, 1);
});

test('SIGINT cancels HTTP and releases the session lock', async t => {
  const f = await fixture(t);
  let received;
  const ready = new Promise(resolve => { received = resolve; });
  const p = await provider(t, (_, response) => {
    response.writeHead(200, { 'content-type': 'text/event-stream' });
    response.write(': waiting\n\n');
    received();
  });
  const running = cli(f, p.url, ['--json', '-p', 'wait']);
  await ready;
  running.child.kill('SIGINT');
  const result = await running.done;
  assert.equal(result.code, 130, result.stderr + result.stdout);
  assert.deepEqual((await readdir(join(f.data, 'goon/sessions'))).filter(n => n.endsWith('.lock')), []);
});

test('incomplete response never executes a partially streamed tool', async t => {
  const f = await fixture(t);
  const p = await provider(t, (_, response) => {
    response.writeHead(200, { 'content-type': 'text/event-stream' });
    response.end('data: {"id":"x","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"partial","type":"function","function":{"name":"write_file","arguments":"{\\"path\\":\\"oops.txt\\""}}]},"finish_reason":null}]}\n\n');
  });
  const result = await cli(f, p.url, ['--yes', '-p', 'write']).done;
  assert.equal(result.code, 1, result.stderr);
  await assert.rejects(readFile(join(f.cwd, 'oops.txt')), { code: 'ENOENT' });
});

test('Anthropic Messages streams through the same independent harness', async t => {
  const f = await fixture(t);
  const p = await provider(t, (body, response) => {
    assert.equal(body.model, 'test-model');
    response.writeHead(200, { 'content-type': 'text/event-stream' });
    const send = (type, data) => response.write(`event: ${type}\ndata: ${JSON.stringify({ type, ...data })}\n\n`);
    send('message_start', { message: { id: 'msg_test', type: 'message', role: 'assistant', model: 'test-model', content: [], stop_reason: null, stop_sequence: null, usage: { input_tokens: 10, output_tokens: 0 } } });
    send('content_block_start', { index: 0, content_block: { type: 'text', text: '' } });
    send('content_block_delta', { index: 0, delta: { type: 'text_delta', text: 'Anthropic works.' } });
    send('content_block_stop', { index: 0 });
    send('message_delta', { delta: { stop_reason: 'end_turn', stop_sequence: null }, usage: { output_tokens: 5 } });
    send('message_stop', {});
    response.end();
  });
  const result = await cli(f, p.url, ['--provider', 'anthropic', '-p', 'hello']).done;
  assert.equal(result.code, 0, result.stderr);
  assert(result.stdout.includes('Anthropic works.'));
});

test('OpenAI Responses streams through the same independent harness', async t => {
  const f = await fixture(t);
  const p = await provider(t, (body, response) => {
    assert.equal(body.model, 'test-model');
    assert(Array.isArray(body.input));
    response.writeHead(200, { 'content-type': 'text/event-stream' });
    const send = (type, data) => response.write(`event: ${type}\ndata: ${JSON.stringify({ type, ...data })}\n\n`);
    send('response.created', { response: { id: 'resp_test', created_at: 1, model: 'test-model' } });
    send('response.output_item.added', { output_index: 0, item: { type: 'message', id: 'msg_1' } });
    send('response.output_text.delta', { item_id: 'msg_1', delta: 'Responses works.' });
    send('response.output_item.done', { output_index: 0, item: { type: 'message', id: 'msg_1' } });
    send('response.completed', { response: { usage: { input_tokens: 10, output_tokens: 5 } } });
    response.end();
  });
  const result = await cli(f, p.url, ['--provider', 'openai-responses', '-p', 'hello']).done;
  assert.equal(result.code, 0, result.stderr);
  assert(result.stdout.includes('Responses works.'));
});

test('init saves only configuration, doctor probes /models without calling completions', async t => {
  const f = await fixture(t);
  const p = await provider(t, () => { throw new Error('Doctor must not call chat completions'); });
  const init = await cli(f, p.url, ['init']).done;
  assert.equal(init.code, 0, init.stderr);
  const saved = await readFile(join(f.config, 'goon/config.json'), 'utf8');
  assert(!saved.includes('test-secret-never-echo'));
  assert.equal(JSON.parse(saved).model, 'test-model');
  const doctor = await cli(f, p.url, ['doctor']).done;
  assert.equal(doctor.code, 0, doctor.stderr);
  const docJson = JSON.parse(doctor.stdout);
  assert.equal(docJson.networkTested, true);
  assert(Array.isArray(docJson.discoveredModels));
  assert.equal(p.requests.length, 0);
});

test('invalid config is rejected before network I/O', async t => {
  const f = await fixture(t);
  const p = await provider(t, () => { throw new Error('Invalid config reached provider'); });
  const result = await cli(f, p.url, ['--context-window', '-1', '-p', 'hello']).done;
  assert.equal(result.code, 1);
  assert.equal(p.requests.length, 0);
});

test('--effort serializes reasoning effort across openai-compatible, anthropic, and openai-responses', async t => {
  const f = await fixture(t);

  // 1. OpenAI-compatible effort='xhigh' -> body.reasoning_effort === 'xhigh'
  const pCompat = await provider(t, (body, response) => {
    assert.equal(body.reasoning_effort, 'xhigh');
    return sse(response, { text: 'Compat xhigh ok.' });
  });
  const resCompat = await cli(f, pCompat.url, ['--effort', 'xhigh', '-p', 'test effort']).done;
  assert.equal(resCompat.code, 0, resCompat.stderr);
  assert(resCompat.stdout.includes('Compat xhigh ok.'));

  // 2. Anthropic effort='max' -> body.output_config.effort === 'max'
  const pAnthropic = await provider(t, (body, response) => {
    assert.deepEqual(body.output_config, { effort: 'max' });
    response.writeHead(200, { 'content-type': 'text/event-stream' });
    const send = (type, data) => response.write(`event: ${type}\ndata: ${JSON.stringify({ type, ...data })}\n\n`);
    send('message_start', { message: { id: 'msg_eff', type: 'message', role: 'assistant', model: 'test-model', content: [], stop_reason: null, stop_sequence: null, usage: { input_tokens: 10, output_tokens: 0 } } });
    send('content_block_start', { index: 0, content_block: { type: 'text', text: '' } });
    send('content_block_delta', { index: 0, delta: { type: 'text_delta', text: 'Anthropic max effort ok.' } });
    send('content_block_stop', { index: 0 });
    send('message_delta', { delta: { stop_reason: 'end_turn', stop_sequence: null }, usage: { output_tokens: 5 } });
    send('message_stop', {});
    response.end();
  });
  const resAnth = await cli(f, pAnthropic.url, ['--provider', 'anthropic', '--effort', 'max', '-p', 'test max']).done;
  assert.equal(resAnth.code, 0, resAnth.stderr);
  assert(resAnth.stdout.includes('Anthropic max effort ok.'));

  // 3. OpenAI Responses effort='high' -> body.reasoning.effort === 'high'
  const pResp = await provider(t, (body, response) => {
    assert.equal(body.reasoning?.effort, 'high');
    response.writeHead(200, { 'content-type': 'text/event-stream' });
    const send = (type, data) => response.write(`event: ${type}\ndata: ${JSON.stringify({ type, ...data })}\n\n`);
    send('response.created', { response: { id: 'resp_eff', created_at: 1, model: 'test-model' } });
    send('response.output_item.added', { output_index: 0, item: { type: 'message', id: 'msg_1' } });
    send('response.output_text.delta', { item_id: 'msg_1', delta: 'Responses high effort ok.' });
    send('response.output_item.done', { output_index: 0, item: { type: 'message', id: 'msg_1' } });
    send('response.completed', { response: { usage: { input_tokens: 10, output_tokens: 5 } } });
    response.end();
  });
  const resResp = await cli(f, pResp.url, ['--provider', 'openai-responses', '--effort', 'high', '-p', 'test high']).done;
  assert.equal(resResp.code, 0, resResp.stderr);
  assert(resResp.stdout.includes('Responses high effort ok.'));
});

test('repeat-tool-call loop detector warns the model on 3 consecutive identical tool calls', async t => {
  const f = await fixture(t);
  const p = await provider(t, (body, response, n) => {
    if (n <= 3) {
      return sse(response, { calls: [{ id: `call_${n}`, name: 'list_files', input: { path: '.' } }] });
    }
    const lastToolMsg = body.messages.filter(m => m.role === 'tool').at(-1);
    const contentStr = JSON.stringify(lastToolMsg);
    assert.match(contentStr, /Loop warning: list_files was called 3 times consecutively/);
    return sse(response, { text: 'Broke out of loop.' });
  });
  const res = await cli(f, p.url, ['--read-only', '-p', 'check files']).done;
  assert.equal(res.code, 0, res.stderr);
  assert(res.stdout.includes('Broke out of loop.'));
});
