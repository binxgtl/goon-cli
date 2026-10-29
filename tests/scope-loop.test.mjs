import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile, rm, mkdtemp, realpath } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { CostLedger } from '../dist/cost.js';
import { budgetStopReason, runScopeLoop, scopeRoundCap } from '../dist/coord.js';
import { WorkspaceFiles } from '../dist/files.js';
import { VerificationApprovalError, DEFAULT_VERIFY_POLICY } from '../dist/verify.js';

const verifyConfig = (extra = {}) => ({
  enabled: true,
  commands: ['npm test'],
  timeoutMs: 30_000,
  maxRepairRounds: 2,
  policy: DEFAULT_VERIFY_POLICY,
  ...extra,
});

const fakeRunner = (codes) => {
  const calls = [];
  let index = 0;
  return {
    calls,
    run: async (command, args, opts) => {
      calls.push({ command, args, cwd: opts.cwd });
      const code = codes[Math.min(index, codes.length - 1)];
      index++;
      return { code, output: code === 0 ? 'all tests passed' : `test.ts:3: error: expected ${index} (token: supersecret)` };
    },
  };
};

async function tempWorkspace(t) {
  const dir = await realpath(await mkdtemp(join(tmpdir(), 'goon-scope-')));
  t.after(() => rm(dir, { recursive: true, force: true, maxRetries: 5, retryDelay: 50 }));
  return dir;
}

const exists = async (path) => readFile(path, 'utf8').then(() => true, () => false);

test('scopeRoundCap: min(maxRepairRounds + 1, 4)', () => {
  assert.equal(scopeRoundCap(0), 1);
  assert.equal(scopeRoundCap(1), 2);
  assert.equal(scopeRoundCap(2), 3);
  assert.equal(scopeRoundCap(10), 4);
});

test('scope loop: fail twice then pass keeps the workspace and approves exact commands', async t => {
  const cwd = await tempWorkspace(t);
  const files = new WorkspaceFiles(cwd);
  const runner = fakeRunner([1, 1, 0]);
  const approvals = [];
  const events = [];
  const attempted = [];
  const result = await runScopeLoop(
    'make the tests pass',
    {
      attempt: async (context) => {
        attempted.push({ round: context.round, related: [...context.relatedFiles], verifyOutput: context.verifyOutput, commands: [...context.commands] });
        await files.write(`fix-${context.round}.ts`, `export const r${context.round} = 1;\n`);
        return `attempt ${context.round}`;
      },
    },
    {
      cwd,
      files,
      signal: AbortSignal.timeout(8000),
      verifyConfig: verifyConfig(),
      runner,
      approve: plan => {
        approvals.push(plan.commands.map(command => command.display));
        return true;
      },
      emit: event => events.push(event),
    },
  );
  assert.equal(result.ok, true, JSON.stringify(result));
  assert.equal(result.rounds, 3);
  assert.equal(runner.calls.length, 3);
  assert.deepEqual(runner.calls.map(call => call.command), ['npm', 'npm', 'npm']);
  assert.deepEqual(runner.calls.map(call => call.args), [['test'], ['test'], ['test']]);
  assert.deepEqual(approvals, [['npm test'], ['npm test'], ['npm test']], 'approve sees the exact display commands');
  assert.deepEqual(result.changedFiles, ['fix-1.ts', 'fix-2.ts', 'fix-3.ts']);
  // Round 2+ sees the failure output and the related files of the previous round.
  assert.equal(attempted[1].round, 2);
  assert.match(attempted[1].verifyOutput, /Verification failed/);
  assert.deepEqual(attempted[1].related, ['fix-1.ts']);
  assert.deepEqual(attempted[1].commands, ['npm test']);
  const actions = events.map(event => `${event.round}:${event.action}`);
  assert.deepEqual(actions, ['1:expand', '2:expand', '3:contract'], JSON.stringify(events));
  assert.equal(result.rollback, undefined);
  // Pass keeps every attempt's changes on disk.
  assert.equal(await readFile(join(cwd, 'fix-3.ts'), 'utf8'), 'export const r3 = 1;\n');
});

