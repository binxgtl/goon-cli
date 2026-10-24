/**
 * E2E: the verification gate over a real CLI run.
 *
 * `--verify-cmd "node check.js"` replaces the whole command matrix with one deterministic check:
 * `check.js` exits 1 (with a `path:line: message` line) while `app.js` is broken and exits 0 once
 * it is fixed. The stub provider scripts the model: write a broken file, react to the pushed
 * verification failure, and finally give up (to exercise rollback).
 */
import assert from 'node:assert/strict';
import { access, readFile, writeFile } from 'node:fs/promises';
import { test } from 'node:test';
import { BASE_FLAGS, e2eFixture, jsonLines, runGoon, writeFixture } from './helpers.mjs';

const CHECK_SCRIPT = [
  "const fs = require('node:fs');",
  "const content = fs.readFileSync('app.js', 'utf8');",
  "if (content.includes('broken')) { console.error('app.js:1: app is still broken'); process.exit(1); }",
  'process.exit(0);',
  '',
].join('\n');

async function setupWorkspace(t) {
  const f = await e2eFixture(t);
  await writeFile(`${f.workspace}/check.js`, CHECK_SCRIPT, 'utf8');
  return f;
}

test('e2e: verify fails, the model repairs, verify passes (exit 0)', async t => {
  const f = await setupWorkspace(t);
  const fixture = await writeFixture(f, 'repair.json', {
    v: 1,
    turns: [
      {
        match: 'make the app',
        toolCalls: [{ id: 'call-broken', name: 'write_file', input: { path: 'app.js', content: 'broken' } }],
        usage: { inputTokens: 10, outputTokens: 5, totalTokens: 15 },
      },
      {
        match: 'Verification failed',
        toolCalls: [{ id: 'call-fixed', name: 'write_file', input: { path: 'app.js', content: 'fixed' } }],
        usage: { inputTokens: 20, outputTokens: 5, totalTokens: 25 },
      },
      { text: ['Repaired and verified.'], usage: { inputTokens: 8, outputTokens: 3, totalTokens: 11 } },
    ],
  });
  const run = await runGoon(t, f, [
    ...BASE_FLAGS, '-p', 'make the app', '--stub-fixture', fixture, '--json', '--yes',
    '--verify-cmd', 'node check.js',
  ]);
  assert.equal(run.code, 0, `stdout=${run.stdout} stderr=${run.stderr}`);

  const events = jsonLines(run.stdout);
  const verifyStarts = events.filter(event => event.type === 'verify_start');
  assert.equal(verifyStarts.length, 2);
  assert.deepEqual(verifyStarts[0], { type: 'verify_start', files: ['app.js'], commands: ['node check.js'] });
  assert.deepEqual(verifyStarts[1], { type: 'verify_start', files: ['app.js'], commands: ['node check.js'] });

  const verifyResults = events.filter(event => event.type === 'verify_result');
  assert.equal(verifyResults.length, 2);
  assert.equal(verifyResults[0].status, 'fail');
  assert.equal(verifyResults[0].stage, 'test');
  assert.equal(verifyResults[0].reason, 'test failed (exit code 1)');
  assert.equal(verifyResults[0].errors.length, 1);
  assert.equal(verifyResults[0].errors[0].path, 'app.js');
  assert.equal(verifyResults[0].errors[0].line, 1);
  assert.match(verifyResults[0].errors[0].message, /app is still broken/);
  assert.equal(verifyResults[1].status, 'pass');
  assert.deepEqual(verifyResults[1].errors, []);

  // Both writes ran, in order: broken first, repair second.
  const writes = events.filter(event => event.type === 'tool' && event.name === 'write_file' && event.state === 'done');
  assert.equal(writes.length, 2);
  assert.match(writes[0].meta.diff, /\+broken/);
  assert.match(writes[1].meta.diff, /\+fixed/);

  assert.equal(events.find(event => event.type === 'done')?.text, 'Repaired and verified.');
  assert.equal(await readFile(`${f.workspace}/app.js`, 'utf8'), 'fixed');
});

test('e2e: model gives up after verify failure -> rollback deletes the edit, exit 1', async t => {
  const f = await setupWorkspace(t);
  const fixture = await writeFixture(f, 'giveup.json', {
    v: 1,
    turns: [
      {
        match: 'make the app',
        toolCalls: [{ id: 'call-broken', name: 'write_file', input: { path: 'app.js', content: 'broken' } }],
        usage: { inputTokens: 10, outputTokens: 5, totalTokens: 15 },
      },
      { match: 'Verification failed', text: ['I cannot fix this, giving up.'], usage: { inputTokens: 20, outputTokens: 5, totalTokens: 25 } },
    ],
  });
  const run = await runGoon(t, f, [
    ...BASE_FLAGS, '-p', 'make the app', '--stub-fixture', fixture, '--json', '--yes',
    '--verify-cmd', 'node check.js',
  ]);
  // VerificationFailedError maps to exit 1 (not the denial exit 2).
  assert.equal(run.code, 1, `stdout=${run.stdout} stderr=${run.stderr}`);

  const events = jsonLines(run.stdout);
  assert.equal(events.some(event => event.type === 'done'), false);

  const rollback = events.find(event => event.type === 'verify_rollback');
  assert.ok(rollback, 'expected a verify_rollback event');
  assert.equal(rollback.reason, 'Model ended the turn with unresolved verification failures.');
  assert.deepEqual(rollback.restored, []);
  assert.deepEqual(rollback.deleted, ['app.js']);
  assert.deepEqual(rollback.conflicts, []);

  const failure = events.filter(event => event.type === 'verify_result').at(-1);
  assert.equal(failure.status, 'fail');

  // The broken edit must not survive the run.
  await assert.rejects(access(`${f.workspace}/app.js`), /ENOENT/);
});
