import assert from 'node:assert/strict';
import { test } from 'node:test';
import { readdirSync, readFileSync, writeFileSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { fixture, stubCli } from './helpers.mjs';

test('stub-provider: CLI --stub runs without model/base-url/API key and writes the log', async t => {
  const f = await fixture(t);
  const fixtureFile = join(f.root, 'fix.json');
  writeFileSync(fixtureFile, JSON.stringify({
    v: 1,
    turns: [{ text: ['Stub answer one.'], usage: { inputTokens: 21, outputTokens: 9, totalTokens: 30 } }],
  }));
  const run = await stubCli(f, ['--no-mcp', '--no-subagents', '--event-log', '-p', 'hi', '--stub-fixture', fixtureFile]).done;
  assert.equal(run.code, 0);
  assert.ok(run.stdout.includes('Stub answer one.'), run.stdout);
  const defaultRun = await stubCli(f, ['--no-mcp', '--no-subagents', '-p', 'hi']).done;
  assert.equal(defaultRun.code, 0);
  assert.ok(defaultRun.stdout.includes('[stub] deterministic reply'), defaultRun.stdout);
  const logs = readdirSync(join(f.data, 'goon', 'sessions')).filter(name => name.endsWith('.events.jsonl'));
  assert.equal(logs.length >= 1, true);
  const parsed = readFileSync(join(f.data, 'goon', 'sessions', logs[0]), 'utf8').split('\n').filter(Boolean).map(line => JSON.parse(line));
  assert.equal(parsed[0].type, 'session');
  assert.equal(parsed[1].type, 'user');
  assert.equal(parsed[parsed.length - 1].type, 'done');
});

test('stub-provider: --json stays compatible without tool_stream/user', async t => {
  const f = await fixture(t);
  writeFileSync(join(f.cwd, 'notes.txt'), 'note body\n');
  const run = await stubCli(f, ['--json', '--no-mcp', '--no-subagents', '-p', 'read the file', '--stub-fixture',
    resolve('tests/fixtures/stub-tool-call.json')]).done;
  assert.equal(run.code, 0);
  const events = run.stdout.split('\n').filter(Boolean).map(line => JSON.parse(line));
  assert.equal(events.some(event => event.type === 'user'), false);
  assert.equal(events.some(event => event.type === 'tool_stream'), false);
  assert.equal(events.some(event => event.type === 'tool'), true);
  assert.equal(events[events.length - 1].type, 'done');
});
