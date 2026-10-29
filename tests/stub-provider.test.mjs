import assert from 'node:assert/strict';
import { test } from 'node:test';
import { mkdtemp, rm, realpath } from 'node:fs/promises';
import { readdirSync, readFileSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { fixture, stubCli } from './helpers.mjs';
import { APICallError } from 'ai';
import { loadStubFixture, stubFixtureSchema } from '../dist/stub-fixtures.js';
import { createStubModel } from '../dist/stub-provider.js';
import { resolveRuntime } from '../dist/config.js';

test('stub-provider: totalTokens-only fixture moves usage budgets', async () => {
  const onlyTotal = await createStubModel({ v: 1, turns: [{ text: ['x'], usage: { totalTokens: 100 } }] })
    .doGenerate(callOptions('x'));
  assert.equal(onlyTotal.usage.inputTokens.total, 0);
  assert.equal(onlyTotal.usage.outputTokens.total, 100);
  const withInput = await createStubModel({ v: 1, turns: [{ text: ['x'], usage: { inputTokens: 40, totalTokens: 100 } }] })
    .doGenerate(callOptions('x'));
  assert.equal(withInput.usage.inputTokens.total, 40);
  assert.equal(withInput.usage.outputTokens.total, 60);
});

test('stub-provider: resolveRuntime keeps one cursor across config reloads', async t => {
  const f = await fixture(t);
  const base = { stub: true, stubFixture: join(f.root, 'fix.json'), keyEnv: 'GOON_API_KEY', model: 'stub-model', baseUrl: 'stub://local' };
  const first = await resolveRuntime({ ...base }, f.cwd);
  const second = await resolveRuntime({ ...base }, f.cwd);
  assert.equal(first.stubCursor, second.stubCursor);
  assert.equal(first.model, 'stub-model');
  assert.equal(first.baseUrl, 'stub://local');
});

function callOptions(text) {
  return {
    prompt: [{ role: 'user', content: [{ type: 'text', text }] }],
  };
}

test('stub-provider: CLI --stub runs without model/base-url/API key and writes the log', async t => {
  const f = await fixture(t);
  const fixtureFile = join(f.root, 'fix.json');
  writeFileSync(fixtureFile, JSON.stringify({
    v: 1,
    turns: [{ text: ['Stub answer one.'], usage: { inputTokens: 21, outputTokens: 9, totalTokens: 30 } }],
  }));
  const run = await stubCli(f, ['--no-mcp', '--no-subagents', '--event-log', '-p', 'hi', '--stub-fixture', fixtureFile]).done;
  assert.equal(run.code, 0);
  assert.ok(run.stdout.includes('Stub answer one.'), run.stdout);
  const defaultRun = await stubCli(f, ['--no-mcp', '--no-subagents', '-p', 'hi']).done;
  assert.equal(defaultRun.code, 0);
  assert.ok(defaultRun.stdout.includes('[stub] deterministic reply'), defaultRun.stdout);
  const logs = readdirSync(join(f.data, 'goon', 'sessions')).filter(name => name.endsWith('.events.jsonl'));
  assert.equal(logs.length >= 1, true);
  const parsed = readFileSync(join(f.data, 'goon', 'sessions', logs[0]), 'utf8').split('\n').filter(Boolean).map(line => JSON.parse(line));
  assert.equal(parsed[0].type, 'session');
  assert.equal(parsed[1].type, 'user');
  assert.equal(parsed[parsed.length - 1].type, 'done');
});

test('stub-provider: --json stays compatible without tool_stream/user', async t => {
  const f = await fixture(t);
  writeFileSync(join(f.cwd, 'notes.txt'), 'note body\n');
  const run = await stubCli(f, ['--json', '--no-mcp', '--no-subagents', '-p', 'read the file', '--stub-fixture',
    resolve('tests/fixtures/stub-tool-call.json')]).done;
  assert.equal(run.code, 0);
  const events = run.stdout.split('\n').filter(Boolean).map(line => JSON.parse(line));
  assert.equal(events.some(event => event.type === 'user'), false);
  assert.equal(events.some(event => event.type === 'tool_stream'), false);
  assert.equal(events.some(event => event.type === 'tool'), true);
  assert.equal(events[events.length - 1].type, 'done');
});

const twoTurnFixture = {
  v: 1,
  turns: [
    { match: 'first', text: ['A'], usage: { inputTokens: 1, outputTokens: 2, totalTokens: 3 } },
    { match: 'second', text: ['B'], usage: { inputTokens: 4, outputTokens: 5, totalTokens: 9 } },
  ],
};

test('stub-provider: turn selection by match, auxiliary and exhaustion', async () => {
  const model = createStubModel(twoTurnFixture);
  const auxiliary = await model.doGenerate(callOptions('third prompt'));
  assert.deepEqual(auxiliary.content, [{ type: 'text', text: '[stub] auxiliary response' }]);
  const first = await model.doGenerate(callOptions('first prompt'));
  assert.deepEqual(first.content, [{ type: 'text', text: 'A' }]);
  const second = await model.doGenerate(callOptions('second prompt'));
  assert.deepEqual(second.content, [{ type: 'text', text: 'B' }]);
  const exhausted = await model.doGenerate(callOptions('third prompt'));
  assert.deepEqual(exhausted.content, [{ type: 'text', text: '[stub] script exhausted' }]);
});

test('stub-provider: fixture.input selects first unused turn', async () => {
  const fixture = { v: 1, input: 'hi', turns: [{ text: ['A'] }, { text: ['B'] }] };
  const model = createStubModel(fixture);
  const first = await model.doGenerate(callOptions('hi there'));
  assert.deepEqual(first.content, [{ type: 'text', text: 'A' }]);
  const second = await model.doGenerate(callOptions('hi again'));
  assert.deepEqual(second.content, [{ type: 'text', text: 'B' }]);
});

test('stub-provider: fresh cursor per model, shared cursor when passed', async () => {
  const separateA = createStubModel(twoTurnFixture);
  const separateB = createStubModel(twoTurnFixture);
  assert.deepEqual((await separateA.doGenerate(callOptions('first'))).content, [{ type: 'text', text: 'A' }]);
  assert.deepEqual((await separateB.doGenerate(callOptions('first'))).content, [{ type: 'text', text: 'A' }]);

  const cursor = { index: 0 };
  const sharedA = createStubModel(twoTurnFixture, cursor);
  const sharedB = createStubModel(twoTurnFixture, cursor);
  assert.deepEqual((await sharedA.doGenerate(callOptions('first'))).content, [{ type: 'text', text: 'A' }]);
  assert.deepEqual((await sharedB.doGenerate(callOptions('first'))).content, [{ type: 'text', text: 'B' }]);
});

test('stub-provider: custom auxiliary and exhaustion', async () => {
  const fixture = { v: 1, input: 'go', turns: [{ text: ['once'] }, { text: ['twice'] }], auxiliary: { text: ['aux'] } };
  const model = createStubModel(fixture);
  assert.deepEqual((await model.doGenerate(callOptions('nothing'))).content, [{ type: 'text', text: 'aux' }]);
  assert.deepEqual((await model.doGenerate(callOptions('go'))).content, [{ type: 'text', text: 'once' }]);
  assert.deepEqual((await model.doGenerate(callOptions('go'))).content, [{ type: 'text', text: 'twice' }]);
  assert.deepEqual((await model.doGenerate(callOptions('go'))).content, [{ type: 'text', text: '[stub] script exhausted' }]);
});

test('stub-provider: exhausted default auxiliary text', async () => {
  const model = createStubModel({ v: 1, turns: [{ text: ['once'] }] });
  await model.doGenerate(callOptions('x'));
  const exhausted = await model.doGenerate(callOptions('y'));
  assert.deepEqual(exhausted.content, [{ type: 'text', text: '[stub] script exhausted' }]);
});

test('stub-provider: error turn throws APICallError', async () => {
  const model = createStubModel({ v: 1, turns: [{ error: { message: 'boom', statusCode: 400, retryable: false } }] });
  await assert.rejects(() => model.doGenerate(callOptions('x')), error => {
    assert.equal(error instanceof APICallError, true);
    assert.equal(error.statusCode, 400);
    assert.equal(error.isRetryable, false);
    return true;
  });
});

test('stub-provider: reasoning, tool calls and usage are emitted', async () => {
  const model = createStubModel({
    v: 1,
    turns: [{
      reasoning: ['think'],
      text: ['answer'],
      toolCalls: [{ id: 'call-1', name: 'read_file', input: { path: 'a.txt' } }],
      usage: { inputTokens: 7, outputTokens: 9, totalTokens: 16 },
    }],
  });
  const generated = await model.doGenerate(callOptions('x'));
  assert.deepEqual(generated.content, [
    { type: 'reasoning', text: 'think' },
    { type: 'tool-call', toolCallId: 'call-1', toolName: 'read_file', input: '{"path":"a.txt"}' },
    { type: 'text', text: 'answer' },
  ]);
  assert.equal(generated.finishReason.unified, 'tool-calls');
  assert.equal(generated.usage.inputTokens.total, 7);
  assert.equal(generated.usage.outputTokens.total, 9);

  const streamed = await createStubModel({
    v: 1,
    turns: [{
      reasoning: ['think'],
      text: ['answer'],
      toolCalls: [{ id: 'call-1', name: 'read_file', input: { path: 'a.txt' } }],
      usage: { inputTokens: 7, outputTokens: 9, totalTokens: 16 },
    }],
  }).doStream(callOptions('x'));
  const parts = [];
  for await (const part of streamed.stream) parts.push(part.type);
  assert.deepEqual(parts, [
    'stream-start',
    'reasoning-start', 'reasoning-delta', 'reasoning-end',
    'tool-input-start', 'tool-input-delta', 'tool-input-end', 'tool-call',
    'text-start', 'text-delta', 'text-end',
    'finish',
  ]);
});

test('stub-provider: fixture schema and 1 MiB cap', async () => {
  assert.throws(() => stubFixtureSchema.parse({ v: 1, turns: [{ extra: true }] }));
  const root = await realpath(await mkdtemp(join(tmpdir(), 'goon-stub-')));
  try {
    const big = join(root, 'big.json');
    writeFileSync(big, JSON.stringify({ v: 1, turns: [{ text: ['x'.repeat(1_100_000)] }] }));
    assert.throws(() => loadStubFixture(big), /exceeds 1 MiB/);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test('stub-provider: default fixture and no network', async () => {
  const originalFetch = globalThis.fetch;
  globalThis.fetch = () => { throw new Error('fetch must not be called'); };
  try {
    const model = createStubModel(loadStubFixture());
    const result = await model.doGenerate(callOptions('anything'));
    assert.deepEqual(result.content, [{ type: 'text', text: '[stub] deterministic reply' }]);
    const streamed = await model.doStream(callOptions('anything'));
    for await (const part of streamed.stream) assert.notEqual(part.type, 'error');
  } finally {
    globalThis.fetch = originalFetch;
  }
});
