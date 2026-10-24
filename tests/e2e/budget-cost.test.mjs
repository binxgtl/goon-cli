/**
 * E2E: cost accounting and the hard cost cap.
 *
 * Pricing is injected via GOON_PRICING_JSON ($1000 per 1M tokens both sides). The stub model
 * spends 1234 input + 56 output tokens on a turn that also calls a tool (so the loop continues),
 * which costs $1.29 — above the `--max-cost-usd 0.5` cap. The run must abort with a precise
 * error and exit 1 instead of making the second model call.
 */
import assert from 'node:assert/strict';
import { test } from 'node:test';
import { BASE_FLAGS, e2eFixture, jsonLines, runGoon, writeFixture } from './helpers.mjs';

const PRICING = JSON.stringify({ default: { inputPer1M: 1000, outputPer1M: 1000 } });

test('e2e: cost cap aborts the run with an exact reason and exit 1', async t => {
  const f = await e2eFixture(t);
  const fixture = await writeFixture(f, 'spend.json', {
    v: 1,
    model: 'expensive-model',
    turns: [
      {
        match: 'spend tokens',
        toolCalls: [{ id: 'call-list', name: 'list_files', input: { path: '.' } }],
        text: ['listing before spending'],
        usage: { inputTokens: 1234, outputTokens: 56, totalTokens: 1290 },
      },
      { text: ['this second call must never happen'], usage: { inputTokens: 100, outputTokens: 10, totalTokens: 110 } },
    ],
  });
  const run = await runGoon(t, f, [
    ...BASE_FLAGS, '-p', 'spend tokens', '--stub-fixture', fixture, '--json', '--yes', '--no-verify',
    '--max-cost-usd', '0.5',
  ], { env: { GOON_PRICING_JSON: PRICING } });
  assert.equal(run.code, 1, `stdout=${run.stdout} stderr=${run.stderr}`);
  assert.equal(run.stderr, '');

  const events = jsonLines(run.stdout);
  // Usage is accounted for the one call that did happen, with the exact priced cost.
  const usage = events.find(event => event.type === 'usage');
  assert.deepEqual({ input: usage.input, output: usage.output, total: usage.total }, { input: 1234, output: 56, total: 1290 });
  assert.ok(Math.abs(usage.costUsd - 1.29) < 1e-9, `costUsd was ${usage.costUsd}`);

  // The cap aborts at the top of the next loop iteration, before the second model call.
  const error = events.find(event => event.type === 'error');
  assert.ok(error, 'expected an error event');
  assert.equal(error.message, 'Run cost budget reached ($1.2900 of $0.5000).');
  assert.equal(events.some(event => event.type === 'done'), false);
  assert.equal(events.some(event => String(event.text ?? '').includes('never happen')), false);
});

test('e2e: an unknown model with a cost cap and no pricing fails closed before the run', async t => {
  const f = await e2eFixture(t);
  const fixture = await writeFixture(f, 'plain.json', {
    v: 1,
    turns: [{ match: 'hello', text: ['Hello from stub.'], usage: { inputTokens: 10, outputTokens: 5, totalTokens: 15 } }],
  });
  const run = await runGoon(t, f, [
    ...BASE_FLAGS, '-p', 'hello', '--stub-fixture', fixture, '--json', '--no-verify',
    '--max-cost-usd', '1',
  ], { env: { GOON_PRICING_JSON: JSON.stringify({ 'other-model': { inputPer1M: 1, outputPer1M: 1 } }) } });
  assert.equal(run.code, 2, `stdout=${run.stdout} stderr=${run.stderr}`);
  const events = jsonLines(run.stdout);
  assert.equal(events.length, 1);
  // Fail-closed happens before the run starts: a bare error object, no events, no session.
  // Stub mode runs as model "stub-model" when no --model is configured.
  assert.match(events[0].error, /No price configured for model "stub-model"/);
  assert.match(events[0].error, /other-model/);
});
