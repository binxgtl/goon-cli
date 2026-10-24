import test from 'node:test';
import assert from 'node:assert/strict';
import {
  AGENT_ROLES,
  DEFAULT_MAX_REWORK_CYCLES,
  PlanDeadlockError,
  PlanValidationError,
  ROLE_TOOLSETS,
  Scheduler,
  applyReviewVerdict,
  diffPaths,
  merge,
  planFromPlanner,
  roleToolset,
  validatePlan,
} from '../dist/coord.js';

function makePlan(todos, extra = {}) {
  return {
    id: 'plan-1',
    goal: 'ship it',
    createdAt: '2026-01-01T00:00:00.000Z',
    todos: todos.map(todo => ({
      acceptance: '',
      dependsOn: [],
      status: 'pending',
      artifacts: [],
      reworkCycles: 0,
      ...todo,
    })),
    ...extra,
  };
}

test('validatePlan accepts a well-formed DAG', () => {
  const plan = makePlan([
    { id: 't1', title: 'explore', role: 'researcher' },
    { id: 't2', title: 'implement', role: 'coder', dependsOn: ['t1'] },
    { id: 't3', title: 'verify', role: 'tester', dependsOn: ['t2'] },
  ]);
  assert.doesNotThrow(() => validatePlan(plan));
});

test('validatePlan rejects duplicate, empty and unknown-dependency ids', () => {
  assert.throws(() => validatePlan(makePlan([
    { id: 't1', title: 'a', role: 'coder' },
    { id: 't1', title: 'b', role: 'coder' },
  ])), error => error instanceof PlanValidationError && /duplicate/i.test(error.message));
  assert.throws(() => validatePlan(makePlan([
    { id: '  ', title: 'a', role: 'coder' },
  ])), PlanValidationError);
  assert.throws(() => validatePlan(makePlan([
    { id: 't1', title: 'a', role: 'coder', dependsOn: ['ghost'] },
  ])), error => error instanceof PlanValidationError && /unknown/i.test(error.message));
  assert.throws(() => validatePlan(makePlan([])), PlanValidationError);
});

test('validatePlan rejects self-dependencies and cycles', () => {
  assert.throws(() => validatePlan(makePlan([
    { id: 't1', title: 'a', role: 'coder', dependsOn: ['t1'] },
  ])), error => error instanceof PlanValidationError && /itself/i.test(error.message));
  assert.throws(() => validatePlan(makePlan([
    { id: 't1', title: 'a', role: 'coder', dependsOn: ['t2'] },
    { id: 't2', title: 'b', role: 'coder', dependsOn: ['t1'] },
  ])), error => error instanceof PlanValidationError && /cycle/i.test(error.message));
  // Diamond (acyclic) is fine.
  assert.doesNotThrow(() => validatePlan(makePlan([
    { id: 't1', title: 'a', role: 'coder' },
    { id: 't2', title: 'b', role: 'coder', dependsOn: ['t1'] },
    { id: 't3', title: 'c', role: 'coder', dependsOn: ['t1'] },
    { id: 't4', title: 'd', role: 'tester', dependsOn: ['t2', 't3'] },
  ])));
});

test('validatePlan rejects invalid roles', () => {
  assert.throws(() => validatePlan(makePlan([
    { id: 't1', title: 'a', role: 'wizard' },
  ])), error => error instanceof PlanValidationError && /role/i.test(error.message));
});

test('planFromPlanner applies defaults and dedupes deps', () => {
  const plan = planFromPlanner({
    goal: 'g',
    todos: [{ id: 't1', title: 'a', role: 'coder', dependsOn: ['t2', 't2'] }, { id: 't2', title: 'b', role: 'researcher' }],
  }, 'plan-x', '2026-01-01T00:00:00.000Z');
  assert.equal(plan.id, 'plan-x');
  assert.deepEqual(plan.todos[0].dependsOn, ['t2']);
  assert.equal(plan.todos[0].status, 'pending');
  assert.equal(plan.todos[0].reworkCycles, 0);
  assert.throws(() => planFromPlanner({ goal: 'g', todos: [{ id: 't1', title: 'a', role: 'wizard' }] }, 'p'), /role/i);
});

