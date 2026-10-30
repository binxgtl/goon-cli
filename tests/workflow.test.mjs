import test from 'node:test';
import assert from 'node:assert/strict';

import { fixture, provider, sse, cli } from './helpers.mjs';

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
