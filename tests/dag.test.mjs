import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdir, readFile, readdir, writeFile } from 'node:fs/promises';
import { existsSync } from 'node:fs';
import { join } from 'node:path';
import { setTimeout } from 'node:timers/promises';
import {
  DEFAULT_DAG_TOKEN_BUDGET,
  ErrNotVerified,
  ROLE_TOOLSETS,
  runDag,
} from '../dist/coord.js';
import { WorkspaceFiles } from '../dist/files.js';
import { ToolRegistry } from '../dist/tools.js';
import { Subagents } from '../dist/subagents.js';
import { SessionStore } from '../dist/session.js';
import { loadConfig } from '../dist/config.js';
import { DEFAULT_VERIFY_POLICY, createVerifyRunner, runVerification } from '../dist/verify.js';
import { fixture, provider, sse, cli } from './helpers.mjs';

const isChild = body => body.messages.some(m => typeof m.content === 'string' && m.content.startsWith('You are a delegated agent.'));
const isPlanner = body => body.messages.some(m => m.role === 'system' && typeof m.content === 'string' && m.content.includes('planning assistant'));
const configFor = (f, p, extra = {}) => ({ cwd: f.cwd, dataDir: f.data, provider: 'openai-compatible', model: 'test-model', baseUrl: p.url,
  apiKey: 'fake', contextWindow: 128000, maxOutputTokens: 8192, timeoutMs: 3000, runTokenBudget: 100000, ...extra });

const verifyConfig = (commands, extra = {}) => ({
  enabled: true,
  commands,
  timeoutMs: 30_000,
  maxRepairRounds: 1,
  policy: DEFAULT_VERIFY_POLICY,
  ...extra,
});

test('allowedTools hides tools from schemas() and execute() (registry-level enforcement)', async t => {
  const f = await fixture(t);
  const tools = new ToolRegistry(f.cwd, async () => true, false, new Set(['read_file', 'list_files', 'search']));
  const names = Object.keys(tools.schemas());
  assert.deepEqual(names.sort(), ['list_files', 'read_file', 'search']);
  // A hidden tool named directly by the model is rejected by execute(), not just omitted.
  await assert.rejects(
    tools.execute({ toolCallId: 'c1', toolName: 'bash', input: { command: 'echo hi' } }, AbortSignal.timeout(2000)),
    /not available/i,
  );
  await assert.rejects(
    tools.execute({ toolCallId: 'c2', toolName: 'write_file', input: { path: 'x.txt', content: 'no' } }, AbortSignal.timeout(2000)),
    /not available/i,
  );
  // Wildcards work: mcp__* admits every MCP tool name.
  const wildcard = new ToolRegistry(f.cwd, async () => true, true, new Set(['read_file', 'mcp__*']));
  assert.deepEqual(Object.keys(wildcard.schemas()).sort(), ['read_file']);
});

test('runTasks is the public spawn path and pins allowedTools in the child registry', async t => {
  const f = await fixture(t);
  let childTools;
  const p = await provider(t, (body, response) => {
    if (isChild(body)) {
      childTools = body.tools.map(tool => tool.function.name).sort();
      return sse(response, { text: 'child report' });
    }
    return sse(response, { text: 'unused' });
  });
  const config = configFor(f, p);
  const store = await SessionStore.open(join(f.data, 'sessions'), config);
  t.after(() => store.close());
  const tools = new ToolRegistry(f.cwd, async () => true);
  const subagents = new Subagents(config, store, tools);
  const out = await subagents.runTasks(['inspect'], {
    readOnly: false,
    signal: AbortSignal.timeout(8000),
    allowedTools: new Set(['read_file', 'list_files', 'search']),
  });
  assert.match(out, /child report/);
  // readOnly:false with a read-only allowlist still exposes exactly the pinned tools.
  assert.deepEqual(childTools, ['list_files', 'read_file', 'search']);
});

