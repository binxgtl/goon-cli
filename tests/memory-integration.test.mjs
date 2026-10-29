import test from 'node:test';
import assert from 'node:assert/strict';
import { join } from 'node:path';
import { mkdir, readFile, writeFile } from 'node:fs/promises';
import { fixture } from './helpers.mjs';
import { Harness } from '../dist/harness.js';
import { ToolRegistry } from '../dist/tools.js';
import { SessionStore } from '../dist/session.js';
import { MemoryStore } from '../dist/memory.js';
import { projectIdFor, ragSnapshotPath } from '../dist/rag.js';

const SECRET = 'goon-test-secret-abc123';

function makeFakeModel(turns) {
  const cursor = { index: 0 };
  return {
    specificationVersion: 'v3',
    provider: 'test',
    modelId: 'test-model',
    supportedUrls: {},
    async doGenerate() { throw new Error('not used'); },
    async doStream() {
      const turn = cursor.index < turns.length ? turns[cursor.index++] : { text: '[stub] script exhausted' };
      const chunks = [{ type: 'stream-start', warnings: [] }];
      if (Array.isArray(turn.calls) && turn.calls.length) {
        for (const call of turn.calls) {
          const id = call.id || `call-${Math.random().toString(36).slice(2)}`;
          chunks.push({ type: 'tool-input-start', id, toolName: call.name });
          chunks.push({ type: 'tool-input-delta', id, delta: JSON.stringify(call.input || {}) });
          chunks.push({ type: 'tool-input-end', id });
          chunks.push({ type: 'tool-call', toolCallId: id, toolName: call.name, input: JSON.stringify(call.input || {}) });
        }
      } else {
        chunks.push({ type: 'text-start', id: 't1' });
        chunks.push({ type: 'text-delta', id: 't1', delta: turn.text || '' });
        chunks.push({ type: 'text-end', id: 't1' });
      }
      chunks.push({
        type: 'finish',
        finishReason: { unified: Array.isArray(turn.calls) && turn.calls.length ? 'tool-calls' : 'stop', raw: Array.isArray(turn.calls) && turn.calls.length ? 'tool_calls' : 'stop' },
        usage: { inputTokens: { total: 10, noCache: 10, cacheRead: 0, cacheWrite: 0 }, outputTokens: { total: 5, text: 5, reasoning: 0 }, totalTokens: 15 },
      });
      return { stream: new ReadableStream({ start(controller) { for (const chunk of chunks) controller.enqueue(chunk); controller.close(); } }) };
    },
  };
}

function makeConfig(f, overrides = {}) {
  return {
    provider: 'openai-compatible',
    baseUrl: 'http://localhost/v1',
    model: 'test-model',
    apiKey: 'test-secret-never-echo',
    cwd: f.cwd,
    dataDir: f.data,
    contextWindow: 128000,
    maxOutputTokens: 1024,
    timeoutMs: 30000,
    runTokenBudget: 0,
    maxCostUsd: 0,
    maxTimeMs: 0,
    inlineReasoning: 'off',
    showReasoning: false,
    keyEnv: 'GOON_API_KEY',
    pricing: { models: {} },
    memoryEnabled: true,
    ragEnabled: true,
    ragTopK: 10,
    ragCapacity: 5000,
    ragThreshold: 0,
    ragIndexMaxFiles: 2000,
    ragIndexMaxFileBytes: 1_048_576,
    contextReserveRatio: 0.15,
    contextConversationRatio: 0.45,
    contextFilesRatio: 0.25,
    verify: {
      enabled: true,
      commands: [],
      timeoutMs: 30000,
      maxRepairRounds: 2,
      policy: { requireTests: false, requireLint: false, requireTypecheck: false, allowWarnings: true },
    },
    ...overrides,
  };
}

