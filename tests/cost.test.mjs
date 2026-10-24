import test from 'node:test';
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import {
  CostLedger,
  CostPricingError,
  createRunBudget,
  formatUsd,
  normalizePricing,
  toUsageSplit,
} from '../dist/cost.js';
import { Harness } from '../dist/harness.js';
import { ToolRegistry } from '../dist/tools.js';

const price = { inputPer1M: 10, outputPer1M: 20, cachedInputPer1M: 1, cacheWritePer1M: 2 };

test('cost: computeCost splits uncached, cached, cache-write and output', () => {
  const ledger = new CostLedger({ models: { m: price } }, 'm');
  const cost = ledger.computeCost({
    inputTokens: 1_000_000,
    outputTokens: 500_000,
    totalTokens: 1_500_000,
    cachedInputTokens: 200_000,
    cacheWriteTokens: 100_000,
  });
  assert.equal(cost, 17.4);
});

test('cost: missing cached rates fall back to the input rate', () => {
  const ledger = new CostLedger({ models: { m: { inputPer1M: 10, outputPer1M: 20 } } }, 'm');
  const cost = ledger.computeCost({ inputTokens: 1_000_000, outputTokens: 0, totalTokens: 1_000_000, cachedInputTokens: 500_000 });
  assert.equal(cost, 10);
});

test('cost: default price is used when the model has no explicit entry', () => {
  const ledger = new CostLedger({ default: { inputPer1M: 3, outputPer1M: 4 }, models: {} }, 'unknown');
  assert.equal(ledger.computeCost({ inputTokens: 1_000_000, outputTokens: 0, totalTokens: 1_000_000 }), 3);
});

test('cost: flat and nested pricing shapes normalize to the same table', () => {
  const flat = normalizePricing({ m: price, default: price });
  const nested = normalizePricing({ models: { m: price }, default: price });
  assert.deepEqual(flat.models.m, price);
  assert.deepEqual(nested.models.m, price);
  assert.deepEqual(flat.default, price);
});

test('cost: maxCostUsd with no price fails closed as CostPricingError', () => {
  assert.throws(
    () => CostLedger.fromConfig({ model: 'unknown', maxCostUsd: 1, pricing: { other: price } }),
    error => error instanceof CostPricingError && /No price configured/.test(error.message),
  );
  assert.doesNotThrow(() => CostLedger.fromConfig({ model: 'unknown', maxCostUsd: 0, pricing: { other: price } }));
});

test('cost: account uses (budget, usage, model) and mirrors ledger total', () => {
  const ledger = new CostLedger({ models: { m: price } }, 'm');
  const budget = { spent: 0, limit: 0, costSpent: 0 };
  const delta = ledger.account(budget, { inputTokens: 1_000_000, outputTokens: 0, totalTokens: 1_000_000 }, 'm');
  assert.equal(delta, 10);
  assert.equal(ledger.total, 10);
  assert.equal(budget.costSpent, 10);
});

test('cost: registerRun seeds a resumed session cost without resetting the mirror', () => {
  const ledger = new CostLedger({ models: { m: price } }, 'm');
  const budget = { spent: 0, limit: 0, costSpent: 5 };
  ledger.registerRun(budget);
  assert.equal(ledger.total, 5);
  ledger.account(budget, { inputTokens: 1_000_000, outputTokens: 0, totalTokens: 1_000_000 }, 'm');
  assert.equal(ledger.total, 15);
  assert.equal(budget.costSpent, 15);
});

test('cost: hard caps detect cost and time budgets', () => {
  const ledger = new CostLedger({ models: { m: price } }, 'm');
  const budget = { spent: 0, limit: 0, costSpent: 2, costLimit: 2, startedAt: Date.now(), timeLimitMs: 0 };
  assert.equal(ledger.hardCapExceeded(budget).exceeded, true);
  const timeBudget = { spent: 0, limit: 0, costSpent: 0, costLimit: 0, startedAt: Date.now() - 100, timeLimitMs: 10 };
  assert.equal(ledger.hardCapExceeded(timeBudget).exceeded, true);
});