test('delegate_edit ignores model-supplied allowedTools and keeps the pinned parent toolset', async t => {
  const f = await fixture(t);
  let childTools;
  const p = await provider(t, (body, response) => {
    if (isChild(body)) {
      childTools = body.tools.map(tool => tool.function.name).sort();
      return sse(response, { text: 'edited.' });
    }
    if (!body.messages.some(m => m.role === 'tool')) {
      return sse(response, { calls: [{ id: 'esc', name: 'delegate_edit', input: { tasks: ['work'], allowedTools: ['read_file'] } }] });
    }
    return sse(response, { text: 'Done.' });
  });
  const config = configFor(f, p);
  const store = await SessionStore.open(join(f.data, 'sessions'), config);
  t.after(() => store.close());
  const tools = new ToolRegistry(f.cwd, async () => true);
  new Subagents(config, store, tools).register();
  await new (await import('../dist/harness.js')).Harness(config, store, tools).run('delegate', AbortSignal.timeout(8000));
  // The model asked to narrow the child to read_file only; the pinned parent toolset wins.
  assert.ok(childTools.includes('write_file'), `child tools must keep the parent write tools: ${childTools}`);
  assert.ok(childTools.includes('bash'), `child tools must keep bash: ${childTools}`);
  assert.ok(childTools.length > 5);
});

test('runDag lifecycle: write-serial spawn, per-todo backup/rollback on test failure, rework, merge', async t => {
  const f = await fixture(t);
  const files = new WorkspaceFiles(f.cwd);
  const events = [];
  const spawnedContents = [];
  let testRuns = 0;
  const budget = { spent: 0, limit: 50_000, costSpent: 0, costLimit: 0, startedAt: 1234, timeLimitMs: 0 };

  const result = await runDag('write a file', {
    planner: async () => ({ goal: 'write a file', todos: [{ id: 't1', title: 'write x.txt', acceptance: 'final content', role: 'coder' }] }),
    spawn: async ({ todo, budget: spawnBudget, allowedTools, readOnly }) => {
      assert.equal(spawnBudget, budget, 'every spawn receives the same shared budget object');
      assert.deepEqual(allowedTools, ROLE_TOOLSETS.coder);
      assert.equal(readOnly, false);
      const attempt = todo.reworkCycles + 1;
      spawnedContents.push(attempt);
      await files.write('x.txt', `attempt ${attempt}`);
      return `report attempt ${attempt} for ${todo.id}`;
    },
    test: async (todo, changed, signal) => {
      assert.equal(signal.aborted, false);
      assert.deepEqual(changed, ['x.txt']);
      testRuns++;
      return testRuns > 1; // first attempt fails, rework, second passes
    },
  }, {
    config: { multiAgentReworkCap: 3, multiAgentMaxConcurrency: 2 },
    signal: AbortSignal.timeout(8000),
    files,
    budget,
    emit: event => events.push(event),
  });

  assert.equal(result.merge.verified, true);
  assert.equal(result.plan.todos[0].reworkCycles, 1);
  assert.equal(result.plan.todos[0].status, 'done');
  assert.equal(await readFile(join(f.cwd, 'x.txt'), 'utf8'), 'attempt 2');
  assert.ok(result.merge.combinedDiff.includes('x.txt'));
  assert.deepEqual(events.filter(e => e.type === 'dag_todo_update' && e.status === 'in_progress').map(e => e.attempt), [1, 2]);
  assert.ok(events.some(e => e.type === 'dag_todo_update' && e.status === 'done'));
  assert.ok(events.some(e => e.type === 'dag_plan_ready' && e.todos === 1));
  assert.ok(events.some(e => e.type === 'dag_merge' && e.verified === true && e.conflicts === 0));
  assert.equal(budget.startedAt, 1234, 'the shared budget object is reused, not re-seeded');
});