async function setup(t, turns, { memory, configOverrides = {}, approve } = {}) {
  const f = await fixture(t);
  await mkdir(join(f.cwd, 'src'), { recursive: true });
  await writeFile(join(f.cwd, 'src', 'notes.md'), 'The quuxflurb doc lives here.\n');
  const store = await SessionStore.open(f.data, { cwd: f.cwd, provider: 'openai-compatible', model: 'test-model', baseUrl: 'http://localhost/v1' });
  const tools = new ToolRegistry(f.cwd, approve ?? (async () => true));
  const config = makeConfig(f, configOverrides);
  const events = [];
  let memoryStore = memory;
  if (memoryStore === undefined) {
    memoryStore = await MemoryStore.open({
      dataDir: f.data,
      projectId: projectIdFor(f.cwd),
      configDir: f.config,
      root: f.cwd,
      secrets: [SECRET],
      rag: { topK: config.ragTopK, capacity: config.ragCapacity, threshold: config.ragThreshold, maxFiles: config.ragIndexMaxFiles, maxFileBytes: config.ragIndexMaxFileBytes },
      conversation: { get messages() { return store.session.messages; } },
    });
  }
  const fakeRunner = { async run() { return { code: 0, output: '', timedOut: false, notFound: false }; } };
  const harness = new Harness(config, store, tools, event => events.push(event), makeFakeModel(turns), {
    verify: { enabled: config.verify.enabled, runner: fakeRunner, policy: config.verify.policy },
    memory: memoryStore,
  });
  return { f, store, tools, events, config, harness, memoryStore };
}

test('memory integration: one run wires index/retrieval/events and redacts tool results', async t => {
  const { f, store, events, harness, memoryStore } = await setup(t, [
    { calls: [{ id: 'w1', name: 'write_file', input: { path: 'note.ts', content: 'export const quuxnote = 1;\n' } }] },
    { text: 'Wrote note.ts with the quuxnote export.' },
    { calls: [{ id: 'b1', name: 'bash', input: { command: `echo ${SECRET}` } }] },
    { text: 'Status is fine.' },
  ]);
  const text = await harness.run('create note.ts using the quuxflurb docs', new AbortController().signal);
  assert.match(text, /Wrote note\.ts/);

  // Index once per session, before the first request.
  const indexed = events.filter(e => e.type === 'memory_indexed');
  assert.ok(indexed.length >= 1 && indexed[0].files >= 1 && indexed[0].chunks >= 1);
  // Retrieval served lexical chunks from that index.
  const retrieved = events.filter(e => e.type === 'memory_retrieved');
  assert.ok(retrieved.length >= 1 && retrieved[0].chunks >= 1 && retrieved[0].tokens >= 0);
  // The context budget allocation was emitted and is internally consistent.
  const budget = events.find(e => e.type === 'context_budget');
  assert.ok(budget, 'context_budget event expected');
  assert.equal(budget.inputBudget, 128000 - 1024);
  assert.ok(budget.system > 0 && budget.conversation > 0 && budget.files > 0);
  assert.ok(budget.system + budget.conversation + budget.files <= budget.inputBudget, 'allocation fits the input budget');
  // The session store never sees the untrusted block or pruned messages.
  assert.ok(!JSON.stringify(store.session.messages).includes('<untrusted_context>'));

  // Drain the single-writer chain, then check what memory actually persisted.
  await memoryStore.persist();
  const memoryRaw = await readFile(join(f.data, 'memory', projectIdFor(f.cwd), 'memory.json'), 'utf8');
  const memoryJson = JSON.parse(memoryRaw);
  assert.equal(memoryJson.exec.length, 1, 'task_completed wrote one lesson');
  assert.match(memoryJson.exec[0].text, /task: create note\.ts/);
  assert.match(memoryJson.exec[0].text, /outcome: Wrote note\.ts/);
  assert.match(memoryJson.exec[0].text, /edited note\.ts \(\d+ bytes\)/, 'patch_applied recorded a byte delta');
  assert.match(memoryJson.exec[0].text, /verify pass/, 'verify_result reached memory');

  // Patch reindex: the file written during the run is searchable in rag.json.
  const ragRaw = await readFile(ragSnapshotPath(f.data, projectIdFor(f.cwd)), 'utf8');
  assert.ok(ragRaw.includes('quuxnote'), 'run-start index or patch reindex captured note.ts');
  assert.ok(!ragRaw.includes(SECRET));

  // A second run must not re-index (once per session) and its bash summary reaches memory redacted.
  const before = events.filter(e => e.type === 'memory_indexed').length;
  await harness.run(`echo ${SECRET} for status`, new AbortController().signal);
  assert.equal(events.filter(e => e.type === 'memory_indexed').length, before);
  await memoryStore.persist();
  const memoryRaw2 = await readFile(join(f.data, 'memory', projectIdFor(f.cwd), 'memory.json'), 'utf8');
  const memoryJson2 = JSON.parse(memoryRaw2);
  assert.ok(memoryJson2.exec.length >= 2);
  assert.match(memoryJson2.exec.at(-1).text, /echo \[REDACTED\]/, 'tool_result summary persisted redacted');
  assert.ok(!memoryRaw2.includes(SECRET), 'secret from a bash command must be redacted in memory');
  await memoryStore.close();
});

