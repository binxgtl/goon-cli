import test from 'node:test';
import assert from 'node:assert/strict';
import { join } from 'node:path';
import { SessionStore } from '../dist/session.js';
import { Subagents } from '../dist/subagents.js';
import { ToolRegistry } from '../dist/tools.js';
import { fixture, provider, sse, cli } from './helpers.mjs';

const configFor = (f, p, extra = {}) => ({
  cwd: f.cwd, dataDir: f.data, provider: 'openai-compatible', model: 'test-model', baseUrl: p.url,
  apiKey: 'fake', contextWindow: 128000, maxOutputTokens: 8192, timeoutMs: 5000,
  runTokenBudget: 100000, ...extra,
});

test('workflow prompt steps share the parent run budget', async t => {
  const f = await fixture(t);
  // Request 1 is the parent's run_workflow call. Requests 2+ are the tdd-cycle prompt steps, each
  // running in its own child session. A fresh budget per step would allow all three steps and the
  // parent continuation; a shared budget stops before the third step (110 tokens per request).
  const p = await provider(t, (_body, response, requestNumber) => {
    if (requestNumber === 1) {
      return sse(response, { calls: [{ id: 'wf-1', name: 'run_workflow', input: { name: 'tdd-cycle', input: 'shared budget' } }] });
    }
    return sse(response, { text: `Workflow step ${requestNumber - 1} done.` });
  });
  const result = await cli(f, p.url, ['--yes', '--json', '--run-token-budget', '250', '-p', 'run the workflow']).done;

  assert.equal(p.requests.length, 3, 'the shared budget stopped the workflow before its third step');
  assert.notEqual(result.code, 0);
  assert.match(`${result.stdout}\n${result.stderr}`, /token budget reached/i);
});

test('workflow subagent steps stay pinned to the shared budget between harness runs', async t => {
  const f = await fixture(t);
  let childRequests = 0;
  const p = await provider(t, (body, response) => {
    if (body.messages.some(m => typeof m.content === 'string' && m.content.startsWith('You are a delegated agent.'))) {
      childRequests++;
      return sse(response, { text: `Child ${childRequests} report` });
    }
    return sse(response, { text: 'unused' });
  });
  const config = configFor(f, p);
  const store = await SessionStore.open(join(f.data, 'sessions'), config);
  t.after(() => store.close());
  const tools = new ToolRegistry(f.cwd, async () => true);
  new Subagents(config, store, tools).register();

  // The /workflow slash path runs between harness turns, and Harness.run clears tools.budget on
  // exit; pin the shared budget so delegate children cannot start a fresh one per step.
  tools.budget = undefined;
  const budget = { spent: 0, limit: 110, costSpent: 0, costLimit: 0, startedAt: Date.now(), timeLimitMs: 600_000 };
  const delegate = tasks => tools.withBudget(budget, () => tools.execute(
    { toolCallId: `wf-delegate-${tasks[0]}`, toolName: 'delegate_task', input: { tasks } },
    new AbortController().signal,
  ));

  // sse reports 110 tokens per request, so the first step reaches the shared cap exactly.
  const first = await delegate(['alpha']);
  assert.equal(childRequests, 1);
  assert.match(first.text, /Child 1 report/);

  // A fresh budget per step would reset `spent` and run this child too; a pinned budget must not.
  const second = await delegate(['beta']);
  assert.equal(childRequests, 1, 'the second step must not start a fresh token budget');
  assert.match(second.text, /token budget reached/i);
  assert.equal(tools.budget, undefined, 'the previous registry budget is restored');
  assert.equal(budget.limit, 110);
});