test('runDag creates a fallback budget from config when none is passed', async t => {
  const f = await fixture(t);
  const files = new WorkspaceFiles(f.cwd);
  let seen;
  await runDag('g', {
    planner: async () => ({ goal: 'g', todos: [{ id: 't1', title: 'read', role: 'researcher' }] }),
    spawn: async ({ budget }) => { seen = budget; return 'r'; },
  }, {
    config: { runTokenBudget: 4242 },
    signal: AbortSignal.timeout(8000),
    files,
    allowEmpty: true,
  });
  assert.equal(seen.limit, 4242);
  assert.equal(seen.spent, 0);

  await runDag('g', {
    planner: async () => ({ goal: 'g', todos: [{ id: 't1', title: 'read', role: 'researcher' }] }),
    spawn: async ({ budget }) => { seen = budget; return 'r'; },
  }, {
    config: { runTokenBudget: 0 },
    signal: AbortSignal.timeout(8000),
    files,
    allowEmpty: true,
  });
  assert.equal(seen.limit, DEFAULT_DAG_TOKEN_BUDGET);
});

test('runDag verify=false rolls back every todo and throws ErrNotVerified', async t => {
  const f = await fixture(t);
  const files = new WorkspaceFiles(f.cwd);
  const events = [];
  await assert.rejects(
    runDag('g', {
      planner: async () => ({ goal: 'g', todos: [{ id: 't1', title: 'write', role: 'coder' }] }),
      spawn: async () => {
        await files.write('x.txt', 'should not survive');
        return 'r';
      },
      verify: async () => false,
    }, {
      config: {},
      signal: AbortSignal.timeout(8000),
      files,
      emit: event => events.push(event),
    }),
    error => error instanceof ErrNotVerified && /verification failed/i.test(error.message),
  );
  assert.equal(existsSync(join(f.cwd, 'x.txt')), false, 'the todo change was rolled back');
  assert.ok(events.some(e => e.type === 'dag_merge' && e.verified === false));
});

test('runDag test hook with commands and no approve callback fails closed and rolls back', async t => {
  const f = await fixture(t);
  const files = new WorkspaceFiles(f.cwd);
  const events = [];
  await assert.rejects(
    runDag('g', {
      planner: async () => ({ goal: 'g', todos: [{ id: 't1', title: 'write', role: 'coder' }] }),
      spawn: async () => {
        await files.write('x.txt', 'temp');
        return 'r';
      },
      // Simulates /dag wiring runVerification without an approve callback: fail-closed.
      test: (todo, changed, signal) => runVerification(
        f.cwd, changed, verifyConfig(['node --version']), DEFAULT_VERIFY_POLICY, createVerifyRunner(), signal,
      ),
    }, {
      config: {},
      signal: AbortSignal.timeout(8000),
      files,
      emit: event => events.push(event),
    }),
    /Plan failed.*t1\(failed: Verification commands require approval before running\.\)/s,
  );
  assert.equal(existsSync(join(f.cwd, 'x.txt')), false, 'rollback ran before the approval error surfaced');
  const failedEvent = events.find(e => e.type === 'dag_todo_update' && e.status === 'failed');
  assert.ok(failedEvent, 'the fail-closed reason reaches the terminal todo event');
  assert.match(failedEvent.reason, /approval/i);
});

