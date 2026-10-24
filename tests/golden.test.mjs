import assert from 'node:assert/strict';
import { test } from 'node:test';
import { existsSync, readFileSync, readdirSync, rmSync, writeFileSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { fixture, normalizeStream, stubCli } from './helpers.mjs';

const GOLDEN_DIR = resolve('tests/golden');
const FIXTURE_DIR = resolve('tests/fixtures');

const SCENARIOS = [
  { name: 'stub-text', fixture: 'stub-text.json', prompt: 'hello', exitCode: 0, done: 'Hello from stub.' },
  { name: 'stub-tool-call', fixture: 'stub-tool-call.json', prompt: 'read the file', exitCode: 0, done: 'Read complete.', notes: true },
  { name: 'stub-error', fixture: 'stub-error.json', prompt: 'boom', exitCode: 1, done: null },
  { name: 'stub-usage', fixture: 'stub-usage.json', prompt: 'usage please', exitCode: 0, done: 'usage check' },
];

async function runWithLog(t, fixtureName, prompt, { notes = false } = {}) {
  const f = await fixture(t);
  if (notes) writeFileSync(join(f.cwd, 'notes.txt'), 'note body\n');
  const run = await stubCli(f, [
    '--event-log', '--no-mcp', '--no-subagents', '-p', prompt,
    '--stub-fixture', join(FIXTURE_DIR, fixtureName),
  ]).done;
  const logName = readdirSync(join(f.data, 'goon', 'sessions')).find(name => name.endsWith('.events.jsonl'));
  assert.ok(logName, 'event log was not created');
  const path = join(f.data, 'goon', 'sessions', logName);
  const envelopes = readFileSync(path, 'utf8').split('\n').filter(Boolean).map(line => JSON.parse(line));
  return { f, run, path, envelopes, sessionId: envelopes[0].sessionId };
}

for (const scenario of SCENARIOS) {
  test(`golden: ${scenario.name}`, async t => {
    const { f, run, envelopes, sessionId } = await runWithLog(t, scenario.fixture, scenario.prompt, scenario);
    assert.equal(run.code, scenario.exitCode, run.stderr);
    const seqCounter = { value: 0 };
    const normalized = normalizeStream(envelopes, {
      cwd: f.cwd, root: f.root, data: f.data, config: f.config, sessionId, seqCounter,
    });
    const actual = `${normalized.map(event => JSON.stringify(event)).join('\n')}\n`;
    const expectedPath = join(GOLDEN_DIR, `${scenario.name}.jsonl`);
    if (process.env.GOON_UPDATE_GOLDEN === '1') {
      writeFileSync(expectedPath, actual, 'utf8');
    } else {
      assert.equal(actual, readFileSync(expectedPath, 'utf8'));
    }
    const types = normalized.map(event => event.type);
    assert.equal(types[0], 'session');
    assert.equal(types[1], 'user');
    const done = normalized.filter(event => event.type === 'done').at(-1);
    if (scenario.done !== null) {
      assert.equal(done.event.text, scenario.done);
    } else {
      assert.equal(done, undefined);
      assert.equal(types.includes('error'), true);
    }
    if (scenario.name === 'stub-usage') {
      assert.deepEqual(normalized.find(event => event.type === 'usage').event, { type: 'usage', input: 1234, output: 56, total: 1290 });
    }
    if (scenario.name === 'stub-tool-call') {
      assert.equal(types.includes('tool'), true);
      assert.equal(types.includes('tool_stream'), true);
    }
  });
}

test('golden: replay raw and envelopes are idempotent, gaps only warn', async t => {
  const { f, path, sessionId } = await runWithLog(t, 'stub-text.json', 'hello');
  const first = await stubCli(f, ['replay', sessionId]).done;
  const second = await stubCli(f, ['replay', sessionId]).done;
  assert.equal(first.code, 0, first.stderr);
  assert.equal(first.stdout, second.stdout);
  const raw = first.stdout.split('\n').filter(Boolean).map(line => JSON.parse(line));
  assert.deepEqual(raw.map(event => event.type), ['session', 'user', 'context_budget', 'text', 'usage', 'done']);

  const envelopes = await stubCli(f, ['replay', sessionId, '--envelopes']).done;
  assert.equal(envelopes.code, 0, envelopes.stderr);
  const lines = envelopes.stdout.split('\n').filter(Boolean).map(line => JSON.parse(line));
  assert.deepEqual(lines.map(envelope => envelope.seq), [1, 2, 3, 4, 5, 6]);
  assert.equal(lines[0].sessionId, sessionId);

  const all = readFileSync(path, 'utf8').split('\n').filter(Boolean);
  writeFileSync(path, `${[all[0], ...all.slice(2)].join('\n')}\n`);
  const gapped = await stubCli(f, ['replay', sessionId]).done;
  assert.equal(gapped.code, 0, gapped.stderr);
  assert.match(gapped.stderr, /sequence gap/);
});

test('golden: resuming a session without a log writes a session envelope', async t => {
  const f = await fixture(t);
  const fixtureFile = join(FIXTURE_DIR, 'stub-text.json');
  const first = await stubCli(f, ['--json', '--no-mcp', '--no-subagents', '-p', 'hello', '--stub-fixture', fixtureFile]).done;
  assert.equal(first.code, 0, first.stderr);
  const id = JSON.parse(first.stdout.split('\n').filter(Boolean)[0]).id;
  const resumed = await stubCli(f, ['--event-log', '--no-mcp', '--no-subagents', '--resume', id, '-p', 'hello', '--stub-fixture', fixtureFile]).done;
  assert.equal(resumed.code, 0, resumed.stderr);
  const log = readFileSync(join(f.data, 'goon', 'sessions', `${id}.events.jsonl`), 'utf8')
    .split('\n').filter(Boolean).map(line => JSON.parse(line));
  assert.equal(log[0].type, 'session');
  assert.equal(log[0].sessionId, id);
  assert.equal(log[0].event.cwd, f.cwd);
  assert.equal(log[1].type, 'user');
  assert.equal(log[log.length - 1].type, 'done');
});

test('golden: replay rebuild writes a partial-safe, idempotent session', async t => {
  const { f, path, sessionId } = await runWithLog(t, 'stub-text.json', 'hello');
  const sessionPath = join(f.data, 'goon', 'sessions', `${sessionId}.json`);
  const original = readFileSync(sessionPath, 'utf8');
  const rebuiltPath = join(f.data, 'goon', 'sessions', `${sessionId}.replayed.json`);

  const first = await stubCli(f, ['replay', sessionId, '--rebuild-session']).done;
  assert.equal(first.code, 0, first.stderr);
  const firstContent = readFileSync(rebuiltPath, 'utf8');
  const rebuilt = JSON.parse(firstContent);
  assert.equal(rebuilt.id, sessionId);
  assert.equal(rebuilt.cwd, f.cwd);
  assert.deepEqual(rebuilt.user, ['hello']);
  assert.equal(rebuilt.answer, 'Hello from stub.');
  assert.equal(rebuilt.denied, false);
  assert.equal(readFileSync(sessionPath, 'utf8'), original);

  const second = await stubCli(f, ['replay', sessionId, '--rebuild-session']).done;
  assert.equal(second.code, 0, second.stderr);
  assert.equal(readFileSync(rebuiltPath, 'utf8'), firstContent);

  // Rebuild must work after the session JSON is gone: the log is resolved by id.
  rmSync(sessionPath);
  const third = await stubCli(f, ['replay', sessionId, '--rebuild-session']).done;
  assert.equal(third.code, 0, third.stderr);
  assert.equal(existsSync(rebuiltPath), true);
  assert.equal(JSON.parse(readFileSync(rebuiltPath, 'utf8')).answer, 'Hello from stub.');

  // A crashed turn (no `done`) still rebuilds the user turns.
  const partialPath = join(f.root, 'partial.events.jsonl');
  const partial = readFileSync(path, 'utf8').split('\n').filter(Boolean)
    .filter(line => JSON.parse(line).type !== 'done').join('\n');
  writeFileSync(partialPath, `${partial}\n`);
  const partialRun = await stubCli(f, ['replay', partialPath, '--rebuild-session']).done;
  assert.equal(partialRun.code, 0, partialRun.stderr);
  const partialRebuilt = JSON.parse(readFileSync(join(f.root, `${sessionId}.replayed.json`), 'utf8'));
  assert.deepEqual(partialRebuilt.user, ['hello']);
  assert.equal('answer' in partialRebuilt, false);
});