test('scope loop: secrets are redacted in the expanded verify output', async t => {
  const cwd = await tempWorkspace(t);
  const files = new WorkspaceFiles(cwd);
  const runner = fakeRunner([1, 0]);
  const seenOutputs = [];
  await runScopeLoop(
    'fix it',
    {
      attempt: async (context) => {
        seenOutputs.push(context.verifyOutput);
        await files.write(`a-${context.round}.ts`, `export const a = ${context.round};\n`);
        return 'ok';
      },
    },
    {
      cwd,
      files,
      signal: AbortSignal.timeout(8000),
      verifyConfig: verifyConfig(),
      runner,
      approve: () => true,
      secrets: ['supersecret'],
    },
  );
  assert.equal(seenOutputs[0], '', 'round 1 has no previous verification output');
  assert.doesNotMatch(seenOutputs[1], /supersecret/);
  assert.match(seenOutputs[1], /\[redacted\]/);
});

// Regression (reviewer finding): ScopeLoopResult.verdict crossed the module boundary unredacted
// while VerifyIssue.message/stage.output carry raw command output.
test('scope loop: the returned verdict is redacted (literal secret, bearer, key-value)', async t => {
  const cwd = await tempWorkspace(t);
  const files = new WorkspaceFiles(cwd);
  const rawOutput = 'token: supersecret\nAuthorization: Bearer sk-live-abcdef123456\nnote: LITERALSECRETVALUE';
  const runner = {
    calls: [],
    run: async () => ({ code: 1, output: rawOutput }),
  };
  const result = await runScopeLoop(
    'fix it',
    {
      attempt: async (context) => {
        await files.write(`r-${context.round}.ts`, 'x\n');
        return 'attempted';
      },
    },
    { cwd, files, signal: AbortSignal.timeout(8000), verifyConfig: verifyConfig(), runner, approve: () => true, secrets: ['LITERALSECRETVALUE'] },
  );
  assert.equal(result.ok, false);
  assert.ok(result.verdict, 'exhausted rounds return the verdict');
  const rendered = JSON.stringify(result.verdict);
  assert.equal(rendered.includes('LITERALSECRETVALUE'), false, `literal secret leaked: ${rendered}`);
  assert.equal(rendered.includes('supersecret'), false, `key-value secret leaked: ${rendered}`);
  assert.equal(rendered.includes('sk-live-abcdef123456'), false, `bearer token leaked: ${rendered}`);
  // Even without any literal secret the shared bearer/key-value patterns scrub credential output.
  assert.match(rendered, /\[redacted\]/);
});

test('scope loop: fail-closed without approve — commands never run and writes roll back', async t => {
  const cwd = await tempWorkspace(t);
  const files = new WorkspaceFiles(cwd);
  const runner = fakeRunner([0]);
  await assert.rejects(
    runScopeLoop(
      'fix it',
      {
        attempt: async (context) => {
          await files.write(`leak-${context.round}.ts`, 'x\n');
          return 'attempted';
        },
      },
      { cwd, files, signal: AbortSignal.timeout(8000), verifyConfig: verifyConfig(), runner },
    ),
    error => error instanceof VerificationApprovalError,
  );
  assert.equal(runner.calls.length, 0, 'no verification command ran without approval');
  assert.equal(await exists(join(cwd, 'leak-1.ts')), false, 'the attempt was rolled back');
});

test('scope loop: approval denial is a skip, not a failure, and keeps changes', async t => {
  const cwd = await tempWorkspace(t);
  const files = new WorkspaceFiles(cwd);
  const runner = fakeRunner([0]);
  const events = [];
  const result = await runScopeLoop(
    'fix it',
    {
      attempt: async (context) => {
        await files.write(`keep-${context.round}.ts`, 'x\n');
        return 'attempted';
      },
    },
    {
      cwd,
      files,
      signal: AbortSignal.timeout(8000),
      verifyConfig: verifyConfig(),
      runner,
      approve: () => false,
      emit: event => events.push(event),
    },
  );
  assert.equal(runner.calls.length, 0);
  assert.equal(result.ok, true, 'a denied plan is a skip verdict (pass), matching §5.1');
  assert.equal(result.verdict?.severity, 'skip');
  assert.equal(events.at(-1).action, 'contract');
  assert.equal(await readFile(join(cwd, 'keep-1.ts'), 'utf8'), 'x\n', 'skip does not roll back');
});

