/**
 * E2E: the approval gate layers, exercised through the real CLI in `--yes` (auto) mode:
 * - critical bash is denied before any approval is consulted;
 * - high-risk bash is never auto-approved and needs a human (denied headless);
 * - network access is default-deny and the blocked command never executes;
 * - `--allow-net-hosts` lifts the network gate only, so the command still stops at approval.
 *
 * Every denial must end the run with exit code 2, a `stopped` event and `done.denied: true`.
 */
import assert from 'node:assert/strict';
import { access, writeFile } from 'node:fs/promises';
import { test } from 'node:test';
import { BASE_FLAGS, e2eFixture, jsonLines, runGoon, writeFixture } from './helpers.mjs';

/** Runs one scenario where the stub model makes exactly one bash call, then asserts the denial. */
async function expectBashDenial(t, { command, flags = [], denyMessage, sentinel }) {
  const f = await e2eFixture(t);
  if (sentinel) await writeFile(`${f.workspace}/${sentinel}`, 'untouched', 'utf8');
  const fixture = await writeFixture(f, 'bash-call.json', {
    v: 1,
    turns: [
      {
        match: 'run it',
        toolCalls: [{ id: 'call-bash', name: 'bash', input: { command } }],
        usage: { inputTokens: 10, outputTokens: 5, totalTokens: 15 },
      },
      { text: ['finished'], usage: { inputTokens: 8, outputTokens: 3, totalTokens: 11 } },
    ],
  });
  const run = await runGoon(t, f, [...BASE_FLAGS, '-p', 'run it', '--stub-fixture', fixture, '--json', '--yes', '--no-verify', ...flags]);
  assert.equal(run.code, 2, `stdout=${run.stdout} stderr=${run.stderr}`);
  assert.equal(run.stderr, '');

  const events = jsonLines(run.stdout);
  const failed = events.find(event => event.type === 'tool' && event.state === 'error');
  assert.equal(failed.name, 'bash');
  assert.equal(failed.meta.summary, command);
  assert.deepEqual(failed.meta.outputPreview, [denyMessage]);
  assert.deepEqual(events.find(event => event.type === 'stopped'), { type: 'stopped', reason: 'Permission denied.' });
  const done = events.find(event => event.type === 'done');
  assert.equal(done.denied, true);
  assert.equal(done.text, '');
  // The "finished" fallback turn must never have been reached.
  assert.equal(events.some(event => event.type === 'text'), false);
  return f;
}

test('e2e: critical bash (rm -rf /) is denied even in auto mode, nothing runs', async t => {
  const f = await expectBashDenial(t, {
    command: 'rm -rf /',
    denyMessage: 'Permission denied: bash is critical risk and cannot run. Stop this run.',
    sentinel: 'sentinel.txt',
  });
  await assert.doesNotReject(access(`${f.workspace}/sentinel.txt`));
});

test('e2e: high-risk bash (git reset --hard) needs a human approver even in auto mode', async t => {
  await expectBashDenial(t, {
    command: 'git reset --hard',
    denyMessage: 'Permission denied by the user. Stop this run.',
  });
});

test('e2e: network is default-deny — curl is blocked by the policy and never executes', async t => {
  const f = await expectBashDenial(t, {
    command: 'curl --silent https://example.invalid/payload -o network-marker.txt',
    denyMessage: 'Permission denied: network access is blocked by the execution policy for this host. '
      + 'Use --allow-net or --allow-net-hosts=HOST to permit it.',
  });
  // The policy gate denies before spawn, so the command's output file must not exist.
  await assert.rejects(access(`${f.workspace}/network-marker.txt`), /ENOENT/);
});

test('e2e: --allow-net-hosts lifts the network gate but the high-risk command still needs approval', async t => {
  await expectBashDenial(t, {
    command: 'curl --silent https://example.invalid/payload -o network-marker.txt',
    flags: ['--allow-net-hosts', 'example.invalid'],
    // Past the network gate, stopped at the approval layer (no human headless), not the policy.
    denyMessage: 'Permission denied by the user. Stop this run.',
  });
});
