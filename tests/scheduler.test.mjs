import test from 'node:test';
import assert from 'node:assert/strict';
import { setTimeout } from 'node:timers/promises';
import { join } from 'node:path';
import { z } from 'zod';
import { Harness } from '../dist/harness.js';
import { SessionStore } from '../dist/session.js';
import { ToolRegistry } from '../dist/tools.js';
import { fixture, provider, sse } from './helpers.mjs';

test('bounded parallel reads drain before mutation and results keep model order', async t => {
  const f = await fixture(t);
  const calls = ['slow_read', 'fast_read', 'third_read', 'mutate'].map((name, i) => ({ id: `call${i}`, name, input: {} }));
  const p = await provider(t, (body, response, n) => {
    if (n === 1) return sse(response, { calls });
    assert.deepEqual(body.messages.filter(m => m.role === 'tool').map(m => m.tool_call_id), ['call0', 'call1', 'call2', 'call3']);
    sse(response, { text: 'done' });
  });
  const config = { cwd: f.cwd, provider: 'openai-compatible', model: 'test-model', baseUrl: p.url, apiKey: 'fake', contextWindow: 128000, maxOutputTokens: 8192, timeoutMs: 3000, runTokenBudget: 100000 };
  const store = await SessionStore.open(join(f.root, 'sessions'), config);
  t.after(() => store.close());
  const registry = new ToolRegistry(f.cwd, async () => true);
  let active = 0, maximum = 0;
  const order = [];
  for (const [name, delay] of [['slow_read', 40], ['fast_read', 5], ['third_read', 5]]) {
    registry.register(name, { description: 'test read', schema: z.object({}), readOnly: true, execute: async () => {
      active++; maximum = Math.max(maximum, active); order.push(`start:${name}`);
      await setTimeout(delay); active--; order.push(`done:${name}`); return name;
    } });
  }
  registry.register('mutate', { description: 'test mutation', schema: z.object({}), readOnly: false, execute: async () => {
    assert.equal(active, 0); order.push('mutate'); return 'mutated';
  } });
  await new Harness(config, store, registry).run('test', new AbortController().signal);
  assert.equal(maximum, 2);
  assert(order.indexOf('mutate') > order.indexOf('done:third_read'));
});

test('cancelled tools are drained and every unstarted call gets a terminal result', async t => {
  const f = await fixture(t);
  const p = await provider(t, (_, response) => sse(response, { calls: [
    { id: 'slow', name: 'slow', input: {} }, { id: 'later', name: 'mutate', input: {} },
  ] }));
  const config = { cwd: f.cwd, provider: 'openai-compatible', model: 'test-model', baseUrl: p.url, apiKey: 'fake', contextWindow: 128000, maxOutputTokens: 8192, timeoutMs: 3000, runTokenBudget: 100000 };
  const store = await SessionStore.open(join(f.root, 'sessions'), config);
  t.after(() => store.close());
  const controller = new AbortController();
  let drained = false, mutated = false;
  const registry = new ToolRegistry(f.cwd, async () => true);
  registry.register('slow', { description: 'cancel fixture', schema: z.object({}), readOnly: true, execute: async (_, signal) => {
    controller.abort();
    try { await setTimeout(50, undefined, { signal }); } finally { drained = true; }
    return 'never';
  } });
  registry.register('mutate', { description: 'must not run', schema: z.object({}), readOnly: false, execute: async () => { mutated = true; return 'bad'; } });
  await assert.rejects(new Harness(config, store, registry).run('test', controller.signal));
  assert(drained); assert(!mutated);
  const results = store.session.messages.filter(m => m.role === 'tool').flatMap(m => m.content);
  assert.deepEqual(results.map(r => r.toolCallId), ['slow', 'later']);
  assert(results.every(r => r.output.type === 'error-text'));
});
