/**
 * E2E: the append-only event log, `goon replay`, and session resume across CLI invocations.
 */
import assert from 'node:assert/strict';
import { readdir, readFile } from 'node:fs/promises';
import { join } from 'node:path';
import { test } from 'node:test';
import { BASE_FLAGS, e2eFixture, jsonLines, runGoon, sessionsDir, writeFixture } from './helpers.mjs';

const HELLO_FIXTURE = {
  v: 1,
  turns: [{ match: 'hello', text: ['Hello from stub.'], usage: { inputTokens: 100, outputTokens: 10, totalTokens: 110 } }],
};

test('e2e: --event-log writes redacted envelopes and `goon replay` replays them deterministically', async t => {
  const f = await e2eFixture(t);
  const fixture = await writeFixture(f, 'hello.json', HELLO_FIXTURE);
  const run = await runGoon(t, f, [...BASE_FLAGS, '-p', 'hello', '--stub-fixture', fixture, '--json', '--event-log', '--no-verify']);
  assert.equal(run.code, 0, run.stderr);
  const events = jsonLines(run.stdout);
  const sessionId = events[0].id;

  const logFiles = (await readdir(sessionsDir(f))).filter(name => name.endsWith('.events.jsonl'));
  assert.deepEqual(logFiles, [`${sessionId}.events.jsonl`]);
  const envelopes = jsonLines(await readFile(join(sessionsDir(f), logFiles[0]), 'utf8'));
  assert.equal(envelopes[0].type, 'session');
  assert.equal(envelopes[0].sessionId, sessionId);
  assert.equal(envelopes[0].seq, 1);
  assert.equal(envelopes.at(-1).type, 'done');
  for (let i = 1; i < envelopes.length; i++) {
    assert.equal(envelopes[i].seq, envelopes[i - 1].seq + 1, 'sequence numbers must be gapless');
  }

  // Raw replay: same event stream, twice identically.
  const first = await runGoon(t, f, ['replay', sessionId]);
  assert.equal(first.code, 0, first.stderr);
  const second = await runGoon(t, f, ['replay', sessionId]);
  assert.equal(second.code, 0, second.stderr);
  assert.equal(first.stdout, second.stdout);
  const replayed = jsonLines(first.stdout);
  assert.equal(replayed[0].type, 'session');
  assert.equal(replayed.some(event => event.type === 'user' && event.text === 'hello'), true);
  assert.equal(replayed.at(-1).type, 'done');
  assert.equal(replayed.find(event => event.type === 'done')?.text, 'Hello from stub.');

  // Envelope replay: sequence numbers and session binding survive.
  const envRun = await runGoon(t, f, ['replay', sessionId, '--envelopes']);
  assert.equal(envRun.code, 0, envRun.stderr);
  const envs = jsonLines(envRun.stdout);
  assert.equal(envs[0].sessionId, sessionId);
  assert.equal(envs.at(-1).seq, envelopes.length);
});

test('e2e: --resume continues the same session: id, file artifacts and history carry over', async t => {
  const f = await e2eFixture(t);
  const firstFixture = await writeFixture(f, 'part-one.json', {
    v: 1,
    turns: [
      {
        match: 'part one',
        toolCalls: [{ id: 'call-write', name: 'write_file', input: { path: 'resume-marker.txt', content: 'from part one' } }],
        usage: { inputTokens: 10, outputTokens: 5, totalTokens: 15 },
      },
      { text: ['part one done'], usage: { inputTokens: 8, outputTokens: 3, totalTokens: 11 } },
    ],
  });
  const secondFixture = await writeFixture(f, 'part-two.json', {
    v: 1,
    turns: [
      {
        match: 'part two',
        toolCalls: [{ id: 'call-read', name: 'read_file', input: { path: 'resume-marker.txt' } }],
        usage: { inputTokens: 12, outputTokens: 4, totalTokens: 16 },
      },
      { text: ['part two done'], usage: { inputTokens: 9, outputTokens: 2, totalTokens: 11 } },
    ],
  });

  const first = await runGoon(t, f, [...BASE_FLAGS, '-p', 'part one', '--stub-fixture', firstFixture, '--json', '--yes', '--no-verify']);
  assert.equal(first.code, 0, first.stderr);
  const sessionId = jsonLines(first.stdout)[0].id;
  assert.match(sessionId, /^[0-9a-f-]{36}$/);

  const second = await runGoon(t, f, [
    ...BASE_FLAGS, '--resume', sessionId, '-p', 'part two', '--stub-fixture', secondFixture, '--json', '--yes', '--no-verify',
  ]);
  assert.equal(second.code, 0, second.stderr);
  const secondEvents = jsonLines(second.stdout);
  assert.equal(secondEvents[0].type, 'session');
  assert.equal(secondEvents[0].id, sessionId, 'resumed run must keep the session id');

  // The resumed turn can read the artifact the first run created.
  const read = secondEvents.find(event => event.type === 'tool' && event.name === 'read_file' && event.state === 'done');
  assert.ok(read, 'expected the resumed run to read the file created in run one');
  assert.equal(secondEvents.find(event => event.type === 'done')?.text, 'part two done');

  // Both prompts live in the same persisted session file.
  const session = JSON.parse(await readFile(join(sessionsDir(f), `${sessionId}.json`), 'utf8'));
  const userTexts = session.messages
    .filter(message => message.role === 'user')
    .map(message => (typeof message.content === 'string' ? message.content : ''))
    .join('\n');
  assert.match(userTexts, /part one/);
  assert.match(userTexts, /part two/);

  const list = JSON.parse((await runGoon(t, f, ['sessions', '--json'])).stdout);
  assert.ok(list.some(entry => entry.id === sessionId), 'sessions listing must contain the resumed session');
});
