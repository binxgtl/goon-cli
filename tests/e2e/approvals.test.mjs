/**
 * E2E: the approval gate layers across default `ask` mode vs unrestricted `--yes` / `--yolo` mode:
 * - in `ask` mode: critical bash is denied before approval, high-risk bash needs a human (denied headless),
 *   network access is default-deny, and `--allow-net-hosts` lifts only the network gate;
 * - in `--yes` / `--yolo` / `-y` (`auto`) mode: all risk levels, shell substitutions, network access,
 *   and outside-workspace file reads/writes are permitted without `--allow-net`.
 */
import assert from 'node:assert/strict';
import http from 'node:http';
import { access, readFile, writeFile } from 'node:fs/promises';
import { join } from 'node:path';
import { test } from 'node:test';
import { BASE_FLAGS, e2eFixture, jsonLines, runGoon, writeFixture } from './helpers.mjs';

/** Runs one scenario in default `ask` mode where the stub model makes one bash call, then asserts denial. */
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
  const run = await runGoon(t, f, [...BASE_FLAGS, '-p', 'run it', '--stub-fixture', fixture, '--json', '--no-verify', ...flags]);
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

test('e2e: critical bash (rm -rf /) is denied in ask mode, nothing runs', async t => {
  const f = await expectBashDenial(t, {
    command: 'rm -rf /',
    denyMessage: 'Permission denied: bash is critical risk and cannot run. Stop this run.',
    sentinel: 'sentinel.txt',
  });
  await assert.doesNotReject(access(`${f.workspace}/sentinel.txt`));
});

test('e2e: high-risk bash (git reset --hard) needs a human approver in ask mode', async t => {
  await expectBashDenial(t, {
    command: 'git reset --hard',
    denyMessage: 'Permission denied by the user. Stop this run.',
  });
});

test('e2e: network is default-deny in ask mode — curl is blocked by the policy and never executes', async t => {
  const f = await expectBashDenial(t, {
    command: 'curl --silent https://example.invalid/payload -o network-marker.txt',
    denyMessage: 'Permission denied: network access is blocked by the execution policy for this host. '
      + 'Use --allow-net or --allow-net-hosts=HOST to permit it.',
  });
  // The policy gate denies before spawn, so the command's output file must not exist.
  await assert.rejects(access(`${f.workspace}/network-marker.txt`), /ENOENT/);
});

test('e2e: --allow-net-hosts lifts the network gate in ask mode but the high-risk command still needs approval', async t => {
  await expectBashDenial(t, {
    command: 'curl --silent https://example.invalid/payload -o network-marker.txt',
    flags: ['--allow-net-hosts', 'example.invalid'],
    // Past the network gate, stopped at the approval layer (no human headless), not the policy.
    denyMessage: 'Permission denied by the user. Stop this run.',
  });
});

test('e2e: --yolo / --yes grants full unrestricted permissions (network, critical shell & outside-workspace files)', async t => {
  const server = http.createServer((_req, res) => {
    res.writeHead(200, { 'content-type': 'application/json' });
    res.end(JSON.stringify({ repo: 'morluto/rea', ok: true }));
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  t.after(() => server.close());
  const { port } = server.address();

  for (const modeFlag of ['--yolo', '--yes', '-y']) {
    const f = await e2eFixture(t);
    const outsideFile = join(f.root, `outside-${modeFlag.replace(/^-+/, '')}.txt`);
    const fixture = await writeFixture(f, 'yolo-full.json', {
      v: 1,
      turns: [
        {
          match: 'install plugin',
          toolCalls: [
            {
              id: 'call-net',
              name: 'bash',
              input: { command: `curl -sS http://127.0.0.1:${port}/repo && echo $(printf subshell-ok)` },
            },
            {
              id: 'call-outside-write',
              name: 'write_file',
              input: { path: outsideFile, content: 'installed=true\n' },
            },
          ],
          usage: { inputTokens: 12, outputTokens: 8, totalTokens: 20 },
        },
        {
          text: ['all done'],
          usage: { inputTokens: 10, outputTokens: 4, totalTokens: 14 },
        },
      ],
    });

    const run = await runGoon(t, f, [...BASE_FLAGS, modeFlag, '-p', 'install plugin', '--stub-fixture', fixture, '--json', '--no-verify']);
    assert.equal(run.code, 0, `${modeFlag}: stdout=${run.stdout} stderr=${run.stderr}`);
    const events = jsonLines(run.stdout);
    const errors = events.filter(event => event.type === 'tool' && event.state === 'error');
    assert.deepEqual(errors, [], `${modeFlag} should not fail any tool call`);
    const done = events.find(event => event.type === 'done');
    assert.equal(done.denied, false);
    assert.equal(done.text, 'all done');
    assert.equal(await readFile(outsideFile, 'utf8'), 'installed=true\n');
  }
});
