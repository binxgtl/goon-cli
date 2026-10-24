/**
 * E2E: CLI basics — version/help and the read → edit tool loop over real files, driven by the
 * stub provider and observed through `--json` events, exit codes and on-disk file contents.
 */
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { test } from 'node:test';
import { BASE_FLAGS, e2eFixture, eventTypes, jsonLines, runGoon, writeFixture, writeWorkspaceFile } from './helpers.mjs';

test('e2e: --version prints the CLI version and exits 0', async t => {
  const f = await e2eFixture(t);
  const run = await runGoon(t, f, ['--version']);
  assert.equal(run.code, 0, run.stderr);
  assert.match(run.stdout, /^goon cli \d+\.\d+\.\d+\n$/);
  assert.equal(run.stderr, '');
});

test('e2e: --help prints usage and exits 0', async t => {
  const f = await e2eFixture(t);
  const run = await runGoon(t, f, ['--help']);
  assert.equal(run.code, 0, run.stderr);
  assert.match(run.stdout, /Usage: goon/);
  assert.match(run.stdout, /-p, --prompt/);
  assert.match(run.stdout, /--max-cost-usd/);
  assert.match(run.stdout, /--no-verify/);
  assert.equal(run.stderr, '');
});

test('e2e: stub tool loop reads then edits a file end to end (--json)', async t => {
  const f = await e2eFixture(t);
  await writeWorkspaceFile(f, 'notes.txt', 'alpha\nbeta\n');
  // The stub only matches turn scripts against user *text* (tool results ride in `role: 'tool'`
  // messages), so the read+edit pair shares one turn and the answer follows as the last turn.
  const fixture = await writeFixture(f, 'edit-loop.json', {
    v: 1,
    turns: [
      {
        match: 'update the notes',
        toolCalls: [
          { id: 'call-read', name: 'read_file', input: { path: 'notes.txt' } },
          { id: 'call-edit', name: 'edit_file', input: { path: 'notes.txt', oldText: 'beta', newText: 'gamma' } },
        ],
        usage: { inputTokens: 50, outputTokens: 5, totalTokens: 55 },
      },
      { text: ['Notes updated.'], usage: { inputTokens: 60, outputTokens: 4, totalTokens: 64 } },
    ],
  });
  const run = await runGoon(t, f, [...BASE_FLAGS, '-p', 'update the notes', '--stub-fixture', fixture, '--json', '--yes', '--no-verify']);
  assert.equal(run.code, 0, run.stderr);
  assert.equal(run.stderr, '');

  // The file on disk really changed, and only on the expected line.
  assert.equal(await readFile(`${f.workspace}/notes.txt`, 'utf8'), 'alpha\ngamma\n');

  const events = jsonLines(run.stdout);
  assert.equal(events[0].type, 'session');
  assert.equal(events[0].cwd, f.workspace);
  assert.deepEqual(eventTypes(events), ['session', 'usage', 'tool', 'tool', 'tool', 'tool', 'text', 'usage', 'done']);

  const read = events.filter(event => event.type === 'tool' && event.name === 'read_file');
  assert.equal(read.length, 2); // start + done
  assert.equal(read[0].state, 'start');
  assert.deepEqual(read[0].meta, { kind: 'read', summary: 'notes.txt' });
  assert.equal(read[1].state, 'done');

  const edit = events.filter(event => event.type === 'tool' && event.name === 'edit_file');
  assert.equal(edit.length, 2);
  assert.equal(edit[0].state, 'start');
  assert.equal(edit[0].meta.kind, 'edit');
  assert.equal(edit[0].meta.summary, 'notes.txt');
  assert.match(edit[0].meta.diff, /-beta/);
  assert.match(edit[0].meta.diff, /\+gamma/);
  assert.equal(edit[0].meta.verb, 'Edited');
  assert.equal(edit[1].state, 'done');

  assert.deepEqual(events.find(event => event.type === 'usage'), { type: 'usage', input: 50, output: 5, total: 55 });
  const done = events.find(event => event.type === 'done');
  assert.equal(done.text, 'Notes updated.');
  assert.equal(done.denied, false);
});

test('e2e: default ask mode denies file edits when nobody can approve (headless)', async t => {
  const f = await e2eFixture(t);
  const fixture = await writeFixture(f, 'edit.json', {
    v: 1,
    turns: [
      {
        match: 'create a file',
        toolCalls: [{ id: 'call-write', name: 'write_file', input: { path: 'blocked.txt', content: 'should not exist' } }],
        usage: { inputTokens: 10, outputTokens: 5, totalTokens: 15 },
      },
      { text: ['done'], usage: { inputTokens: 8, outputTokens: 3, totalTokens: 11 } },
    ],
  });
  // No `--yes`: ask mode must consult the user, and headless stdin has no approver.
  const run = await runGoon(t, f, [...BASE_FLAGS, '-p', 'create a file', '--stub-fixture', fixture, '--json', '--no-verify']);
  assert.equal(run.code, 2, `stdout=${run.stdout} stderr=${run.stderr}`);
  assert.equal(run.stderr, '');

  const events = jsonLines(run.stdout);
  const failedTool = events.find(event => event.type === 'tool' && event.state === 'error');
  assert.equal(failedTool.name, 'write_file');
  // The denial text goes back to the model, not into edit-tool event meta (outputPreview is
  // exec-only); the user-visible signals are the stopped event and the denied done event.
  assert.deepEqual(events.find(event => event.type === 'stopped'), { type: 'stopped', reason: 'Permission denied.' });
  const done = events.find(event => event.type === 'done');
  assert.equal(done.denied, true);
  assert.equal(done.text, '');

  await assert.rejects(readFile(`${f.workspace}/blocked.txt`), /ENOENT/);
});