test('cost: createRunBudget fills configured caps without resetting a shared budget', () => {
  const shared = { spent: 3, limit: 10, costSpent: 1 };
  const budget = createRunBudget({ runTokenBudget: 99, maxCostUsd: 5, maxTimeMs: 1000 }, shared);
  assert.equal(budget, shared);
  assert.equal(budget.spent, 3);
  assert.equal(budget.limit, 10);
  assert.equal(budget.costLimit, 5);
  assert.equal(budget.timeLimitMs, 1000);
});

test('cost: toUsageSplit reads AI SDK token details and estimates fallbacks', () => {
  const split = toUsageSplit({ inputTokens: 100, outputTokens: 20, inputTokenDetails: { cacheReadTokens: 30, cacheWriteTokens: 10 } });
  assert.deepEqual(split, { inputTokens: 100, outputTokens: 20, totalTokens: 120, cachedInputTokens: 30, cacheWriteTokens: 10 });
  const fallback = toUsageSplit(undefined, 7, 3);
  assert.deepEqual(fallback, { inputTokens: 7, outputTokens: 3, totalTokens: 10 });
});

test('cost: formatUsd never prints negative or NaN values', () => {
  assert.equal(formatUsd(1.23456), '$1.2346');
  assert.equal(formatUsd(-1), '$0.0000');
  assert.equal(formatUsd(Number.NaN), '$0.0000');
});

function harnessFixture(overrides = {}) {
  const config = {
    model: 'test-model',
    maxCostUsd: 0,
    pricing: { 'test-model': { inputPer1M: 1000, outputPer1M: 2000 } },
    ...overrides,
  };
  const session = {
    id: 'test-session',
    messages: [],
    tokens: 0,
    compactions: 0,
    costUsd: 0,
    todos: [],
    checkpoints: [],
    rejectedSignatures: {},
  };
  const store = { session, save: async () => {} };
  const tools = new ToolRegistry('/tmp', async () => true);
  const events = [];
  const harness = new Harness(config, store, tools, event => events.push(event), {});
  return { harness, session, store, tools, events, config };
}

test('cost: Harness.accountUsage updates session canonical and budget mirror (known issue 1)', () => {
  const { harness, session, events } = harnessFixture();
  const budget = { spent: 0, limit: 0, costSpent: 0 };
  harness.costLedger.registerRun(budget);
  harness.accountUsage({ inputTokens: 1000, outputTokens: 500 }, 'test-model', budget);
  assert.equal(harness.costLedger.total, 2);
  assert.equal(session.costUsd, 2);
  assert.equal(budget.costSpent, 2);
  const usage = events.find(event => event.type === 'usage');
  assert.equal(usage.costUsd, 2);
  assert.equal(usage.total, 1500);
});

test('cost: child harness never clobbers a shared budget with its own session cost (known issue 5)', () => {
  const parentLedger = new CostLedger({ models: { 'test-model': { inputPer1M: 1, outputPer1M: 0 } } }, 'test-model');
  parentLedger.total = 5;
  const shared = { spent: 0, limit: 0, costSpent: 5, ledger: parentLedger };
  const { harness: child, session: childSession } = harnessFixture();
  child.accountUsage({ inputTokens: 1_000_000, outputTokens: 0 }, 'test-model', shared);
  assert.equal(parentLedger.total, 6);
  assert.equal(shared.costSpent, 6);
  assert.equal(childSession.costUsd, 1);
  assert.equal(child.costLedger.total, 0);
});

test('cost: CLI exits 2 when maxCostUsd is set but the model has no price', () => {
  const dir = mkdtempSync(join(tmpdir(), 'goon-cost-'));
  try {
    const cli = fileURLToPath(new URL('../bin/goon.mjs', import.meta.url));
    const env = {
      ...process.env,
      HOME: dir,
      USERPROFILE: dir,
      GOON_API_KEY: 'test-key',
      GOON_MODEL: 'unpriced-model',
      GOON_MAX_COST_USD: '1',
    };
    const result = spawnSync(process.execPath, [cli, 'hello'], { cwd: dir, env, encoding: 'utf8' });
    assert.equal(result.status, 2, result.stderr);
    assert.match(result.stderr + result.stdout, /No price configured/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});