import test from 'node:test';
import assert from 'node:assert/strict';
import { join } from 'node:path';
import { mkdir, readFile, writeFile } from 'node:fs/promises';
import { fixture } from './helpers.mjs';
import { HELP } from '../dist/help.js';
import { loadConfig } from '../dist/config.js';
import { Harness } from '../dist/harness.js';
import { ToolRegistry } from '../dist/tools.js';
import { SessionStore } from '../dist/session.js';
import { VerificationFailedError } from '../dist/verify.js';

function makeFakeModel(turns) {
  const cursor = { index: 0 };
  function getTurn() {
    let turn;
    if (cursor.index < turns.length) {
      turn = turns[cursor.index++];
    } else {
      turn = { text: '[stub] script exhausted' };
    }
    return turn;
  }
  return {
    specificationVersion: 'v3',
    provider: 'test',
    modelId: 'test-model',
    supportedUrls: {},
    async doGenerate() { throw new Error('not used'); },
    async doStream() {
      const turn = getTurn();
      const chunks = [];
      chunks.push({ type: 'stream-start', warnings: [] });
      if (Array.isArray(turn.calls) && turn.calls.length) {
        for (const call of turn.calls) {
          const id = call.id || `call-${Math.random().toString(36).slice(2)}`;
          chunks.push({ type: 'tool-input-start', id, toolName: call.name });
          const args = JSON.stringify(call.input || {});
          chunks.push({ type: 'tool-input-delta', id, delta: args });
          chunks.push({ type: 'tool-input-end', id });
          chunks.push({ type: 'tool-call', toolCallId: id, toolName: call.name, input: args });
        }
      } else {
        const id = 'text-1';
        chunks.push({ type: 'text-start', id });
        chunks.push({ type: 'text-delta', id, delta: turn.text || '' });
        chunks.push({ type: 'text-end', id });
      }
      chunks.push({
        type: 'finish',
        finishReason: { unified: Array.isArray(turn.calls) && turn.calls.length ? 'tool-calls' : 'stop', raw: Array.isArray(turn.calls) && turn.calls.length ? 'tool_calls' : 'stop' },
        usage: { inputTokens: { total: 10, noCache: 10, cacheRead: 0, cacheWrite: 0 }, outputTokens: { total: 5, text: 5, reasoning: 0 }, totalTokens: 15 },
      });
      const stream = new ReadableStream({
        start(controller) { for (const chunk of chunks) controller.enqueue(chunk); controller.close(); }
      });
      return { stream };
    },
  };
}

function makeConfig(f, verifyOverrides = {}) {
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
    verify: {
      enabled: true,
      commands: ['node -e "process.exit(0)"'],
      timeoutMs: 30000,
      maxRepairRounds: 2,
      policy: { requireTests: false, requireLint: false, requireTypecheck: false, allowWarnings: true },
      ...verifyOverrides,
    },
  };
}

async function setup(t, turns, verifyOverrides, approveImpl, runnerFactory) {
  const f = await fixture(t);
  await mkdir(join(f.cwd, 'src'), { recursive: true });
  const store = await SessionStore.open(f.data, { cwd: f.cwd, provider: 'openai-compatible', model: 'test-model', baseUrl: 'http://localhost/v1' });
  const tools = new ToolRegistry(f.cwd, approveImpl || (async () => true));
  const events = [];
  const emit = (event) => events.push(event);
  const runnerCalls = [];
  const fakeRunner = runnerFactory ? runnerFactory(f) : {
    async run(command, args) {
      let code = 0;
      let output = '';
      try {
        const content = await readFile(join(f.cwd, 'bad.ts'), 'utf8');
        if (content.includes('BROKEN')) {
          code = 1;
          output = 'bad.ts(1,1): error TS2322: Type string is not assignable to number (test-secret-never-echo)';
        }
      } catch {
        code = 0;
      }
      runnerCalls.push({ command, args, code });
      return { code, output, timedOut: false, notFound: false };
    },
  };
  const config = makeConfig(f, verifyOverrides);
  const model = makeFakeModel(turns);
  const harness = new Harness(config, store, tools, emit, model, { verify: { enabled: config.verify.enabled, runner: fakeRunner, policy: config.verify.policy } });
  return { f, store, tools, events, runnerCalls, harness };
}

test('harness verify: fail then repair pass emits start/result and injects only once', async t => {
  const { f, store, events, runnerCalls, harness } = await setup(t, [
    { calls: [{ id: 'w1', name: 'write_file', input: { path: 'bad.ts', content: 'const x: number = "BROKEN";\n' } }] },
    // A read-only batch after the failure must not consume another repair attempt.
    { calls: [{ id: 'r0', name: 'read_file', input: { path: 'bad.ts' } }] },
    { calls: [{ id: 'e1', name: 'edit_file', input: { path: 'bad.ts', oldText: 'const x: number = "BROKEN";', newText: 'const x: number = 42;' } }] },
    { text: 'Fixed.' },
  ]);
  const text = await harness.run('please fix', new AbortController().signal);
  assert.equal(text.trim(), 'Fixed.');
  const starts = events.filter(e => e.type === 'verify_start');
  const results = events.filter(e => e.type === 'verify_result');
  assert.equal(starts.length, 2);
  assert.deepEqual(results.map(r => r.status), ['fail', 'pass']);
  // Runner invoked twice (once per edit batch).
  assert.equal(runnerCalls.length, 2);
  // Failure injected into the session before the repair turn.
  const userMsgs = store.session.messages.filter(m => m.role === 'user').map(m => (typeof m.content === 'string' ? m.content : ''));
  assert.ok(userMsgs.some(m => m.includes('Verification failed (attempt 1/2)')));
  // Redaction: the secret must not leak.
  const serialized = JSON.stringify({ events, messages: store.session.messages });
  assert.ok(!serialized.includes('test-secret-never-echo'));
  // Repaired file on disk.
  const content = await readFile(join(f.cwd, 'bad.ts'), 'utf8');
  assert.ok(!content.includes('BROKEN'));
});