test('scheduler: depsMet only when all deps done; failed dep blocks dependents transitively', () => {
  const plan = makePlan([
    { id: 't1', title: 'a', role: 'coder' },
    { id: 't2', title: 'b', role: 'coder', dependsOn: ['t1'] },
    { id: 't3', title: 'c', role: 'tester', dependsOn: ['t2'] },
  ]);
  const scheduler = new Scheduler(plan);
  assert.equal(scheduler.readyTodos().map(t => t.id).join(','), 't1');
  scheduler.markDone('t1');
  assert.equal(scheduler.readyTodos().map(t => t.id).join(','), 't2');
  assert.equal(scheduler.depsMet(plan.todos[1]), true);
  scheduler.markDone('t2');
  assert.equal(scheduler.readyTodos().map(t => t.id).join(','), 't3');
  // A failed dependency blocks (and the block propagates down the chain).
  const plan2 = makePlan([
    { id: 'a', title: 'a', role: 'coder' },
    { id: 'b', title: 'b', role: 'coder', dependsOn: ['a'] },
    { id: 'c', title: 'c', role: 'tester', dependsOn: ['b'] },
  ]);
  const s2 = new Scheduler(plan2);
  s2.markDone('a');
  const blocked = s2.markFailed('b');
  assert.deepEqual(blocked, ['c']);
  assert.equal(plan2.todos[2].status, 'blocked');
  assert.equal(s2.readyTodos().length, 0);
  assert.deepEqual(s2.checkDeadlock(), []);
});

test('scheduler dispatchReady respects write-serial and read-parallel bounds', () => {
  const plan = makePlan([
    { id: 'r1', title: 'a', role: 'researcher' },
    { id: 'r2', title: 'b', role: 'reviewer' },
    { id: 'r3', title: 'c', role: 'researcher' },
    { id: 'w1', title: 'd', role: 'coder' },
    { id: 'w2', title: 'e', role: 'coder' },
  ]);
  const scheduler = new Scheduler(plan, 1, 2);
  const first = scheduler.dispatchReady();
  // Read-only roles run in parallel up to the bound; only one coder may run at a time.
  assert.deepEqual(first.map(t => t.id).sort(), ['r1', 'r2', 'w1']);
  assert.equal(plan.todos.find(t => t.id === 'w2').status, 'pending');
  assert.equal(plan.todos.find(t => t.id === 'r3').status, 'pending');
  scheduler.markDone('w1');
  const second = scheduler.dispatchReady();
  assert.deepEqual(second.map(t => t.id), ['w2']);
  scheduler.markDone('r1');
  scheduler.markDone('r2');
  scheduler.markDone('w2');
  assert.deepEqual(scheduler.dispatchReady().map(t => t.id).sort(), ['r3']);
});

test('scheduler detects deadlock when nothing is inflight or dispatchable but todos remain pending', () => {
  const plan = makePlan([
    { id: 'a', title: 'a', role: 'coder' },
    { id: 'b', title: 'b', role: 'coder', dependsOn: ['a'] },
  ]);
  const scheduler = new Scheduler(plan);
  // Artificially leave `a` in_progress without an inflight slot (stale dispatch).
  plan.todos[0].status = 'in_progress';
  assert.deepEqual(scheduler.checkDeadlock(), ['b']);
  assert.throws(() => {
    const ids = scheduler.checkDeadlock();
    if (ids.length) throw new PlanDeadlockError(ids);
  }, PlanDeadlockError);
});

test('applyReviewVerdict counts rework and fails past the cap', () => {
  const todo = { id: 't1', reworkCycles: 0, status: 'in_progress' };
  assert.equal(applyReviewVerdict(todo, 'approved'), 'approved');
  assert.equal(todo.reworkCycles, 0);
  for (let i = 0; i < DEFAULT_MAX_REWORK_CYCLES; i++) {
    assert.equal(applyReviewVerdict(todo, 'rejected'), 'rework');
    assert.equal(todo.status, 'pending');
  }
  assert.equal(todo.reworkCycles, DEFAULT_MAX_REWORK_CYCLES);
  assert.equal(applyReviewVerdict(todo, 'rejected'), 'failed');
  assert.equal(todo.status, 'failed');
  // A custom cap overrides the default.
  const strict = { id: 't2', reworkCycles: 0, status: 'in_progress' };
  assert.equal(applyReviewVerdict(strict, 'rejected', 1), 'rework');
  assert.equal(applyReviewVerdict(strict, 'rejected', 1), 'failed');
});

