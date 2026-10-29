import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdir, readFile, readdir, writeFile } from 'node:fs/promises';
import { existsSync } from 'node:fs';
import { join } from 'node:path';
import { setTimeout } from 'node:timers/promises';
import { fixture, provider, sse, cli } from './helpers.mjs';

const isChild = body => body.messages.some(m => typeof m.content === 'string' && m.content.startsWith('You are a delegated agent.'));
const isPlanner = body => body.messages.some(m => m.role === 'system' && typeof m.content === 'string' && m.content.includes('planning assistant'));
test('/dag CLI decomposes, spawns a coder child, verifies with auto-approved commands and merges', async t => {
  const f = await fixture(t);
  await mkdir(join(f.config, 'goon'), { recursive: true });
  await writeFile(join(f.config, 'goon', 'config.json'), JSON.stringify({ verify: { commands: ['node --version'] } }));
  const p = await provider(t, (body, response) => {
    if (isPlanner(body)) {
      return sse(response, { text: JSON.stringify({
        goal: 'create a.txt',
        todos: [{ id: 't1', title: 'write a.txt', acceptance: 'a.txt contains the dag output', role: 'coder' }],
      }) });
    }
    if (isChild(body)) {
      const childTools = body.tools.map(tool => tool.function.name).sort();
      assert.deepEqual(childTools, ['apply_patch', 'bash', 'edit_file', 'list_files', 'read_file', 'search', 'write_file'],
        `coder toolset must be pinned: ${childTools}`);
      if (!body.messages.some(m => m.role === 'tool')) {
        return sse(response, { calls: [{ id: 'w1', name: 'write_file', input: { path: 'a.txt', content: 'dag output' } }] });
      }
      return sse(response, { text: 'Wrote a.txt.' });
    }
    return sse(response, { text: 'unexpected non-dag request' });
  });
  const result = await cli(f, p.url, ['--no-tui', '--json', '--yes', 'dag', 'create a.txt with the dag output']).done;
  assert.equal(result.code, 0, result.stdout + result.stderr);
  const events = result.stdout.trim().split('\n').map(line => { try { return JSON.parse(line); } catch { return null; } }).filter(Boolean);
  assert.ok(events.some(e => e.type === 'dag_plan_ready' && e.todos === 1), JSON.stringify(events));
  assert.ok(events.some(e => e.type === 'dag_todo_update' && e.todoId === 't1' && e.status === 'done' && e.role === 'coder'));
  assert.ok(events.some(e => e.type === 'dag_merge' && e.verified === true), JSON.stringify(events));
  assert.equal(await readFile(join(f.cwd, 'a.txt'), 'utf8'), 'dag output');
  // The verify commands ran through the approve callback (auto-approved): both test and final verify.
  const verifyEvents = events.filter(e => e.type === 'verify_start');
  assert.ok(verifyEvents.length >= 2, 'test stage and final verification both ran');
});

test('/dag blocks a hidden tool call at the child registry and the run continues', async t => {
  const f = await fixture(t);
  const p = await provider(t, (body, response) => {
    if (isPlanner(body)) {
      return sse(response, { text: JSON.stringify({
        goal: 'g',
        todos: [{ id: 't1', title: 'write a.txt', role: 'coder' }],
      }) });
    }
    if (isChild(body)) {
      if (!body.messages.some(m => m.role === 'tool')) {
        // todo_write is not in the coder role toolset: execute() must reject it.
        return sse(response, { calls: [{ id: 'hidden', name: 'todo_write', input: { todos: [] } }] });
      }
      if (body.messages.filter(m => m.role === 'tool').length === 1) {
        return sse(response, { calls: [{ id: 'w1', name: 'write_file', input: { path: 'a.txt', content: 'ok' } }] });
      }
      return sse(response, { text: 'Recovered and wrote the file.' });
    }
    return sse(response, { text: 'unexpected' });
  });
  const result = await cli(f, p.url, ['--no-tui', '--json', '--yes', 'dag', 'do it']).done;
  assert.equal(result.code, 0, result.stdout + result.stderr);
  const events = result.stdout.trim().split('\n').map(line => { try { return JSON.parse(line); } catch { return null; } }).filter(Boolean);
  assert.ok(events.some(e => e.type === 'dag_merge' && e.verified === true));
  const parent = events.find(e => e.type === 'session').id;
  const childDir = join(f.data, 'goon', 'subagents', parent);
  const transcripts = await Promise.all((await readdir(childDir)).filter(n => n.endsWith('.json')).map(n => readFile(join(childDir, n), 'utf8')));
  assert(transcripts.some(text => text.includes('not available')), 'the child transcript records the hidden-tool denial');
  assert.equal(await readFile(join(f.cwd, 'a.txt'), 'utf8'), 'ok');
});

test('/dag is refused when multiAgentMode is off', async t => {
  const f = await fixture(t);
  await mkdir(join(f.config, 'goon'), { recursive: true });
  await writeFile(join(f.config, 'goon', 'config.json'), JSON.stringify({ multiAgentMode: 'off' }));
  const p = await provider(t, (body, response) => sse(response, { text: 'never called' }));
  const result = await cli(f, p.url, ['--no-tui', '--json', 'dag', 'write files']).done;
  assert.equal(result.code, 0, result.stdout + result.stderr);
  assert.equal(p.requests.length, 0, 'no model request is made when /dag is disabled');
  assert.match(result.stderr, /disabled/);
});