test('harness verify: model gives up triggers rollback and VerificationFailedError', async t => {
  const { f, store, events, harness } = await setup(t, [
    { calls: [{ id: 'w1', name: 'write_file', input: { path: 'bad.ts', content: 'const x: number = "BROKEN";\n' } }] },
    { text: 'I give up.' },
  ]);
  await assert.rejects(() => harness.run('please fix', new AbortController().signal), VerificationFailedError);
  const rollback = events.find(e => e.type === 'verify_rollback');
  assert.ok(rollback, 'verify_rollback event expected');
  // File removed by rollback (it did not exist before the turn).
  await assert.rejects(() => readFile(join(f.cwd, 'bad.ts'), 'utf8'));
  // Checkpoint retains no files after a complete rollback.
  const lastCp = store.session.checkpoints?.at(-1);
  assert.ok(lastCp);
  assert.equal((lastCp.files || []).length, 0);
});

test('harness verify: approval denied skips without rollback and clears verdict', async t => {
  const { f, events, runnerCalls, harness } = await setup(t, [
    { calls: [{ id: 'w1', name: 'write_file', input: { path: 'bad.ts', content: 'const x: number = "BROKEN";\n' } }] },
    { text: 'Done without fixing.' },
  ], {}, async (req) => req.tool !== 'verify');
  const text = await harness.run('please fix', new AbortController().signal);
  assert.equal(text.trim(), 'Done without fixing.');
  const results = events.filter(e => e.type === 'verify_result');
  assert.equal(results.length, 1);
  assert.equal(results[0].status, 'skip');
  assert.equal(events.filter(e => e.type === 'verify_rollback').length, 0);
  assert.equal(runnerCalls.length, 0);
  // File stays broken because the user denied verification.
  const content = await readFile(join(f.cwd, 'bad.ts'), 'utf8');
  assert.ok(content.includes('BROKEN'));
});

test('harness verify: disabled harness never calls the runner', async t => {
  const { events, runnerCalls, harness } = await setup(t, [
    { calls: [{ id: 'w1', name: 'write_file', input: { path: 'bad.ts', content: 'const x: number = "BROKEN";\n' } }] },
    { text: 'Done.' },
  ], { enabled: false }, async () => true);
  const text = await harness.run('please fix', new AbortController().signal);
  assert.equal(text.trim(), 'Done.');
  assert.equal(runnerCalls.length, 0);
  assert.equal(events.filter(e => e.type === 'verify_start').length, 0);
});

test('harness verify: rollback keeps user-changed files as conflicts', async t => {
  const { f, store, events, harness } = await setup(t, [
    { calls: [{ id: 'w1', name: 'write_file', input: { path: 'bad.ts', content: 'const x: number = "BROKEN";\n' } }] },
    { text: 'Giving up.' },
  ], {}, async () => true, (fixtureDir) => ({
    async run() {
      const path = join(fixtureDir.cwd, 'bad.ts');
      const current = await readFile(path, 'utf8');
      await writeFile(path, `// user change\n${current}`);
      return { code: 1, output: 'still broken', timedOut: false, notFound: false };
    },
  }));
  await assert.rejects(() => harness.run('please fix', new AbortController().signal), VerificationFailedError);
  // The user's edit is newer than the turn backup, so rollback must not overwrite it.
  const content = await readFile(join(f.cwd, 'bad.ts'), 'utf8');
  assert.ok(content.includes('// user change'));
  const rollback = events.find(e => e.type === 'verify_rollback');
  assert.deepEqual(rollback.conflicts, ['bad.ts']);
  // The conflict stays in the checkpoint so an explicit /revert can still restore it.
  const lastCp = store.session.checkpoints?.at(-1);
  assert.equal((lastCp.files || []).length, 1);
});

test('config verify: env and CLI overrides merge without dropping defaults', async t => {
  const f = await fixture(t);
  const saved = {
    XDG_CONFIG_HOME: process.env.XDG_CONFIG_HOME,
    GOON_VERIFY_MAX_REPAIR_ROUNDS: process.env.GOON_VERIFY_MAX_REPAIR_ROUNDS,
    GOON_VERIFY_CMDS: process.env.GOON_VERIFY_CMDS,
    GOON_VERIFY_REQUIRE_TESTS: process.env.GOON_VERIFY_REQUIRE_TESTS,
  };
  process.env.XDG_CONFIG_HOME = f.config;
  process.env.GOON_VERIFY_MAX_REPAIR_ROUNDS = '4';
  process.env.GOON_VERIFY_CMDS = 'npm test,npm run build';
  process.env.GOON_VERIFY_REQUIRE_TESTS = 'true';
  t.after(() => {
    for (const [name, value] of Object.entries(saved)) {
      if (value === undefined) delete process.env[name];
      else process.env[name] = value;
    }
  });
  const config = await loadConfig({ verify: { enabled: false } });
  assert.equal(config.verify.enabled, false, 'CLI override wins');
  assert.equal(config.verify.maxRepairRounds, 4, 'env value applies');
  assert.deepEqual(config.verify.commands, ['npm test', 'npm run build']);
  assert.equal(config.verify.policy.requireTests, true);
  assert.equal(config.verify.policy.allowWarnings, true, 'untouched defaults survive the merge');
});

test('help lists verify and cost flags', () => {
  for (const flag of ['--no-verify', '--verify-cmd', '--max-cost-usd', '--max-time-ms']) {
    assert.ok(HELP.includes(flag), `${flag} missing from help`);
  }
});
