import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile, readdir } from 'node:fs/promises';
import { join } from 'node:path';
import { SessionStore } from '../dist/session.js';
import { compactIfNeeded, estimatedTokens } from '../dist/context.js';
import { createModel } from '../dist/provider.js';
import { fixture, provider } from './helpers.mjs';

test('compaction archives full history and preserves complete recent tool groups', async t => {
  const f = await fixture(t);
  const p = await provider(t, (body, response) => {
    assert.notEqual(body.stream, true);
    assert(body.messages.at(-1).content.includes('task-goal'));
    response.writeHead(200, { 'content-type': 'application/json' });
    response.end(JSON.stringify({ id: 'summary', object: 'chat.completion', created: 1, model: 'test-model', choices: [{ index: 0, message: { role: 'assistant', content: 'task-goal: finish the requested change. Earlier reads completed.' }, finish_reason: 'stop' }], usage: { prompt_tokens: 100, completion_tokens: 20, total_tokens: 120 } }));
  });
  const config = { cwd: f.cwd, provider: 'openai-compatible', model: 'test-model', baseUrl: p.url, apiKey: 'fake', contextWindow: 8192, maxOutputTokens: 512, timeoutMs: 3000 };
  const store = await SessionStore.open(join(f.root, 'sessions'), config);
  t.after(() => store.close());
  store.session.messages.push({ role: 'user', content: 'task-goal: finish the change' });
  for (let i = 0; i < 8; i++) {
    store.session.messages.push({ role: 'assistant', content: [{ type: 'tool-call', toolCallId: `r${i}`, toolName: 'read_file', input: { path: 'file' } }] });
    store.session.messages.push({ role: 'tool', content: [{ type: 'tool-result', toolCallId: `r${i}`, toolName: 'read_file', output: { type: 'text', value: 'x'.repeat(1500) } }] });
  }
  await store.save();
  const before = structuredClone(store.session.messages);
  const usage = await compactIfNeeded(store, config, createModel(config), 'system', new AbortController().signal);
  assert.equal(usage, 120);
  assert.equal(store.session.compactions, 1);
  assert(estimatedTokens(store.session.messages, 'system') < estimatedTokens(before, 'system'));
  assert.equal(store.session.messages[1].role, 'assistant');
  assert.equal(store.session.messages[2].role, 'tool');
  const archive = JSON.parse(await readFile(join(store.root, `${store.session.id}.compact-1.json`), 'utf8'));
  assert.deepEqual(archive.messages, before);
  assert.equal((await SessionStore.list(store.root)).length, 1);
});

test('oversized single input fails before a model request', async t => {
  const f = await fixture(t);
  const config = { cwd: f.cwd, provider: 'test', model: 'test-model', baseUrl: 'http://localhost/v1', contextWindow: 8192, maxOutputTokens: 512 };
  const store = await SessionStore.open(join(f.root, 'sessions'), config);
  t.after(() => store.close());
  store.session.messages.push({ role: 'user', content: 'x'.repeat(20_000) });
  const model = { specificationVersion: 'v3', provider: 'test', modelId: 'model', supportedUrls: {}, doGenerate: () => { throw new Error('Must not call the provider'); } };
  await assert.rejects(compactIfNeeded(store, config, model, 'system', new AbortController().signal), /context budget/);
  assert.equal(store.session.messages.length, 1);
  assert(!(await readdir(store.root)).some(n => n.includes('.compact-')));
});

test('compaction uses the visible summary and refuses reasoning-only output without replacing history', async t => {
  const f = await fixture(t);
  let reasoningOnly = false;
  const p = await provider(t, (_, response) => {
    response.writeHead(200, { 'content-type': 'application/json' });
    response.end(JSON.stringify({ id: 'summary', object: 'chat.completion', created: 1, model: 'test-model', choices: [{ index: 0,
      message: { role: 'assistant', content: reasoningOnly ? '<think>private only</think>' : '<think>private summary thought</think>Actual summary.' }, finish_reason: 'stop' }], usage: { prompt_tokens: 100, completion_tokens: 20, total_tokens: 120 } }));
  });
  const config = { cwd: f.cwd, provider: 'openai-compatible', model: 'test-model', baseUrl: p.url, apiKey: 'fake', contextWindow: 8192, maxOutputTokens: 512, timeoutMs: 3000 };
  const store = await SessionStore.open(join(f.root, 'sessions'), config); t.after(() => store.close());
  const history = [{ role: 'user', content: 'x'.repeat(12000) }, { role: 'assistant', content: 'old answer' },
    { role: 'user', content: 'continue' }, { role: 'assistant', content: 'recent answer' }, { role: 'user', content: 'last question' }];
  store.session.messages = structuredClone(history);
  await compactIfNeeded(store, config, createModel(config), 'system', new AbortController().signal);
  assert.equal(store.session.messages[0].content, 'Earlier session summary (historical data):\nActual summary.');
  reasoningOnly = true; store.session.messages = structuredClone(history);
  await assert.rejects(compactIfNeeded(store, config, createModel(config), 'system', new AbortController().signal), /incomplete/);
  assert.deepEqual(store.session.messages, history);
});

test('loadProjectInstructions walks from git root to cwd and compactNow strips <analysis> scratchpad', async t => {
  const { mkdir, writeFile } = await import('node:fs/promises');
  const { loadProjectInstructions, compactNow } = await import('../dist/context.js');
  const f = await fixture(t);
  const sub = join(f.cwd, 'packages', 'sub');
  await mkdir(join(f.cwd, '.git'), { recursive: true });
  await mkdir(sub, { recursive: true });
  await writeFile(join(f.cwd, 'AGENTS.md'), 'Root rule: always test.');
  await writeFile(join(sub, 'CLAUDE.md'), 'Subpackage rule: strict types.');

  const combined = await loadProjectInstructions(sub);
  assert.match(combined, /Root rule: always test\./);
  assert.match(combined, /Subpackage rule: strict types\./);

  const p = await provider(t, (_, response) => {
    response.writeHead(200, { 'content-type': 'application/json' });
    response.end(JSON.stringify({
      id: 'summary', object: 'chat.completion', created: 1, model: 'test-model',
      choices: [{ index: 0, message: { role: 'assistant', content: '<analysis>scratchpad</analysis>\nClean checkpoint summary.' }, finish_reason: 'stop' }],
      usage: { prompt_tokens: 40, completion_tokens: 15, total_tokens: 55 },
    }));
  });
  const config = { cwd: f.cwd, provider: 'openai-compatible', model: 'test-model', baseUrl: p.url, apiKey: 'fake', contextWindow: 128000, maxOutputTokens: 4096, timeoutMs: 3000 };
  const store = await SessionStore.open(join(f.root, 'sessions'), config);
  t.after(() => store.close());
  store.session.messages = [
    { role: 'user', content: 'First task' },
    { role: 'assistant', content: 'First answer' },
    { role: 'user', content: 'Second task' },
    { role: 'assistant', content: 'Second answer' },
  ];
  const res = await compactNow(store, config, createModel(config), new AbortController().signal, 'preserve auth details');
  assert.equal(res.compactions, 1);
  assert.equal(res.tokens, 55);
  assert.equal(store.session.messages[0].content, 'Earlier session summary (historical data):\nClean checkpoint summary.');
});