test('runDag review rework cap: todo fails, dependents block, plan fails with full rollback', async t => {
  const f = await fixture(t);
  const files = new WorkspaceFiles(f.cwd);
  const events = [];
  let attempts = 0;
  await assert.rejects(
    runDag('g', {
      planner: async () => ({
        goal: 'g',
        todos: [
          { id: 't1', title: 'write', role: 'coder' },
          { id: 't2', title: 'check', role: 'tester', dependsOn: ['t1'] },
        ],
      }),
      spawn: async ({ todo }) => {
        attempts++;
        if (todo.id === 't1') await files.write('x.txt', `attempt ${attempts}`);
        return 'r';
      },
      review: async () => 'rejected',
    }, {
      config: { multiAgentReworkCap: 1 },
      signal: AbortSignal.timeout(8000),
      files,
      emit: event => events.push(event),
    }),
    /Plan failed.*t1\(failed: rework cap.*t2\(blocked\)/s,
  );
  assert.equal(attempts, 2, 'one rework then cap exceeded');
  assert.equal(existsSync(join(f.cwd, 'x.txt')), false, 'failed todo changes were rolled back');
  assert.ok(events.some(e => e.type === 'dag_todo_update' && e.todoId === 't2' && e.status === 'blocked'));
  const failedEvent = events.find(e => e.type === 'dag_todo_update' && e.todoId === 't1' && e.status === 'failed');
  assert.ok(failedEvent, 'the failed todo emits a terminal event');
  assert.match(failedEvent.reason, /rework cap/);
});

test('runDag: a read-only todo completing mid-write never steals the write todo snapshot', async t => {
  const f = await fixture(t);
  const files = new WorkspaceFiles(f.cwd);
  const events = [];
  const testedTodos = [];
  const result = await runDag('g', {
    planner: async () => ({
      goal: 'g',
      todos: [
        { id: 'r1', title: 'explore', role: 'researcher' },
        { id: 'w1', title: 'write', role: 'coder' },
      ],
    }),
    spawn: async ({ todo }) => {
      if (todo.id === 'r1') {
        // Resolve in the middle of the coder's spawn: the exact window where an unconditional
        // takeTurnBackups() would drain the shared buffer and steal the coder's snapshot.
        await setTimeout(15);
        return 'research notes';
      }
      await files.write('n1.txt', 'one');
      await files.write('n2.txt', 'two');
      await setTimeout(40);
      return 'coder report';
    },
    test: async (todo, changed) => {
      testedTodos.push({ todo: todo.id, changed: [...changed] });
      return true;
    },
  }, {
    config: {},
    signal: AbortSignal.timeout(8000),
    files,
    emit: event => events.push(event),
  });
  assert.equal(result.merge.verified, true);
  assert.equal(result.merge.conflicts.length, 0, 'no phantom researcher/coder conflict');
  assert.deepEqual(result.merge.artifacts.sort(), ['n1.txt', 'n2.txt'], 'the write todo keeps its own artifacts');
  assert.deepEqual(testedTodos, [{ todo: 'w1', changed: ['n1.txt', 'n2.txt'] }],
    'the test hook fires once, for the coder, with the coder files only');
  assert.ok(!events.some(e => e.type === 'dag_todo_update' && e.status === 'failed'));
});

test('runDag fills costLimit/timeLimitMs/startedAt from config for caller-provided budgets too', async t => {
  const f = await fixture(t);
  const files = new WorkspaceFiles(f.cwd);
  let seen;
  await runDag('g', {
    planner: async () => ({ goal: 'g', todos: [{ id: 't1', title: 'read', role: 'researcher' }] }),
    spawn: async ({ budget }) => { seen = budget; return 'r'; },
  }, {
    config: { maxCostUsd: 2.5, maxTimeMs: 60_000 },
    signal: AbortSignal.timeout(8000),
    files,
    budget: { spent: 0, limit: 12345 },
    allowEmpty: true,
  });
  assert.equal(seen.limit, 12345);
  assert.equal(seen.costLimit, 2.5, 'config.maxCostUsd reaches a bare caller budget');
  assert.equal(seen.timeLimitMs, 60_000, 'config.maxTimeMs reaches a bare caller budget');
  assert.equal(typeof seen.startedAt, 'number', 'startedAt defaults to now');
  // Existing values are never overwritten.
  const pinned = { spent: 0, limit: 1, costLimit: 9, timeLimitMs: 9, startedAt: 42 };
  await runDag('g', {
    planner: async () => ({ goal: 'g', todos: [{ id: 't1', title: 'read', role: 'researcher' }] }),
    spawn: async ({ budget }) => { seen = budget; return 'r'; },
  }, {
    config: { maxCostUsd: 2.5, maxTimeMs: 60_000 },
    signal: AbortSignal.timeout(8000),
    files,
    budget: pinned,
    allowEmpty: true,
  });
  assert.equal(seen.costLimit, 9);
  assert.equal(seen.timeLimitMs, 9);
  assert.equal(seen.startedAt, 42);
});

test('runDag surfaces spawn errors in the plan-failure detail', async t => {
  const f = await fixture(t);
  const files = new WorkspaceFiles(f.cwd);
  await assert.rejects(
    runDag('g', {
      planner: async () => ({ goal: 'g', todos: [{ id: 't1', title: 'write', role: 'coder' }] }),
      spawn: async () => {
        throw new Error('child exploded: root cause detail');
      },
    }, {
      config: {},
      signal: AbortSignal.timeout(8000),
      files,
    }),
    /Plan failed.*t1\(failed.*child exploded: root cause detail/s,
  );
});

test('runDag drains parallel siblings and rolls back when a hook throws mid-run', async t => {
  const f = await fixture(t);
  const files = new WorkspaceFiles(f.cwd);
  const events = [];
  let coderFinished = false;
  await assert.rejects(
    runDag('g', {
      planner: async () => ({
        goal: 'g',
        todos: [
          { id: 'r1', title: 'explore', role: 'researcher' },
          { id: 'w1', title: 'write', role: 'coder' },
        ],
      }),
      spawn: async ({ todo }) => {
        if (todo.id === 'r1') return 'research notes'; // review below throws for this todo
        await files.write('x.txt', 'in flight');
        await setTimeout(40);
        coderFinished = true;
        return 'coder report';
      },
      review: async todo => {
        if (todo.id === 'r1') throw new Error('review exploded');
        return 'approved';
      },
    }, {
      config: {},
      signal: AbortSignal.timeout(8000),
      files,
      emit: event => events.push(event),
    }),
    /Plan failed.*r1\(failed.*review exploded/s,
  );
  assert.equal(existsSync(join(f.cwd, 'x.txt')), false, 'the sibling write is rolled back with the plan');
  assert.ok(!events.some(e => e.type === 'dag_merge'), 'no merge is attempted for a failed plan');
  assert.ok(coderFinished, 'the sibling settled before the rollback (never left floating)');
});

test('runDag drains and rolls back when the final verify hook throws', async t => {
  const f = await fixture(t);
  const files = new WorkspaceFiles(f.cwd);
  await assert.rejects(
    runDag('g', {
      planner: async () => ({ goal: 'g', todos: [{ id: 't1', title: 'write', role: 'coder' }] }),
      spawn: async () => {
        await files.write('x.txt', 'temp');
        return 'r';
      },
      verify: async () => {
        throw new Error('verify exploded');
      },
    }, {
      config: {},
      signal: AbortSignal.timeout(8000),
      files,
    }),
    /verify exploded/,
  );
  assert.equal(existsSync(join(f.cwd, 'x.txt')), false, 'rollback still runs when verify throws');
});

test('runDag applies config.maxCostUsd to the shared budget and the child stops at the cap', async t => {
  const f = await fixture(t);
  // Each provider request costs exactly $1 (100 input tokens at $10_000/1M).
  const pricing = { 'test-model': { inputPer1M: 10_000, outputPer1M: 0 } };
  const p = await provider(t, (body, response) => {
    if (isChild(body)) {
      if (!body.messages.some(m => m.role === 'tool')) {
        return sse(response, { calls: [{ id: 'c1', name: 'list_files', input: {} }] });
      }
      return sse(response, { text: 'never reached: the cost cap fires first' });
    }
    return sse(response, { text: 'unused' });
  });
  const config = configFor(f, p, { pricing, maxCostUsd: 0.5 });
  const store = await SessionStore.open(join(f.data, 'sessions'), config);
  t.after(() => store.close());
  const tools = new ToolRegistry(f.cwd, async () => true);
  const subagents = new Subagents(config, store, tools);
  const files = new WorkspaceFiles(f.cwd);
  let seen;
  let spawnReport;
  await assert.rejects(
    runDag('g', {
      planner: async () => ({ goal: 'g', todos: [{ id: 't1', title: 'research', role: 'researcher' }] }),
      spawn: async ({ budget, signal: spawnSignal }) => {
        seen = budget;
        spawnReport = await subagents.runTasks(['inspect'], { readOnly: true, signal: spawnSignal, budget });
        return spawnReport;
      },
    }, {
      config,
      signal: AbortSignal.timeout(15_000),
      files,
    }),
    /no changes/,
    'the child never wrote, so the plan ends "no changes" after the cap stop',
  );
  assert.equal(seen.costLimit, 0.5, 'the runDag budget inherits config.maxCostUsd');
  assert.ok((seen.costSpent ?? 0) >= 0.5, `the shared mirror shows the child spend: ${seen.costSpent}`);
  assert.match(spawnReport, /Run cost budget reached/, 'the child stopped with the cost-cap reason');
});

test('/dag CLI decomposes, spawns a coder child, verifies with auto-approved commands and merges', async t => {
  const f = await fixture(t);
  await mkdir(join(f.config, 'goon'), { recursive: true });
  await writeFile(join(f.config, 'goon', 'config.json'), JSON.stringify({ verify: { commands: ['node --version'] } }));
  const p = await provider(t, (body, response) => {
    if (isPlanner(body)) {
      return sse(response, { text: JSON.stringify({
        goal: 'create a.txt',
        todos: [{ id: 't1', title: 'write a.txt', acceptance: 'a.txt contains the dag output', role: 'coder' }],
      }) });
    }
    if (isChild(body)) {
      const childTools = body.tools.map(tool => tool.function.name).sort();
      assert.deepEqual(childTools, ['apply_patch', 'bash', 'edit_file', 'list_files', 'read_file', 'search', 'write_file'],
        `coder toolset must be pinned: ${childTools}`);
      if (!body.messages.some(m => m.role === 'tool')) {
        return sse(response, { calls: [{ id: 'w1', name: 'write_file', input: { path: 'a.txt', content: 'dag output' } }] });
      }
      return sse(response, { text: 'Wrote a.txt.' });
    }
    return sse(response, { text: 'unexpected non-dag request' });
  });
  const result = await cli(f, p.url, ['--no-tui', '--json', '--yes', 'dag', 'create a.txt with the dag output']).done;
  assert.equal(result.code, 0, result.stdout + result.stderr);
  const events = result.stdout.trim().split('\n').map(line => { try { return JSON.parse(line); } catch { return null; } }).filter(Boolean);
  assert.ok(events.some(e => e.type === 'dag_plan_ready' && e.todos === 1), JSON.stringify(events));
  assert.ok(events.some(e => e.type === 'dag_todo_update' && e.todoId === 't1' && e.status === 'done' && e.role === 'coder'));
  assert.ok(events.some(e => e.type === 'dag_merge' && e.verified === true), JSON.stringify(events));
  assert.equal(await readFile(join(f.cwd, 'a.txt'), 'utf8'), 'dag output');
  // The verify commands ran through the approve callback (auto-approved): both test and final verify.
  const verifyEvents = events.filter(e => e.type === 'verify_start');
  assert.ok(verifyEvents.length >= 2, 'test stage and final verification both ran');
});

test('/dag blocks a hidden tool call at the child registry and the run continues', async t => {
  const f = await fixture(t);
  const p = await provider(t, (body, response) => {
    if (isPlanner(body)) {
      return sse(response, { text: JSON.stringify({
        goal: 'g',
        todos: [{ id: 't1', title: 'write a.txt', role: 'coder' }],
      }) });
    }
    if (isChild(body)) {
      if (!body.messages.some(m => m.role === 'tool')) {
        // todo_write is not in the coder role toolset: execute() must reject it.
        return sse(response, { calls: [{ id: 'hidden', name: 'todo_write', input: { todos: [] } }] });
      }
      if (body.messages.filter(m => m.role === 'tool').length === 1) {
        return sse(response, { calls: [{ id: 'w1', name: 'write_file', input: { path: 'a.txt', content: 'ok' } }] });
      }
      return sse(response, { text: 'Recovered and wrote the file.' });
    }
    return sse(response, { text: 'unexpected' });
  });
  const result = await cli(f, p.url, ['--no-tui', '--json', '--yes', 'dag', 'do it']).done;
  assert.equal(result.code, 0, result.stdout + result.stderr);
  const events = result.stdout.trim().split('\n').map(line => { try { return JSON.parse(line); } catch { return null; } }).filter(Boolean);
  assert.ok(events.some(e => e.type === 'dag_merge' && e.verified === true));
  const parent = events.find(e => e.type === 'session').id;
  const childDir = join(f.data, 'goon', 'subagents', parent);
  const transcripts = await Promise.all((await readdir(childDir)).filter(n => n.endsWith('.json')).map(n => readFile(join(childDir, n), 'utf8')));
  assert(transcripts.some(text => text.includes('not available')), 'the child transcript records the hidden-tool denial');
  assert.equal(await readFile(join(f.cwd, 'a.txt'), 'utf8'), 'ok');
});

test('/dag is refused when multiAgentMode is off', async t => {
  const f = await fixture(t);
  await mkdir(join(f.config, 'goon'), { recursive: true });
  await writeFile(join(f.config, 'goon', 'config.json'), JSON.stringify({ multiAgentMode: 'off' }));
  const p = await provider(t, (body, response) => sse(response, { text: 'never called' }));
  const result = await cli(f, p.url, ['--no-tui', '--json', 'dag', 'write files']).done;
  assert.equal(result.code, 0, result.stdout + result.stderr);
  assert.equal(p.requests.length, 0, 'no model request is made when /dag is disabled');
  assert.match(result.stderr, /disabled/);
});

test('multiAgent config keys parse with the documented defaults', async t => {
  const f = await fixture(t);
  const previous = process.env.XDG_CONFIG_HOME;
  process.env.XDG_CONFIG_HOME = f.config;
  t.after(() => {
    if (previous === undefined) delete process.env.XDG_CONFIG_HOME;
    else process.env.XDG_CONFIG_HOME = previous;
  });
  await mkdir(join(f.config, 'goon'), { recursive: true });
  await writeFile(join(f.config, 'goon', 'config.json'), '{}');
  const config = await loadConfig();
  assert.equal(config.multiAgentMode, 'manual');
  assert.equal(config.multiAgentReworkCap, 3);
  assert.equal(config.multiAgentMaxConcurrency, 2);
  assert.deepEqual(config.multiAgentMcpTools, []);

  await writeFile(join(f.config, 'goon', 'config.json'), JSON.stringify({
    multiAgentMode: 'auto',
    multiAgentReworkCap: 5,
    multiAgentMaxConcurrency: 4,
    multiAgentMcpTools: ['mcp__fs__read'],
  }));
  const tuned = await loadConfig();
  assert.equal(tuned.multiAgentMode, 'auto');
  assert.equal(tuned.multiAgentReworkCap, 5);
  assert.deepEqual(tuned.multiAgentMcpTools, ['mcp__fs__read']);
  await writeFile(join(f.config, 'goon', 'config.json'), JSON.stringify({ multiAgentMode: 'sometimes' }));
  await assert.rejects(() => loadConfig(), /multiAgentMode/);
});