test('merge joins done diffs, detects conflicts and rejects empty results', async () => {
  const plan = makePlan([
    { id: 't1', title: 'a', role: 'coder' },
    { id: 't2', title: 'b', role: 'coder' },
  ]);
  plan.todos[0].status = 'done';
  plan.todos[1].status = 'done';
  const diffA = ['--- a/src/a.ts (+1 -0)', '+++ b/src/a.ts', '@@ -1,1 +1,2 @@', ' old', '+new'].join('\n');
  const diffB = ['--- a/src/b.ts (+1 -0)', '+++ b/src/b.ts', '@@ -1,1 +1,2 @@', ' x', '+y'].join('\n');
  const ok = await merge(plan, { t1: diffA, t2: diffB });
  assert.equal(ok.verified, true);
  assert.equal(ok.reason, 'ok');
  assert.deepEqual(ok.artifacts.sort(), ['src/a.ts', 'src/b.ts']);
  assert.ok(ok.combinedDiff.includes('src/a.ts'));
  assert.ok(ok.combinedDiff.includes('src/b.ts'));

  const conflictDiff = ['--- a/src/a.ts (+1 -0)', '+++ b/src/a.ts', '@@ -1,1 +1,2 @@', ' old', '+other'].join('\n');
  const conflict = await merge(plan, { t1: diffA, t2: conflictDiff });
  assert.equal(conflict.verified, false);
  assert.equal(conflict.conflicts.length, 1);
  assert.deepEqual(conflict.conflicts[0].todos, ['t1', 't2']);
  assert.match(conflict.reason, /merge conflict/);

  const empty = await merge(plan, {});
  assert.equal(empty.verified, false);
  assert.equal(empty.reason, 'no changes');
  assert.equal((await merge(plan, {}, { allowEmpty: true })).verified, true);

  const verifyFalse = await merge(plan, { t1: diffA, t2: diffB }, { verify: () => false });
  assert.equal(verifyFalse.verified, false);
  assert.equal(verifyFalse.reason, 'verification failed');
  const verifyTrue = await merge(plan, { t1: diffA, t2: diffB }, { verify: () => true });
  assert.equal(verifyTrue.verified, true);
  // Not-done todos never contribute.
  plan.todos[1].status = 'pending';
  const onlyDone = await merge(plan, { t1: diffA, t2: conflictDiff });
  assert.equal(onlyDone.verified, true);
});

test('merge treats an empty diff on a shared file as non-conflicting', async () => {
  const plan = makePlan([
    { id: 't1', title: 'a', role: 'coder' },
    { id: 't2', title: 'b', role: 'coder' },
  ]);
  plan.todos[0].status = 'done';
  plan.todos[1].status = 'done';
  const diffA = ['--- a/shared.ts (+1 -0)', '+++ b/shared.ts', '@@ -1,1 +1,2 @@', ' old', '+new'].join('\n');
  // t2 lists shared.ts in artifacts but its diff is empty.
  const result = await merge(plan, { t1: diffA, t2: '' });
  assert.equal(result.verified, true);
  assert.equal(result.conflicts.length, 0);
});

test('diffPaths parses unified diff headers including new/deleted kinds', () => {
  const diff = [
    '--- a/src/new.ts (+3 -0)',
    '+++ b/src/new.ts',
    '@@ -0,0 +1,3 @@',
    '+a',
    '--- a/src/gone.ts (deleted +0 -3)',
    '+++ b/src/gone.ts',
    '@@ -1,3 +0,0 @@',
    '-b',
  ].join('\n');
  assert.deepEqual(diffPaths(diff).sort(), ['src/gone.ts', 'src/new.ts']);
});

test('ROLE_TOOLSETS keep read-only roles read-only and never expose MCP or peer tools by default', () => {
  for (const role of ['planner', 'reviewer', 'researcher']) {
    for (const tool of ROLE_TOOLSETS[role]) {
      assert.match(tool, /^(read_file|list_files|search)$/, `${role} must be read-only`);
    }
  }
  assert.ok(ROLE_TOOLSETS.coder.includes('bash'));
  assert.ok(ROLE_TOOLSETS.coder.includes('write_file'));
  assert.ok(ROLE_TOOLSETS.tester.includes('bash'));
  assert.ok(!ROLE_TOOLSETS.tester.includes('write_file'));
  for (const role of AGENT_ROLES) {
    const set = roleToolset(role, []);
    assert.ok(!set.some(tool => tool.startsWith('mcp__')), `${role} must not get MCP by default`);
    assert.ok(!set.includes('spawn_agent') && !set.includes('peer_send'), `${role} must not get peer/delegation tools`);
  }
  assert.deepEqual(roleToolset('tester', ['mcp__fs__read']), [...ROLE_TOOLSETS.tester, 'mcp__fs__read']);
});

test('runDag validate-failure path emits dag_plan_invalid and never spawns', async () => {
  const { runDag } = await import('../dist/coord.js');
  const events = [];
  let spawned = 0;
  const files = { beginTurn() {}, takeTurnBackups() { return []; }, restoreBackups: async () => ({ restored: [], deleted: [], conflicts: [], pruned: [] }) };
  await assert.rejects(
    runDag('g', {
      planner: async () => ({ goal: 'g', todos: [{ id: 't1', title: 'a', role: 'coder', dependsOn: ['t9'] }] }),
      spawn: async () => { spawned++; return 'report'; },
    }, {
      config: {},
      signal: AbortSignal.timeout(2000),
      files,
      emit: event => events.push(event),
    }),
    PlanValidationError,
  );
  assert.equal(spawned, 0);
  assert.ok(events.some(event => event.type === 'dag_plan_invalid'));
});
