import assert from 'node:assert/strict';
import { test } from 'node:test';
import { existsSync, mkdirSync, readFileSync, rmSync, statSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { fixture } from './helpers.mjs';
import {
  eventLogPath,
  normalizeEventEnvelope,
  openEventLog,
  readEventLog,
  readLastSeq,
  StreamRedactor,
} from '../dist/eventlog.js';
import { isSensitivePath } from '../dist/redact.js';
import { SessionStore } from '../dist/session.js';

test('eventlog: seq, sidecar and reopen', async t => {
  const f = await fixture(t);
  const root = join(f.root, 'sessions');
  mkdirSync(root, { recursive: true });
  const path = eventLogPath(root, 's1');
  const writer = openEventLog(path, { sessionId: 's1' });
  writer.append({ type: 'session', id: 's1' });
  writer.append({ type: 'todo', todos: [] });
  writer.close();
  const first = readEventLog(path);
  assert.equal(first.truncated, false);
  assert.equal(first.envelopes.length, 2);
  assert.deepEqual(first.envelopes.map(e => e.seq), [1, 2]);
  assert.equal(readLastSeq(path), 2);
  assert.equal(existsSync(eventLogPath(root, 's1') + '.seq'), true);

  const again = openEventLog(path, { sessionId: 's1' });
  again.append({ type: 'todo', todos: [] });
  again.close();
  const second = readEventLog(path);
  assert.deepEqual(second.envelopes.map(e => e.seq), [1, 2, 3]);
});

test('eventlog: env credentials are redacted without explicit secrets', async t => {
  const f = await fixture(t);
  const root = join(f.root, 'sessions');
  mkdirSync(root, { recursive: true });
  const path = join(root, 'envsecret.events.jsonl');
  const secret = 'sk-env-secret-1234567890';
  process.env.GOON_API_KEY = secret;
  try {
    const writer = openEventLog(path, { sessionId: 'envsecret' });
    writer.append({ type: 'text', text: `key ${secret}` });
    writer.append({ type: 'done', text: 'done' });
    writer.close();
  } finally {
    delete process.env.GOON_API_KEY;
  }
  assert.equal(readFileSync(path, 'utf8').includes(secret), false);
});

test('eventlog: reopen repairs a torn tail before appending', async t => {
  const f = await fixture(t);
  const root = join(f.root, 'sessions');
  mkdirSync(root, { recursive: true });
  const path = join(root, 'torn.events.jsonl');
  const writer = openEventLog(path, { sessionId: 'torn' });
  writer.append({ type: 'todo', todos: [] });
  writer.close();
  writeFileSync(path, `${readFileSync(path, 'utf8')}{"v":1,"seq":2,"at":"x","sessionId":"torn","type":"text","event":{"type":"text","text":"half`, 'utf8');

  const reopened = openEventLog(path, { sessionId: 'torn' });
  reopened.append({ type: 'todo', todos: [] });
  reopened.append({ type: 'session', id: 'torn' });
  reopened.close();
  const { envelopes, truncated } = readEventLog(path);
  assert.equal(truncated, false);
  assert.deepEqual(envelopes.map(e => e.seq), [1, 2, 3]);
  assert.deepEqual(envelopes.map(e => e.type), ['todo', 'todo', 'session']);
});

test('eventlog: complete final line without newline gets a separator', async t => {
  const f = await fixture(t);
  const root = join(f.root, 'sessions');
  mkdirSync(root, { recursive: true });
  const path = join(root, 'nonewline.events.jsonl');
  const writer = openEventLog(path, { sessionId: 'nonewline' });
  writer.append({ type: 'todo', todos: [] });
  writer.close();
  writeFileSync(path, readFileSync(path, 'utf8').replace(/\n$/, ''), 'utf8');
  const reopened = openEventLog(path, { sessionId: 'nonewline' });
  reopened.append({ type: 'session', id: 'nonewline' });
  reopened.close();
  const { envelopes, truncated } = readEventLog(path);
  assert.equal(truncated, false);
  assert.deepEqual(envelopes.map(e => e.seq), [1, 2]);
});

test('eventlog: sensitive tool content is dropped but path and counts stay', async t => {
  const f = await fixture(t);
  const root = join(f.root, 'sessions');
  mkdirSync(root, { recursive: true });
  const path = join(root, 'sensitive.events.jsonl');
  const writer = openEventLog(path, { sessionId: 'sensitive' });
  writer.append({ type: 'tool', name: 'read_file', state: 'done', meta: {
    kind: 'read', summary: '.env', outputPreview: 'TOP_SECRET=1', diff: '+PRIVATE_KEY=1', omittedLines: 3, added: 1, removed: 0,
  } });
  writer.append({ type: 'tool', name: 'write_file', state: 'done', meta: {
    kind: 'edit', summary: 'certs/server.pem', diff: '-----BEGIN PRIVATE KEY-----\nsecret\n', added: 2, removed: 0,
  } });
  writer.append({ type: 'tool_stream', name: 'write_file', state: 'delta', preview: '{"path":"certs/server.pem","content":"secret"}' });
  writer.append({ type: 'tool_stream', name: 'bash', state: 'delta', preview: '{"command":"cat .env"}' });
  writer.append({ type: 'tool', name: 'bash', state: 'done', meta: {
    kind: 'bash', summary: 'cat .env', outputPreview: 'SECRET=1',
  } });
  writer.append({ type: 'tool', name: 'read_file', state: 'done', meta: {
    kind: 'read', summary: 'notes.txt', outputPreview: 'note body',
  } });
  writer.close();
  const body = readFileSync(path, 'utf8');
  assert.equal(body.includes('TOP_SECRET'), false);
  assert.equal(body.includes('PRIVATE KEY'), false);
  assert.equal(body.includes('SECRET=1'), false);
  const { envelopes } = readEventLog(path);
  const first = envelopes[0].event;
  assert.equal(first.meta.summary, '.env');
  assert.equal('outputPreview' in first.meta, false);
  assert.equal('diff' in first.meta, false);
  assert.equal('omittedLines' in first.meta, false);
  const safe = envelopes[5].event;
  assert.equal(safe.meta.outputPreview, 'note body');
  assert.equal(envelopes[2].event.preview, '');
  assert.equal(envelopes[3].event.preview, '');
});

test('eventlog: switchSession starts a new file; caller writes its session envelope', async t => {
  const f = await fixture(t);
  const root = join(f.root, 'sessions');
  mkdirSync(root, { recursive: true });
  const first = join(root, 'first.events.jsonl');
  const writer = openEventLog(first, { sessionId: 'first' });
  writer.append({ type: 'session', id: 'first', cwd: f.cwd });
  writer.append({ type: 'user', text: 'old turn' });
  writer.switchSession('second', join(root, 'second.events.jsonl'));
  // This is the sequence the CLI performs after /new, /resume and /delete:
  writer.append({ type: 'session', id: 'second', cwd: f.cwd });
  writer.append({ type: 'user', text: 'hello' });
  writer.append({ type: 'done', text: 'ok', denied: false, sessionId: 'second' });
  writer.close();
  const second = readEventLog(join(root, 'second.events.jsonl'));
  assert.equal(second.truncated, false);
  assert.deepEqual(second.envelopes.map(e => e.type), ['session', 'user', 'done']);
  assert.deepEqual(second.envelopes.map(e => e.sessionId), ['second', 'second', 'second']);
  assert.equal(second.envelopes[0].event.id, 'second');
  assert.equal(second.envelopes[0].event.cwd, f.cwd);
  assert.deepEqual(readEventLog(first).envelopes.map(e => e.type), ['session', 'user']);
});

test('eventlog: sidecar ahead wins, stale sidecar falls back to the tail', async t => {
  const f = await fixture(t);
  const root = join(f.root, 'sessions');
  mkdirSync(root, { recursive: true });
  const path = join(root, 'sidecar.events.jsonl');
  const writer = openEventLog(path, { sessionId: 'sidecar' });
  writer.append({ type: 'todo', todos: [] });
  writer.close();
  writeFileSync(`${path}.seq`, '5\n');
  assert.equal(readLastSeq(path), 5);
  writeFileSync(`${path}.seq`, '0\n');
  assert.equal(readLastSeq(path), 1);
  writeFileSync(`${path}.seq`, 'garbage');
  assert.equal(readLastSeq(path), 1);
});

test('eventlog: torn tail is reported and not parsed', async t => {
  const f = await fixture(t);
  const root = join(f.root, 'sessions');
  mkdirSync(root, { recursive: true });
  const path = eventLogPath(root, 's2');
  const writer = openEventLog(path, { sessionId: 's2' });
  writer.append({ type: 'todo', todos: [] });
  writer.close();
  writeFileSync(path, readFileSync(path, 'utf8') + '{"v":1,"seq":99', 'utf8');
  const { envelopes, truncated } = readEventLog(path);
  assert.equal(truncated, true);
  assert.equal(envelopes.length, 1);
});

test('eventlog: 64 KiB+ and 1 MiB+ lines keep their sequence', async t => {
  const f = await fixture(t);
  const root = join(f.root, 'sessions');
  mkdirSync(root, { recursive: true });
  const path = eventLogPath(root, 's3');
  const writer = openEventLog(path, { sessionId: 's3' });
  writer.append({ type: 'tool', payload: 'x'.repeat(100 * 1024) });
  writer.close();
  rmSync(eventLogPath(root, 's3') + '.seq', { force: true });
  let reopened = openEventLog(path, { sessionId: 's3' });
  reopened.append({ type: 'todo', todos: [] });
  reopened.close();
  assert.equal(readLastSeq(path), 2);

  const path2 = eventLogPath(root, 's4');
  const writer2 = openEventLog(path2, { sessionId: 's4' });
  writer2.append({ type: 'tool', payload: 'y'.repeat(1_100_000) });
  writer2.close();
  rmSync(eventLogPath(root, 's4') + '.seq', { force: true });
  const reopened2 = openEventLog(path2, { sessionId: 's4' });
  reopened2.append({ type: 'todo', todos: [] });
  reopened2.close();
  assert.equal(readLastSeq(path2), 2);
});

test('eventlog: redacts literals split across stream deltas', async t => {
  const f = await fixture(t);
  const root = join(f.root, 'sessions');
  mkdirSync(root, { recursive: true });
  const path = eventLogPath(root, 's5');
  const secret = 'sk-live-abcdef1234567890';
  const writer = openEventLog(path, { sessionId: 's5', secrets: [secret] });
  writer.append({ type: 'text', text: `prefix ${secret.slice(0, 10)}` });
  writer.append({ type: 'text', text: `${secret.slice(10)} suffix` });
  writer.append({ type: 'tool', meta: { apiKey: secret, note: 'Bearer abcdefghijklmnopqrstuvwxyz' } });
  writer.append({ type: 'tool', meta: { json: `{"apiKey":"${secret}"}` } });
  writer.append({ type: 'done', text: 'done' });
  writer.close();
  const body = readFileSync(path, 'utf8');
  assert.equal(body.includes(secret), false);
  assert.equal(body.includes('abcdefghijklmnop'), false);
  const { envelopes } = readEventLog(path);
  const text = envelopes.filter(e => e.type === 'text').map(e => e.event.text).join('');
  assert.equal(text, 'prefix [REDACTED] suffix');
});

test('eventlog: stream redactor keeps a tail', () => {
  const secret = 'sk-live-abcdef1234567890';
  const redactor = new StreamRedactor([secret]);
  const first = redactor.push(`hello ${secret.slice(0, 4)}`);
  const second = redactor.push(`${secret.slice(4)} bye`);
  const tail = redactor.flushAll();
  const combined = first + second + tail;
  assert.equal(combined.includes(secret), false);
  assert.equal(combined.includes('[REDACTED]'), true);
});

test('eventlog: normalizeEventEnvelope rejects unsupported versions', () => {
  assert.throws(() => normalizeEventEnvelope({ v: 2, seq: 1, at: 'x', sessionId: 's', type: 'todo', event: {} }));
});

test('eventlog: deleteSession removes log and sidecar', async t => {
  const f = await fixture(t);
  const root = join(f.root, 'sessions');
  mkdirSync(root, { recursive: true });
  const store = await SessionStore.open(root, { cwd: f.cwd, provider: 'openai-compatible', model: 'm', baseUrl: 'http://127.0.0.1:1/v1' });
  await store.save();
  const id = store.session.id;
  await store.close();
  const path = join(root, `${id}.events.jsonl`);
  const writer = openEventLog(path, { sessionId: id });
  writer.append({ type: 'todo', todos: [] });
  writer.close();
  assert.equal(existsSync(path), true);
  assert.equal(existsSync(path + '.seq'), true);
  await SessionStore.deleteSession(root, id);
  assert.equal(existsSync(join(root, `${id}.json`)), false);
  assert.equal(existsSync(path), false);
  assert.equal(existsSync(path + '.seq'), false);
});

test('eventlog: isSensitivePath flags common secrets', () => {
  assert.equal(isSensitivePath('/tmp/project/.env'), true);
  assert.equal(isSensitivePath('/tmp/project/.env.example'), true);
  assert.equal(isSensitivePath('/tmp/project/server.pem'), true);
  assert.equal(isSensitivePath('/tmp/project/secrets/token.txt'), true);
  assert.equal(isSensitivePath('/tmp/project/src/index.ts'), false);
});

test('eventlog: mode 0600 on POSIX', async t => {
  if (process.platform === 'win32') return t.skip('POSIX mode bits unavailable on Windows');
  const f = await fixture(t);
  const root = join(f.root, 'sessions');
  mkdirSync(root, { recursive: true });
  const path = eventLogPath(root, 'mode');
  const writer = openEventLog(path, { sessionId: 'mode' });
  writer.append({ type: 'todo', todos: [] });
  writer.close();
  assert.equal(statSync(path).mode & 0o777, 0o600);
});
