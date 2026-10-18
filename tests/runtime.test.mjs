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

test('--effort serializes reasoning effort across openai-compatible, anthropic, and openai-responses and discoverModels queries /models', async t => {
  const { createServer } = await import('node:http');
  const { discoverModels } = await import('../dist/index.js');
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

  // 4. Live /models discovery
  const modelServer = createServer((req, res) => {
    assert.equal(req.url, '/v1/models');
    assert.equal(req.headers.authorization, 'Bearer disc-key');
    res.writeHead(200, { 'content-type': 'application/json' });
    res.end(JSON.stringify({
      data: [
        { id: 'live-model-alpha', context_window: 200000, max_output_tokens: 16384 },
        { id: 'live-model-beta', context_length: 65536 },
      ],
    }));
  });
  await new Promise(resolve => modelServer.listen(0, '127.0.0.1', resolve));
  t.after(() => new Promise(resolve => modelServer.close(resolve)));
  const discovered = await discoverModels(`http://127.0.0.1:${modelServer.address().port}/v1`, 'disc-key');
  assert.deepEqual(discovered, [
    { id: 'live-model-alpha', contextWindow: 200000, maxOutputTokens: 16384 },
    { id: 'live-model-beta', contextWindow: 65536 },
  ]);
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

test('Plan Mode blocks mutating tools until exit_plan_mode is approved, @file mentions pre-read files, and harness.revert restores both messages and source files', async t => {
  const { writeFile } = await import('node:fs/promises');
  const { Harness, SessionStore, ToolRegistry } = await import('../dist/index.js');
  const f = await fixture(t);
  await writeFile(join(f.cwd, 'app.ts'), 'export const v = 1;\n', 'utf8');

  const p = await provider(t, (body, response, n) => {
    const toolNames = (body.tools ?? []).map(tl => tl.function?.name);
    if (n === 1) {
      // In Plan Mode: write_file / edit_file / bash must be omitted, exit_plan_mode must be present
      assert(toolNames.includes('exit_plan_mode'));
      assert(!toolNames.includes('write_file'));
      assert(!toolNames.includes('edit_file'));
      // Verify @app.ts mention was automatically expanded in user prompt
      const userText = JSON.stringify(body.messages);
      assert.match(userText, /referenced_file[\s\S]*app\.ts/);
      return sse(response, {
        calls: [{ id: 'plan1', name: 'exit_plan_mode', input: { plan: '# Upgrade Plan\n1. Update app.ts\n2. Add extra.ts' } }],
      });
    }
    if (n === 2) {
      // After exit_plan_mode is approved in step 1, step 2 in the SAME turn has write_file & edit_file unlocked!
      assert(toolNames.includes('write_file'));
      assert(toolNames.includes('edit_file'));
      return sse(response, {
        calls: [
          {
            id: 'todo1',
            name: 'todo_write',
            input: {
              todos: [
                { content: 'Update app.ts to v=2', status: 'in_progress', priority: 'high' },
                { content: 'Create extra.ts', status: 'pending', priority: 'medium' },
              ],
            },
          },
          // Because @app.ts was mentioned in the prompt, it is already marked as read!
          { id: 'edit1', name: 'edit_file', input: { path: 'app.ts', oldText: 'export const v = 1;', newText: 'export const v = 2;' } },
          { id: 'write1', name: 'write_file', input: { path: 'extra.ts', content: 'export const extra = true;\n' } },
        ],
      });
    }
    if (n === 3) {
      return sse(response, { text: 'Turn 1 complete.' });
    }
    if (n === 4) {
      // Turn 2: edit app.ts again to v = 3
      return sse(response, {
        calls: [{ id: 'edit2', name: 'edit_file', input: { path: 'app.ts', oldText: 'export const v = 2;', newText: 'export const v = 3;' } }],
      });
    }
    return sse(response, { text: 'Turn 2 complete.' });
  });

  const config = {
    cwd: f.cwd,
    dataDir: f.data,
    provider: 'openai-compatible',
    model: 'test-model',
    baseUrl: p.url,
    apiKey: 'fake',
    keyEnv: 'GOON_API_KEY',
    contextWindow: 8192,
    maxOutputTokens: 512,
    timeoutMs: 5000,
    runTokenBudget: 100000,
    inlineReasoning: 'leading-think',
    showReasoning: false,
  };
  const store = await SessionStore.open(join(f.data, 'sessions'), config);
  t.after(() => store.close());

  const events = [];
  const tools = new ToolRegistry(f.cwd, async () => true, false);
  tools.planMode = true;
  const harness = new Harness(config, store, tools, e => events.push(e));

  // Run Turn 1 starting in Plan Mode with @app.ts mention
  const out1 = await harness.run('Update @app.ts and add extra.ts', new AbortController().signal);
  assert.equal(out1, 'Turn 1 complete.');
  assert.equal(tools.planMode, false);
  assert(events.some(e => e.type === 'plan_exit'));
  assert(events.some(e => e.type === 'todo' && e.todos.length === 2));
  assert.equal(await readFile(join(f.cwd, 'app.ts'), 'utf8'), 'export const v = 2;\n');
  assert.equal(await readFile(join(f.cwd, 'extra.ts'), 'utf8'), 'export const extra = true;\n');
  const messagesAfterTurn1 = store.session.messages.length;
  assert(messagesAfterTurn1 > 0);

  // Run Turn 2 modifying app.ts to v = 3
  const out2 = await harness.run('Bump app.ts to v=3', new AbortController().signal);
  assert.equal(out2, 'Turn 2 complete.');
  assert.equal(await readFile(join(f.cwd, 'app.ts'), 'utf8'), 'export const v = 3;\n');
  assert(store.session.messages.length > messagesAfterTurn1);
  assert.equal(store.session.checkpoints.length, 2);

  // Revert Turn 2 (both messages and source files)
  const rev2 = await harness.revert({ steps: 1, scope: 'both' });
  assert.equal(rev2.turnsReverted, 1);
  assert.deepEqual(rev2.restoredFiles, ['app.ts']);
  assert.equal(store.session.messages.length, messagesAfterTurn1);
  assert.equal(await readFile(join(f.cwd, 'app.ts'), 'utf8'), 'export const v = 2;\n');
  assert.equal(await readFile(join(f.cwd, 'extra.ts'), 'utf8'), 'export const extra = true;\n');

  // Revert Turn 1 (both messages and source files — restores app.ts to v=1 AND deletes newly created extra.ts!)
  const rev1 = await harness.revert({ steps: 1, scope: 'both' });
  assert.equal(rev1.turnsReverted, 1);
  assert.deepEqual(rev1.restoredFiles, ['app.ts']);
  assert.deepEqual(rev1.deletedFiles, ['extra.ts']);
  assert.equal(store.session.messages.length, 0);
  assert.equal(await readFile(join(f.cwd, 'app.ts'), 'utf8'), 'export const v = 1;\n');
  await assert.rejects(readFile(join(f.cwd, 'extra.ts'), 'utf8'));
});

test('AI Gateway ultra-compatibility: session affinity headers, 4 gateway models metadata, compaction effort downgrade, tool/param aliases, and @image.png vision vs beam filtering', async t => {
  const { createServer } = await import('node:http');
  const { writeFile } = await import('node:fs/promises');
  const {
    Harness,
    SessionStore,
    ToolRegistry,
    WorkspaceFiles,
    compactNow,
    createModel,
    discoverModels,
    expandFileMentions,
    formatModelBadge,
    normalizeGatewayUrl,
    resolveModelCapabilities,
  } = await import('../dist/index.js');
  const f = await fixture(t);

  // 1. Gateway URL normalization & 4 gateway models capability resolution
  assert.equal(normalizeGatewayUrl('https://api.yuristore.xyz/'), 'https://api.yuristore.xyz/v1');
  assert.equal(normalizeGatewayUrl('http://127.0.0.1:8000'), 'http://127.0.0.1:8000/v1');

  const gwServer = createServer((req, res) => {
    if (req.url === '/v1/models') {
      res.writeHead(200, { 'content-type': 'application/json' });
      res.end(JSON.stringify({
        data: [
          { id: 'glm-5.3-flash', context_window: 200000, max_output_tokens: 32000, supports_vision: true, reasoning_effort: 'max' },
          { id: 'deepseek-v4.1-flash', context_window: 200000, max_output_tokens: 32000, supports_vision: true, reasoning_effort: 'max' },
          { id: 'space-bunny', context_window: 1048576, max_output_tokens: 65536, supports_vision: true, supports_1m_context: true, reasoning_effort: 'max' },
          { id: 'beam', context_window: 1048576, max_output_tokens: 65536, supports_vision: false, supports_1m_context: true, reasoning_effort: 'max' },
        ],
      }));
      return;
    }
    res.writeHead(404).end();
  });
  await new Promise(resolve => gwServer.listen(0, '127.0.0.1', resolve));
  t.after(() => new Promise(resolve => gwServer.close(resolve)));
  const gwUrl = `http://127.0.0.1:${gwServer.address().port}/v1`;
  const models = await discoverModels(gwUrl, 'gw-key');
  assert.equal(models.length, 4);
  assert.equal(`${models[2].id}${formatModelBadge(models[2])}`, 'space-bunny (ctx: 1024k · out: 64k · vision · effort: max)');
  assert.equal(`${models[3].id}${formatModelBadge(models[3])}`, 'beam (ctx: 1024k · out: 64k · text-only · effort: max)');
  assert.deepEqual(resolveModelCapabilities('beam'), {
    id: 'beam',
    contextWindow: 1048576,
    maxOutputTokens: 65536,
    supportsVision: false,
    defaultEffort: 'max',
  });

  // 2. Multimodal @image.png expansion for vision models vs automatic text-only omission for 'beam'
  // 1x1 transparent PNG
  const pngBytes = Buffer.from(
    'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==',
    'base64',
  );
  await writeFile(join(f.cwd, 'shot.png'), pngBytes);
  const wsFiles = new WorkspaceFiles(f.cwd);

  const visionExpanded = await expandFileMentions('Check @shot.png please', wsFiles, { model: 'space-bunny' });
  assert.equal(visionExpanded.mentioned.includes('shot.png'), true);
  assert.equal(visionExpanded.imageParts.length, 1);
  assert.equal(visionExpanded.imageParts[0].type, 'image');
  assert.equal(visionExpanded.imageParts[0].mediaType, 'image/png');

  const beamExpanded = await expandFileMentions('Check @shot.png please', wsFiles, { model: 'beam' });
  assert.equal(beamExpanded.imageParts.length, 0);
  assert.match(beamExpanded.text, /\[Attached image @shot\.png omitted: model "beam" is text-only\]/);

  // 3. Session Affinity Headers (x-session-id & x-opencode-session), Tool/Param Aliases, and Compaction Effort Downgrade
  await writeFile(join(f.cwd, 'hello.txt'), 'line one\n', 'utf8');
  const observedHeaders = [];
  const observedEfforts = [];
  const p = await provider(t, (body, response, n, req) => {
    observedHeaders.push({
      sessionId: req.headers['x-session-id'],
      opencodeSession: req.headers['x-opencode-session'],
    });
    observedEfforts.push(body.reasoning_effort);
    if (n === 1) {
      // Model uses tool alias 'read' with parameter alias 'filePath'
      return sse(response, {
        calls: [{
          id: 'c_alias_1',
          name: 'read',
          input: { filePath: 'hello.txt' },
        }],
      });
    }
    if (n === 2) {
      // Model uses tool alias 'edit' with parameter aliases 'file_path', 'old_string', 'new_string'
      return sse(response, {
        calls: [{
          id: 'c_alias_2',
          name: 'edit',
          input: { file_path: 'hello.txt', old_string: 'line one', new_string: 'line two' },
        }],
      });
    }
    if (n === 3) {
      return sse(response, { text: 'Aliases and session headers verified.' });
    }
    // Request 4: Compaction request (no tools, compaction system prompt) -> effort must be downgraded to 'low'!
    response.writeHead(200, { 'content-type': 'application/json' });
    response.end(JSON.stringify({
      id: 'summary',
      object: 'chat.completion',
      created: 1,
      model: 'space-bunny',
      choices: [{
        index: 0,
        message: { role: 'assistant', content: 'Clean checkpoint summary of gateway test.' },
        finish_reason: 'stop',
      }],
      usage: { prompt_tokens: 40, completion_tokens: 15, total_tokens: 55 },
    }));
  });

  const config = {
    provider: 'openai-compatible',
    baseUrl: p.url,
    model: 'space-bunny',
    effort: 'max',
    keyEnv: 'GOON_API_KEY',
    apiKey: 'test-secret-never-echo',
    cwd: f.cwd,
    contextWindow: 1048576,
    maxOutputTokens: 65536,
    runTokenBudget: 1000000,
    timeoutMs: 30000,
    maxTurns: 8,
    inlineReasoning: 'leading-think',
    showReasoning: false,
  };
  const store = await SessionStore.open(join(f.data, 'sessions'), config);
  t.after(() => store.close());
  const tools = new ToolRegistry(f.cwd, async () => true, false);
  const harness = new Harness(config, store, tools);

  const answer = await harness.run('Edit @shot.png and hello.txt', new AbortController().signal);
  assert.equal(answer, 'Aliases and session headers verified.');
  assert.equal(await readFile(join(f.cwd, 'hello.txt'), 'utf8'), 'line two\n');

  // Verify x-session-id & x-opencode-session headers matched store.session.id on all turn requests
  for (const h of observedHeaders) {
    assert.equal(h.sessionId, store.session.id);
    assert.equal(h.opencodeSession, store.session.id);
  }
  assert.deepEqual(observedEfforts.slice(0, 3), ['max', 'max', 'max']);

  // Verify compaction downgrades reasoning_effort from 'max' to 'low' so reasoning doesn't exhaust maxOutputTokens
  const model = createModel(config, store.session.id);
  const compacted = await compactNow(store, config, model, new AbortController().signal);
  assert.equal(compacted.compactions, 1);
  assert.equal(observedEfforts[3], 'low');
});

test('Harness retries transient HTTP 500 API errors up to 3 times and SessionStore.open ignores 0-message empty sessions on resume', async t => {
  const { join } = await import('node:path');
  const { Harness, SessionStore, ToolRegistry } = await import('../dist/index.js');
  const f = await fixture(t);
  let attempts = 0;
  const p = await provider(t, (_, response) => {
    attempts++;
    if (attempts <= 3) {
      response.writeHead(500, { 'content-type': 'application/json' });
      response.end(JSON.stringify({ error: { message: `Transient upstream 500 on attempt #${attempts}` } }));
      return;
    }
    return sse(response, { text: 'Recovered on retry #3 (attempt #4).' });
  });

  const config = {
    provider: 'openai-compatible',
    baseUrl: p.url,
    model: 'test-model',
    keyEnv: 'GOON_API_KEY',
    apiKey: 'test-secret-never-echo',
    cwd: f.cwd,
    contextWindow: 32000,
    maxOutputTokens: 1024,
    runTokenBudget: 100000,
    timeoutMs: 10000,
    maxTurns: 4,
    inlineReasoning: 'leading-think',
    showReasoning: false,
  };

  const sessionsRoot = join(f.data, 'sessions');
  const s1 = await SessionStore.open(sessionsRoot, config);
  const retryEvents = [];
  const tools = new ToolRegistry(f.cwd, async () => true, false);
  const harness = new Harness(config, s1, tools, ev => {
    if (ev.type === 'retry') retryEvents.push(ev);
  });

  const answer = await harness.run('Test API retry', new AbortController().signal);
  assert.equal(answer, 'Recovered on retry #3 (attempt #4).');
  assert.equal(attempts, 4);
  assert.equal(retryEvents.length, 3);
  assert.deepEqual(retryEvents.map(r => r.attempt), [1, 2, 3]);
  assert.equal(retryEvents[0].maxRetries, 3);
  await s1.close();

  // Create a newer 0-message empty session (simulating starting `goon` before running `/resume` or `goon resume`)
  await new Promise(resolve => setTimeout(resolve, 15));
  const emptySession = await SessionStore.open(sessionsRoot, config);
  assert.equal(emptySession.session.messages.length, 0);
  await emptySession.close();

  // Resuming 'latest' or '1' must pick s1 (which has messages), NOT emptySession!
  const resumedLatest = await SessionStore.open(sessionsRoot, config, 'latest');
  assert.equal(resumedLatest.session.id, s1.session.id);
  assert.equal(resumedLatest.session.messages.length, 2);
  await resumedLatest.close();

  const resumedOne = await SessionStore.open(sessionsRoot, config, '1');
  assert.equal(resumedOne.session.id, s1.session.id);
  await resumedOne.close();

  // And discardIfEmpty deletes the 0-message empty session file from disk
  const reOpenedEmpty = await SessionStore.open(sessionsRoot, config, emptySession.session.id);
  await reOpenedEmpty.discardIfEmpty();
  await reOpenedEmpty.close();
  const remaining = await SessionStore.list(sessionsRoot);
  assert.equal(remaining.length, 1);
  assert.equal(remaining[0].id, s1.session.id);

  // Verify loadConfig defaults runTokenBudget to 0 (unlimited) and migrates legacy saved defaults
  const { loadConfig } = await import('../dist/index.js');
  const { mkdir, writeFile } = await import('node:fs/promises');
  const oldCfgHome = process.env.XDG_CONFIG_HOME;
  process.env.XDG_CONFIG_HOME = f.config;
  try {
    await mkdir(join(f.config, 'goon'), { recursive: true });
    await writeFile(join(f.config, 'goon', 'config.json'), JSON.stringify({ model: 'gpt-5.4', runTokenBudget: 120000 }), 'utf8');
    const migratedCfg = await loadConfig();
    assert.equal(migratedCfg.runTokenBudget, 0, 'Legacy 120000 runTokenBudget should migrate to 0 (unlimited)');
  } finally {
    if (oldCfgHome === undefined) delete process.env.XDG_CONFIG_HOME;
    else process.env.XDG_CONFIG_HOME = oldCfgHome;
  }
});