test('scope loop: AST-only plans run without an approve callback', async t => {
  const cwd = await tempWorkspace(t);
  const files = new WorkspaceFiles(cwd);
  const result = await runScopeLoop(
    'fix it',
    {
      attempt: async () => {
        await files.write('a.ts', 'export const a = 1;\n');
        return 'attempted';
      },
    },
    {
      cwd,
      files,
      signal: AbortSignal.timeout(8000),
      // No commands: the node matrix resolves nothing runnable for a bare fixture, so the plan
      // is AST-only and must not require approval.
      verifyConfig: verifyConfig({ commands: [] }),
      runner: fakeRunner([0]),
    },
  );
  assert.equal(result.ok, true, JSON.stringify(result));
  assert.equal(result.verdict?.severity === 'pass' || result.verdict?.severity === 'warn', true);
});

test('scope loop: token budget stops before the first attempt', async t => {
  const cwd = await tempWorkspace(t);
  const files = new WorkspaceFiles(cwd);
  let attempts = 0;
  const events = [];
  const result = await runScopeLoop(
    'fix it',
    { attempt: async () => { attempts++; return 'x'; } },
    {
      cwd,
      files,
      signal: AbortSignal.timeout(8000),
      verifyConfig: verifyConfig(),
      runner: fakeRunner([0]),
      budget: { spent: 100, limit: 100 },
      emit: event => events.push(event),
    },
  );
  assert.equal(result.ok, false);
  assert.equal(attempts, 0, 'the exhausted budget prevents any attempt');
  assert.match(result.reason, /token budget reached/);
  assert.deepEqual(events.map(event => event.action), ['stop']);
  assert.deepEqual(result.rollback, { restored: [], deleted: [], conflicts: [], pruned: [] });
});

test('scope loop: budget exhausted mid-loop stops without exceeding the shared budget', async t => {
  const cwd = await tempWorkspace(t);
  const files = new WorkspaceFiles(cwd);
  const budget = { spent: 0, limit: 100 };
  let attempts = 0;
  const events = [];
  const result = await runScopeLoop(
    'fix it',
    {
      attempt: async (context) => {
        attempts++;
        budget.spent += 60; // the model turn accounts its usage into the shared budget
        await files.write(`w-${context.round}.ts`, 'x\n');
        return 'attempted';
      },
    },
    {
      cwd,
      files,
      signal: AbortSignal.timeout(8000),
      verifyConfig: verifyConfig(),
      runner: fakeRunner([1, 1, 1]),
      budget,
      approve: () => true,
      emit: event => events.push(event),
    },
  );
  assert.equal(result.ok, false);
  assert.equal(attempts, 2, 'round 3 never starts: 120/100 tokens would exceed the budget');
  assert.match(result.reason, /token budget reached/);
  assert.deepEqual(events.map(event => `${event.round}:${event.action}`), ['1:expand', '2:expand', '3:stop'], JSON.stringify(events));
  // Terminal failure rolls every accumulated snapshot back per §5.1.
  assert.deepEqual(result.rollback.deleted.sort(), ['w-1.ts', 'w-2.ts']);
  assert.equal(await exists(join(cwd, 'w-1.ts')), false, 'w-1.ts rolled back');
  assert.equal(await exists(join(cwd, 'w-2.ts')), false, 'w-2.ts rolled back');
});

