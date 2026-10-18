import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile, writeFile } from 'node:fs/promises';
import { join } from 'node:path';
import { SessionStore, repairInterrupted } from '../dist/session.js';
import { fixture } from './helpers.mjs';

test('session lock prevents two owners and resume retains message history', async t => {
  const f = await fixture(t), root = join(f.root, 'sessions');
  const input = { cwd: f.cwd, provider: 'test', model: 'model', baseUrl: 'http://localhost/v1' };
  const store = await SessionStore.open(root, input);
  await assert.rejects(SessionStore.open(root, input, store.session.id), /already open/);
  store.session.messages.push({ role: 'user', content: 'remember' });
  await store.save();
  await store.close();
  const resumed = await SessionStore.open(root, input, 'latest');
  assert.equal(resumed.session.messages[0].content, 'remember');
  assert.equal(resumed.session.id, store.session.id);
  await resumed.close();
});

test('crash repair preserves completed results and adds unknown outcomes once', () => {
  const messages = [{ role: 'assistant', content: [
    { type: 'tool-call', toolCallId: 'one', toolName: 'write_file', input: {} },
    { type: 'tool-call', toolCallId: 'two', toolName: 'bash', input: {} },
  ] }, { role: 'tool', content: [{ type: 'tool-result', toolCallId: 'one', toolName: 'write_file', output: { type: 'text', value: 'saved' } }] }];
  assert.equal(repairInterrupted(messages), true);
  assert.equal(messages.at(-1).content[0].toolCallId, 'two');
  assert.equal(messages.at(-1).content[0].output.type, 'error-text');
  assert.equal(repairInterrupted(messages), false);
});

test('resume rejects another workspace and releases the acquired lock', async t => {
  const f = await fixture(t), root = join(f.root, 'sessions');
  const input = { cwd: f.cwd, provider: 'test', model: 'model', baseUrl: 'http://localhost/v1' };
  const store = await SessionStore.open(root, input);
  const id = store.session.id;
  await store.close();
  await assert.rejects(SessionStore.open(root, { ...input, cwd: f.root }, id), /different workspace/);
  const resumed = await SessionStore.open(root, input, id);
  await resumed.close();
});

test('dead process lock can be recovered and archives do not duplicate session listings', async t => {
  const f = await fixture(t), root = join(f.root, 'sessions');
  const input = { cwd: f.cwd, provider: 'test', model: 'model', baseUrl: 'http://localhost/v1' };
  const store = await SessionStore.open(root, input), id = store.session.id;
  await store.close();
  await writeFile(join(root, `${id}.lock`), '2147483647');
  await writeFile(join(root, `${id}.compact-1.json`), await readFile(join(root, `${id}.json`)));
  const resumed = await SessionStore.open(root, input, id);
  assert.equal((await SessionStore.list(root)).length, 1);
  await resumed.close();
});

test('SessionStore supports short 8-char ID prefix resume, excludeId on latest, and forkToNewFile', async t => {
  const f = await fixture(t), root = join(f.root, 'sessions');
  const input = { cwd: f.cwd, provider: 'test', model: 'model', baseUrl: 'http://localhost/v1' };
  const s1 = await SessionStore.open(root, input);
  s1.session.messages.push({ role: 'user', content: 'First task in session 1' });
  await s1.save();
  const forkedId = await s1.forkToNewFile('Forked branch');
  await s1.close();

  // Resume by 8-char prefix
  const byPrefix = await SessionStore.open(root, input, forkedId.slice(0, 8));
  assert.equal(byPrefix.session.id, forkedId);
  assert.equal(byPrefix.session.title, 'Forked branch');
  assert.equal(byPrefix.session.messages.length, 1);

  // Resume latest excluding currently active session
  const latestOther = await SessionStore.open(root, input, 'latest', forkedId);
  assert.equal(latestOther.session.id, s1.session.id);
  await byPrefix.close();
  await latestOther.close();
});