test('memory integration: ragEnabled=false never indexes (opt-in invariant)', async t => {
  const { events, harness, memoryStore, f } = await setup(t, [
    { text: 'Done.' },
  ], { configOverrides: { ragEnabled: false } });
  const text = await harness.run('just answer', new AbortController().signal);
  assert.match(text, /Done\./);
  assert.equal(events.filter(e => e.type === 'memory_indexed').length, 0, 'no index events');
  // Retrieval itself is memory-gated, not rag-gated (plan §7.1): it may run but finds nothing.
  const retrieved = events.filter(e => e.type === 'memory_retrieved');
  assert.ok(retrieved.every(e => e.chunks === 0), 'no chunks without an index');
  // Task wiring still works without the index.
  await memoryStore.persist();
  const memoryJson = JSON.parse(await readFile(join(f.data, 'memory', projectIdFor(f.cwd), 'memory.json'), 'utf8'));
  assert.match(memoryJson.exec.at(-1).text, /outcome: Done\./);
  await assert.rejects(readFile(ragSnapshotPath(f.data, projectIdFor(f.cwd)), 'utf8'), { code: 'ENOENT' }, 'rag.json must not be created');
  await memoryStore.close();
});

test('memory integration: memory failures surface as memory_error and never fail the run', async t => {
  const fakeMemory = {
    handleEvent: async () => { throw new Error('event boom'); },
    ensureRepoIndexed: async () => { throw new Error('index boom'); },
    retrieve: async () => { throw new Error('retrieve boom'); },
  };
  const { events, harness } = await setup(t, [
    { text: 'Still fine.' },
  ], { memory: fakeMemory });
  const text = await harness.run('run with broken memory', new AbortController().signal);
  assert.match(text, /Still fine\./, 'run completes despite broken memory');
  const errors = events.filter(e => e.type === 'memory_error').map(e => e.message);
  assert.ok(errors.some(message => message.includes('index boom')), `got: ${JSON.stringify(errors)}`);
  assert.ok(errors.some(message => message.includes('retrieve boom')));
  assert.ok(errors.some(message => message.includes('rejected an event')), 'handleEvent rejections surface too');
  assert.ok(events.find(e => e.type === 'context_budget'), 'budget event still emitted');
});

test('memory integration: a failed run still writes task_completed with an error outcome', async t => {
  const { f, harness, memoryStore } = await setup(t, [
    { text: 'never reached' },
  ]);
  const controller = new AbortController();
  controller.abort(new Error('cancelled by test'));
  await assert.rejects(() => harness.run('will abort', controller.signal));
  await memoryStore.persist();
  const memoryJson = JSON.parse(await readFile(join(f.data, 'memory', projectIdFor(f.cwd), 'memory.json'), 'utf8'));
  assert.ok(memoryJson.exec.length >= 1);
  assert.match(memoryJson.exec.at(-1).text, /outcome: error:/, 'error outcome recorded');
  await memoryStore.close();
});