test('scope loop: exhausted rounds roll back all attempts and report the verdict', async t => {
  const cwd = await tempWorkspace(t);
  const files = new WorkspaceFiles(cwd);
  const runner = fakeRunner([1]);
  const result = await runScopeLoop(
    'fix it',
    {
      attempt: async (context) => {
        await files.write(`bad-${context.round}.ts`, 'x\n');
        return 'attempted';
      },
    },
    { cwd, files, signal: AbortSignal.timeout(8000), verifyConfig: verifyConfig(), runner, approve: () => true },
  );
  assert.equal(result.ok, false);
  assert.equal(result.rounds, 3, 'maxRepairRounds 2 -> 3 rounds');
  assert.match(result.reason, /verification still failing after 3 round\(s\)/);
  assert.equal(result.rollback.deleted.length, 3, 'every failed attempt was rolled back');
  assert.equal(await exists(join(cwd, 'bad-1.ts')), false);
});

test('scope loop: one file edited by several attempts rolls back to the pre-loop state', async t => {
  const cwd = await tempWorkspace(t);
  const files = new WorkspaceFiles(cwd);
  const result = await runScopeLoop(
    'fix it',
    {
      attempt: async (context) => {
        await files.write('a.ts', `export const v = ${context.round};\n`);
        return 'attempted';
      },
    },
    { cwd, files, signal: AbortSignal.timeout(8000), verifyConfig: verifyConfig(), runner: fakeRunner([1]), approve: () => true },
  );
  assert.equal(result.ok, false);
  // a.ts never existed before the loop: every attempt touched it, and the reverse-order restore
  // must end at the pre-loop state (absent), not at an intermediate attempt's content.
  assert.equal(await exists(join(cwd, 'a.ts')), false, `rollback restored the pre-loop state (rollback=${JSON.stringify(result.rollback)})`);
  assert.deepEqual(result.rollback.conflicts, [], 'no phantom conflict from mismatched snapshots');
});

test('scope loop: no tracked changes stops the loop', async t => {
  const cwd = await tempWorkspace(t);
  const files = new WorkspaceFiles(cwd);
  const events = [];
  const result = await runScopeLoop(
    'fix it',
    { attempt: async () => 'nothing to do' },
    { cwd, files, signal: AbortSignal.timeout(8000), verifyConfig: verifyConfig(), runner: fakeRunner([0]), emit: event => events.push(event) },
  );
  assert.equal(result.ok, false);
  assert.match(result.reason, /no tracked file changes/);
  assert.deepEqual(events.map(event => event.action), ['stop']);
});

test('scope loop: cancellation propagates and rolls back', async t => {
  const cwd = await tempWorkspace(t);
  const files = new WorkspaceFiles(cwd);
  const controller = new AbortController();
  await assert.rejects(
    runScopeLoop(
      'fix it',
      {
        attempt: async (context, signal) => {
          await files.write('partial.ts', 'x\n');
          controller.abort(new Error('user cancelled'));
          signal.throwIfAborted();
          return 'unreachable';
        },
      },
      { cwd, files, signal: controller.signal, verifyConfig: verifyConfig(), runner: fakeRunner([0]) },
    ),
    /user cancelled/,
  );
  assert.equal(await exists(join(cwd, 'partial.ts')), false, 'partial writes rolled back');
});

test('budgetStopReason: token limit first, then ledger cost/time caps', () => {
  assert.equal(budgetStopReason(undefined), undefined);
  assert.match(budgetStopReason({ spent: 5, limit: 5 }), /token budget reached/);
  assert.equal(budgetStopReason({ spent: 4, limit: 5 }), undefined);
  const ledger = new CostLedger({ models: { m: { inputPer1M: 1, outputPer1M: 1 } } }, 'm');
  const costBudget = { spent: 0, limit: 0, costLimit: 10, costSpent: 10, ledger };
  assert.match(budgetStopReason(costBudget), /cost budget reached/);
  const timeBudget = { spent: 0, limit: 0, timeLimitMs: 1000, startedAt: Date.now() - 2000, ledger };
  assert.match(budgetStopReason(timeBudget), /time budget reached/);
  const okBudget = { spent: 0, limit: 0, costLimit: 10, costSpent: 1, ledger, timeLimitMs: 60_000, startedAt: Date.now() };
  assert.equal(budgetStopReason(okBudget), undefined);
});
