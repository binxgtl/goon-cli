/**
 * E2E: secret hygiene through real CLI runs.
 *
 * 1. Provider credentials never reach child processes: the bash tool's environment has
 *    GOON_API_KEY stripped, so `printenv` cannot see it even though the CLI itself can.
 * 2. A credential literal that flows through model output/file content is redacted before it is
 *    logged — in the event log file AND in the `--json` stdout event stream.
 */
import assert from 'node:assert/strict';
import { readdir, readFile } from 'node:fs/promises';
import { join } from 'node:path';
import { test } from 'node:test';
import { BASE_FLAGS, e2eFixture, jsonLines, runGoon, sessionsDir, writeFixture } from './helpers.mjs';

const SECRET = 'bare-secret-literal-42';

test('e2e: GOON_API_KEY is stripped from bash child env — printenv cannot exfiltrate it', async t => {
  const f = await e2eFixture(t);
  const fixture = await writeFixture(f, 'printenv.json', {
    v: 1,
    turns: [
      {
        match: 'print the key',
        toolCalls: [{ id: 'call-env', name: 'bash', input: { command: 'printenv GOON_API_KEY' } }],
        usage: { inputTokens: 10, outputTokens: 5, totalTokens: 15 },
      },
      { text: ['checked'], usage: { inputTokens: 8, outputTokens: 3, totalTokens: 11 } },
    ],
  });
  const run = await runGoon(t, f, [...BASE_FLAGS, '-p', 'print the key', '--stub-fixture', fixture, '--json', '--yes', '--no-verify'], {
    env: { GOON_API_KEY: SECRET },
  });
  assert.equal(run.code, 0, run.stderr);

  const events = jsonLines(run.stdout);
  const bash = events.filter(event => event.type === 'tool' && event.name === 'bash');
  assert.equal(bash.length, 2);
  assert.equal(bash[1].state, 'done');
  // printenv exits 1 with no output when the variable is unset: exactly one line, no secret.
  // (If the key leaked, printenv would print it and outputPreview would carry the literal.)
  assert.deepEqual(bash[1].meta.outputPreview, ['Exit code: 1']);
  assert.equal(run.stdout.includes(SECRET), false, 'the credential literal must not reach stdout');
  assert.equal(run.stderr.includes(SECRET), false);
});

test('e2e: a credential literal in file content is [REDACTED] in the event log and --json stdout', async t => {
  const f = await e2eFixture(t);
  const fixture = await writeFixture(f, 'save-note.json', {
    v: 1,
    turns: [
      {
        match: 'save the note',
        toolCalls: [{
          id: 'call-write',
          name: 'write_file',
          input: { path: 'notes.txt', content: `the credential is ${SECRET} keep it safe` },
        }],
        usage: { inputTokens: 10, outputTokens: 5, totalTokens: 15 },
      },
      { text: ['Saved your note.'], usage: { inputTokens: 8, outputTokens: 3, totalTokens: 11 } },
    ],
  });
  const run = await runGoon(t, f, [
    ...BASE_FLAGS, '-p', 'save the note', '--stub-fixture', fixture, '--json', '--yes', '--no-verify', '--event-log',
  ], { env: { GOON_API_KEY: SECRET } });
  assert.equal(run.code, 0, run.stderr);

  // The JSON stdout stream carries the write diff; the literal must be scrubbed there.
  assert.equal(run.stdout.includes(SECRET), false, 'event log stdout stream leaked the credential');
  assert.equal(run.stdout.includes('[REDACTED]'), true, 'expected redaction markers on stdout');
  const events = jsonLines(run.stdout);
  const diff = events.find(event => event.type === 'tool' && event.name === 'write_file')?.meta.diff;
  assert.match(diff, /the credential is \[REDACTED\] keep it safe/);

  // The on-disk event log is redacted with the same rules.
  const logName = (await readdir(sessionsDir(f))).find(name => name.endsWith('.events.jsonl'));
  assert.ok(logName, 'event log was not created');
  const log = await readFile(join(sessionsDir(f), logName), 'utf8');
  assert.equal(log.includes(SECRET), false, 'event log file leaked the credential');
  assert.equal(log.includes('[REDACTED]'), true);
});
