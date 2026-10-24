import test from 'node:test';
import assert from 'node:assert/strict';
import { rm, mkdtemp, readFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import {
  CANDIDATE_DEGRADE_RATIO,
  MAX_CANDIDATES,
  candidateCap,
  costDegrade,
  reflect,
  rerank,
  runCandidates,
} from '../dist/coord.js';
import { WorkspaceFiles } from '../dist/files.js';

const verdict = (severity, warnings = [], reason = severity) => ({
  pass: severity !== 'fail',
  severity,
  stages: [],
  errors: severity === 'fail' ? [{ stage: 'test', path: 'a.ts', message: 'boom' }] : [],
  warnings,
  reason,
});

const emptyWorkspace = async (t) => {
  const dir = await mkdtemp(join(tmpdir(), 'goon-cand-'));
  t.after(() => rm(dir, { recursive: true, force: true, maxRetries: 5, retryDelay: 50 }));
  return new WorkspaceFiles(dir);
};

test('candidateCap: clamps to MAX_CANDIDATES (2), minimum 1', () => {
  assert.equal(MAX_CANDIDATES, 2);
  assert.equal(candidateCap(), 2);
  assert.equal(candidateCap(undefined), 2);
  assert.equal(candidateCap(5), 2);
  assert.equal(candidateCap(2), 2);
  assert.equal(candidateCap(1), 1);
  assert.equal(candidateCap(0), 1);
  assert.equal(candidateCap(-3), 1);
  assert.equal(candidateCap(Number.NaN), 2);
});

test('reflect: fold a verdict into a rankable candidate (warn and skip pass, fail does not)', () => {
  const base = { index: 0, task: 't', report: 'r', costUsd: 0.5, backups: [] };
  assert.equal(reflect(base, verdict('fail')).passed, false);
  assert.equal(reflect(base, verdict('pass')).passed, true);
  const warn = reflect(base, verdict('warn', [{ stage: 'lint', path: 'a.ts', message: 'style' }]));
  assert.equal(warn.passed, true);
  assert.equal(warn.warnings, 1);
  assert.equal(reflect(base, verdict('skip')).passed, true, 'skip counts as pass per §5.1');
  assert.equal(reflect(base, verdict('fail')).reason, 'fail');
});

test('rerank: pass > warnings > cost, ties keep generation order', () => {
  const mk = (index, passed, warnings, costUsd) => ({ index, task: 't', report: `r${index}`, costUsd, passed, warnings, reason: '', backups: [] });
  const ranked = rerank([
    mk(0, true, 2, 0.1),   // pass with warnings
    mk(1, false, 0, 0.01), // failed
    mk(2, true, 0, 0.9),   // clean pass, expensive
    mk(3, true, 2, 0.05),  // pass with warnings, cheaper
    mk(4, false, 0, 0.0),  // failed
  ]);
  assert.deepEqual(ranked.map(candidate => candidate.index), [2, 3, 0, 4, 1], 'clean pass first, then warn passes by cost, then failures by cost');
  // Deterministic tie-break: identical candidates order by generation index (lower first).
  const tied = rerank([mk(7, true, 0, 0.2), mk(3, true, 0, 0.2), mk(5, true, 0, 0.2)]);
  assert.deepEqual(tied.map(candidate => candidate.index), [3, 5, 7]);
  // Input array is not mutated.
  assert.equal(rerank([]).length, 0);
});

test('costDegrade: below 20% remaining cost or time budget degrades to one candidate', () => {
  assert.deepEqual(costDegrade(undefined), { degrade: false, reason: '' });
  assert.equal(costDegrade({ spent: 0, limit: 0 }).degrade, false, 'no caps, no degrade');
  // 15% cost remaining -> degrade; 50% -> no.
  const low = costDegrade({ spent: 0, limit: 0, costLimit: 100, costSpent: 85 });
  assert.equal(low.degrade, true);
  assert.match(low.reason, /cost budget nearly exhausted/);
  assert.equal(costDegrade({ spent: 0, limit: 0, costLimit: 100, costSpent: 50 }).degrade, false);
  // Exactly 20% remaining does not degrade (strictly-below threshold).
  assert.equal(costDegrade({ spent: 0, limit: 0, costLimit: 100, costSpent: 100 * (1 - CANDIDATE_DEGRADE_RATIO) }).degrade, false);
  // 10% time remaining -> degrade.
  const time = costDegrade({ spent: 0, limit: 0, timeLimitMs: 1000, startedAt: Date.now() - 900 });
  assert.equal(time.degrade, true);
  assert.match(time.reason, /time budget nearly exhausted/);
});

test('runCandidates: candidate 1 fails, candidate 2 passes -> candidate 2 wins and candidate 1 is rolled back', async t => {
  const files = await emptyWorkspace(t);
  const events = [];
  const generated = [];
  const result = await runCandidates(
    'ship the fix',
    {
      generate: async (task, index) => {
        generated.push(index);
        await files.write(`cand-${index}.ts`, `export const v${index} = ${index === 0 ? 'broken' : 'fixed'};\n`);
        return `report ${index}`;
      },
      verify: async (changed) => {
        assert.deepEqual(changed, [`cand-${generated.at(-1)}.ts`], 'verify sees only the current candidate');
        return verdict(generated.length === 1 ? 'fail' : 'pass');
      },
    },
    { files, signal: AbortSignal.timeout(8000), emit: event => events.push(event) },
  );
  assert.equal(result.ok, true);
  assert.equal(result.winner.index, 1);
  assert.deepEqual(generated, [0, 1], 'the failed candidate triggered a second attempt');
  assert.equal(result.candidates.length, 2);
  assert.equal(result.ranked[0].index, 1, 'rerank puts the passing candidate first');
  assert.equal(await readFile(join(files.root, 'cand-0.ts'), 'utf8').then(() => true, () => false), false, 'candidate 1 rolled back');
  assert.equal(await readFile(join(files.root, 'cand-1.ts'), 'utf8'), 'export const v1 = fixed;\n', 'winner kept on disk');
  assert.deepEqual(events, [], 'no cost_degrade when the budget is healthy');
  // The winner's snapshot is surfaced in the caller's turn window for checkpointing.
  assert.deepEqual(files.turnChangedFiles(), ['cand-1.ts']);
});

test('runCandidates: both candidates fail -> everything rolled back, ok false', async t => {
  const files = await emptyWorkspace(t);
  const result = await runCandidates(
    'ship the fix',
    {
      generate: async (task, index) => {
        await files.write(`bad-${index}.ts`, 'export const x = 1;\n');
        return `report ${index}`;
      },
      verify: async () => verdict('fail', [], 'tests fail'),
    },
    { files, signal: AbortSignal.timeout(8000) },
  );
  assert.equal(result.ok, false);
  assert.equal(result.candidates.length, 2);
  assert.equal(result.winner, undefined);
  assert.match(result.reason, /all 2 candidate\(s\) failed verification/);
  for (const index of [0, 1]) {
    assert.equal(await readFile(join(files.root, `bad-${index}.ts`), 'utf8').then(() => true, () => false), false, `bad-${index}.ts rolled back`);
  }
  assert.deepEqual(files.turnChangedFiles(), [], 'no snapshot left in the turn window');
});

test('runCandidates: low budget degrades to one candidate with cost_degrade event and reduced effort', async t => {
  const files = await emptyWorkspace(t);
  const events = [];
  const efforts = [];
  const budget = { spent: 0, limit: 0, costLimit: 100, costSpent: 90 };
  const result = await runCandidates(
    'ship the fix',
    {
      generate: async (task, index, opts) => {
        efforts.push(opts.degraded);
        await files.write(`one-${index}.ts`, 'export const x = 1;\n');
        return 'report';
      },
      verify: async () => verdict('pass'),
    },
    { files, signal: AbortSignal.timeout(8000), budget, candidates: 2, emit: event => events.push(event) },
  );
  assert.equal(result.ok, true);
  assert.equal(result.degraded, true);
  assert.deepEqual(efforts, [true], 'exactly one candidate, generated at reduced effort');
  assert.equal(events.length, 1);
  assert.equal(events[0].type, 'cost_degrade');
  assert.equal(events[0].candidates, 1);
  assert.match(events[0].reason, /cost budget nearly exhausted/);
});

test('runCandidates: hard budget cap stops generation before a candidate runs', async t => {
  const files = await emptyWorkspace(t);
  let generated = 0;
  const result = await runCandidates(
    'ship the fix',
    {
      generate: async () => { generated++; return 'report'; },
      verify: async () => verdict('fail'),
    },
    { files, signal: AbortSignal.timeout(8000), budget: { spent: 50, limit: 50 } },
  );
  assert.equal(result.ok, false);
  assert.equal(generated, 0, 'the exhausted token budget prevents any candidate');
  assert.match(result.reason, /token budget reached/);
});

test('runCandidates: a candidate whose hooks throw is recorded as failed and rolled back', async t => {
  const files = await emptyWorkspace(t);
  const calls = [];
  const result = await runCandidates(
    'ship the fix',
    {
      generate: async (task, index) => {
        calls.push(`generate-${index}`);
        if (index === 0) {
          await files.write('half.ts', 'export const half = 1;\n');
          throw new Error('provider exploded');
        }
        await files.write('full.ts', 'export const full = 1;\n');
        return 'report 1';
      },
      verify: async () => verdict('pass'),
    },
    { files, signal: AbortSignal.timeout(8000) },
  );
  assert.equal(result.ok, true);
  assert.deepEqual(calls, ['generate-0', 'generate-1']);
  assert.equal(result.candidates[0].passed, false);
  assert.match(result.candidates[0].reason, /provider exploded/);
  assert.equal(result.winner.index, 1);
  assert.equal(await readFile(join(files.root, 'half.ts'), 'utf8').then(() => true, () => false), false, 'partial candidate rolled back');
  assert.equal(await readFile(join(files.root, 'full.ts'), 'utf8'), 'export const full = 1;\n');
});
