import test from 'node:test';
import assert from 'node:assert/strict';
import { readdir, readFile } from 'node:fs/promises';
import { join } from 'node:path';
import { setTimeout } from 'node:timers/promises';
import { Harness } from '../dist/harness.js';
import { SessionStore } from '../dist/session.js';
import { Subagents } from '../dist/subagents.js';
import { ToolRegistry } from '../dist/tools.js';
import { fixture, provider, sse, cli } from './helpers.mjs';

const isChild = body => body.messages.some(m => typeof m.content === 'string' && m.content.startsWith('You are a delegated agent.'));
const configFor = (f, p, extra = {}) => ({ cwd: f.cwd, dataDir: f.data, provider: 'openai-compatible', model: 'test-model', baseUrl: p.url,
  apiKey: 'fake', contextWindow: 128000, maxOutputTokens: 8192, timeoutMs: 3000, runTokenBudget: 100000, ...extra });
async function harness(t, f, p, approve, extra) {
  const config = configFor(f, p, extra);
  const store = await SessionStore.open(join(f.data, 'sessions'), config); t.after(() => store.close());
  const tools = new ToolRegistry(f.cwd, approve);
  new Subagents(config, store, tools).register();
  return { store, run: signal => new Harness(config, store, tools).run('parent-private-context', signal ?? new AbortController().signal) };
}

test('real CLI delegates isolated research contexts, caps concurrency at two and saves child sessions', async t => {
  const f = await fixture(t);
  let active = 0, maximum = 0, children = 0;
  const p = await provider(t, async (body, response) => {
    if (isChild(body)) {
      children++; active++; maximum = Math.max(maximum, active);
      assert(!JSON.stringify(body.messages).includes('parent-private-context'));
      assert.deepEqual(body.tools.map(t => t.function.name).sort(), ['list_files', 'read_file', 'search']);
      await setTimeout(60); active--; return sse(response, { text: `Report ${children}` });
    }
    if (!body.messages.some(m => m.role === 'tool')) return sse(response, { calls: [{ id: 'delegate1', name: 'delegate_task', input: { tasks: ['Inspect alpha', 'Inspect beta', 'Inspect gamma'] } }] });
    assert.match(body.messages.at(-1).content, /Session .*\nTask: Inspect alpha/);
    assert.match(body.messages.at(-1).content, /Inspect gamma/);
    return sse(response, { text: 'Reports combined.' });
  });
  const result = await cli(f, p.url, ['--read-only', '--json', '-p', 'parent-private-context']).done;
  assert.equal(result.code, 0, result.stdout + result.stderr);
  assert.equal(maximum, 2); assert.equal(children, 3);
  const events = result.stdout.trim().split('\n').map(JSON.parse), parent = events[0].id;
  assert.equal(events.filter(e => e.type === 'subagent' && e.state === 'done').length, 3);
  const names = await readdir(join(f.data, 'goon/subagents', parent));
  assert.equal(names.filter(n => n.endsWith('.json')).length, 3);
  assert(!names.some(n => n.endsWith('.lock')));
  assert.equal(events.at(-1).text, 'Reports combined.');
});

test('subagent write denial propagates to stop the parent and leaves no side effect', async t => {
  const f = await fixture(t), approvals = [];
  const p = await provider(t, (body, response) => {
    if (isChild(body)) return sse(response, { calls: [{ id: 'write-child', name: 'write_file', input: { path: 'denied.txt', content: 'no' } }] });
    return sse(response, { calls: [{ id: 'edit-agent', name: 'delegate_edit', input: { tasks: ['Write denied.txt'] } }] });
  });
  const h = await harness(t, f, p, async request => { approvals.push(request.tool); return request.tool === 'delegate_edit'; });
  await h.run();
  assert.deepEqual(approvals, ['delegate_edit', 'write_file']);
  assert.equal(p.requests.length, 2);
  assert.equal(h.store.session.messages.at(-1).content[0].output.type, 'error-text');
  await assert.rejects(readFile(join(f.cwd, 'denied.txt')), { code: 'ENOENT' });
  assert(!(await readdir(join(f.data, 'subagents', h.store.session.id))).some(n => n.endsWith('.lock')));
});

test('writable delegated tasks run sequentially and each child mutation uses the parent approval policy', async t => {
  const f = await fixture(t), approvals = [];
  let active = 0, maximum = 0;
  const p = await provider(t, async (body, response) => {
    if (isChild(body)) {
      assert(!body.tools.some(t => t.function.name.startsWith('delegate_')));
      const task = body.messages.find(m => m.role === 'user').content.split('\n').at(-1);
      if (!body.messages.some(m => m.role === 'tool')) {
        active++; maximum = Math.max(maximum, active); await setTimeout(30);
        return sse(response, { calls: [{ id: 'write-child', name: 'write_file', input: { path: task, content: task } }] });
      }
      active--; return sse(response, { text: `Created ${task}` });
    }
    if (!body.messages.some(m => m.role === 'tool')) return sse(response, { calls: [{ id: 'edit-agent', name: 'delegate_edit', input: { tasks: ['a.txt', 'b.txt'] } }] });
    return sse(response, { text: 'Finished edits.' });
  });
  const h = await harness(t, f, p, async request => { approvals.push(request.tool); return true; });
  assert.equal(await h.run(), 'Finished edits.');
  assert.equal(maximum, 1);
  assert.deepEqual(approvals, ['delegate_edit', 'write_file', 'write_file']);
  assert.equal(await readFile(join(f.cwd, 'a.txt'), 'utf8'), 'a.txt');
  assert.equal(await readFile(join(f.cwd, 'b.txt'), 'utf8'), 'b.txt');
});

test('child usage consumes the shared parent run budget', async t => {
  const f = await fixture(t);
  const p = await provider(t, (body, response) => {
    if (isChild(body)) return sse(response, { text: 'Child finished.' });
    return sse(response, { calls: [{ id: 'delegate1', name: 'delegate_task', input: { tasks: ['Inspect files'] } }] });
  });
  const h = await harness(t, f, p, async () => true, { runTokenBudget: 200 });
  await assert.rejects(h.run(), /token budget reached/);
  assert.equal(p.requests.length, 2, 'no parent continuation after parent+child usage exceeds budget');
});

test('SIGINT cancels and drains active subagents and releases all locks', async t => {
  const f = await fixture(t);
  let ready, count = 0;
  const started = new Promise(resolve => { ready = resolve; });
  const p = await provider(t, (body, response) => {
    if (!isChild(body)) return sse(response, { calls: [{ id: 'delegate1', name: 'delegate_task', input: { tasks: ['Wait one', 'Wait two'] } }] });
    response.writeHead(200, { 'content-type': 'text/event-stream' }); response.write(': waiting\n\n');
    if (++count === 2) ready();
  });
  const running = cli(f, p.url, ['--read-only', '--json', '-p', 'delegate']);
  await started; running.child.kill('SIGINT');
  const result = await running.done;
  assert.equal(result.code, 130, result.stdout + result.stderr);
  const parent = JSON.parse(result.stdout.split('\n')[0]).id;
  assert(!(await readdir(join(f.data, 'goon/subagents', parent))).some(n => n.endsWith('.lock')));
  assert(!(await readdir(join(f.data, 'goon/sessions'))).some(n => n.endsWith('.lock')));
  const session = JSON.parse(await readFile(join(f.data, 'goon/sessions', `${parent}.json`), 'utf8'));
  assert.equal(session.messages.at(-1).content[0].toolCallId, 'delegate1');
});

test('--no-subagents removes both delegation tools', async t => {
  const f = await fixture(t);
  const p = await provider(t, (body, response) => {
    assert.deepEqual(body.tools.map(t => t.function.name).sort(), ['list_files', 'read_file', 'search']);
    sse(response, { text: 'Direct run.' });
  });
  const result = await cli(f, p.url, ['--no-subagents', '--read-only', '-p', 'inspect']).done;
  assert.equal(result.code, 0, result.stderr);
});

test('JSON child progress is independent of text token count and carries task only once', async t => {
  const f = await fixture(t);
  const tasks = ['alpha', 'beta'].map(name => `Inspect ${name}: ${'context '.repeat(150)}`.trim());
  const p = await provider(t, (body, response) => {
    if (isChild(body)) {
      if (!body.messages.some(m => m.role === 'tool')) return sse(response, {
        reasoning: 'native child reasoning', textChunks: [...'PREAMBLE'.repeat(100)],
        calls: [{ id: 'child-list', name: 'list_files', input: {} }],
      });
      return sse(response, { reasoning: 'more native reasoning', textChunks: [...'<think>child private</think>FINAL_REPORT'] });
    }
    if (!body.messages.some(m => m.role === 'tool')) return sse(response, { calls: [{ id: 'delegate-many', name: 'delegate_task', input: { tasks } }] });
    assert(!body.messages.at(-1).content.includes('PREAMBLE'));
    assert(!body.messages.at(-1).content.includes('child private'));
    assert.equal(body.messages.at(-1).content.split('FINAL_REPORT').length - 1, 2);
    return sse(response, { text: 'Reports combined.' });
  });
  const result = await cli(f, p.url, ['--show-reasoning', '--read-only', '--json', '-p', 'delegate']).done;
  assert.equal(result.code, 0, result.stdout + result.stderr);
  const events = result.stdout.trim().split('\n').map(JSON.parse);
  const children = events.filter(e => e.type === 'subagent');
  assert.equal(children.length, 12, 'two children: start, two usages, two tool states, terminal; no token mirrors');
  assert(!JSON.stringify(children).includes('PREAMBLE'));
  assert(!JSON.stringify(children).includes('child private'));
  const ids = [...new Set(children.map(e => e.id))]; assert.equal(ids.length, 2);
  for (const id of ids) {
    const own = children.filter(e => e.id === id);
    assert.deepEqual(own.map(e => e.state), ['start', 'progress', 'progress', 'progress', 'progress', 'done']);
    assert(tasks.includes(own[0].task));
    assert(own.slice(1).every(e => !('task' in e)));
    assert.deepEqual(own.filter(e => e.state === 'progress').map(e => e.event.type), ['usage', 'tool', 'tool', 'usage']);
    const root = join(f.data, 'goon/subagents', events[0].id);
    const transcript = await readFile(join(root, `${id}.json`), 'utf8');
    assert(transcript.includes('PREAMBLE')); assert(transcript.includes('child private'));
  }
});

test('terminal child event follows lock release, including cleanup failures; capacity is reclaimed', async t => {
  const { existsSync } = await import('node:fs');
  const f = await fixture(t), events = [];
  const p = await provider(t, (body, response) => {
    if (isChild(body)) return sse(response, { text: 'child report' });
    if (!body.messages.some(m => m.role === 'tool')) return sse(response, { calls: [{ id: 'four', name: 'delegate_task', input: { tasks: ['one', 'two', 'three', 'four'] } }] });
    return sse(response, { text: 'handled child failures' });
  });
  const config = configFor(f, p), store = await SessionStore.open(join(f.data, 'sessions'), config);
  t.after(() => store.close());
  const originalClose = SessionStore.prototype.close;
  let failures = 0;
  SessionStore.prototype.close = async function () {
    await originalClose.call(this);
    if (this.root.includes('/subagents/') && failures++ < 2) throw new Error('injected close failure');
  };
  t.after(() => { SessionStore.prototype.close = originalClose; });
  const tools = new ToolRegistry(f.cwd, async () => true);
  new Subagents(config, store, tools, event => {
    events.push(event);
    if (event.state === 'done' || event.state === 'error') assert(!existsSync(join(config.dataDir, 'subagents', store.session.id, `${event.id}.lock`)));
  }).register();
  const result = await new Harness(config, store, tools).run('four tasks', AbortSignal.timeout(3000));
  assert.equal(result, 'handled child failures');
  assert.equal(events.filter(e => e.state === 'start').length, 4);
  assert.equal(events.filter(e => e.state === 'error').length, 2);
  assert.equal(events.filter(e => e.state === 'done').length, 2);
});

test('model autonomously invokes Codex multi-agent tools (spawn_agent, wait_agent, send_input, close_agent) and flexible delegate_task inputs', async t => {
  const f = await fixture(t), events = [];
  let childTurns = 0;
  const p = await provider(t, (body, response) => {
    if (isChild(body)) {
      childTurns++;
      return sse(response, { text: `Child turn ${childTurns} report` });
    }
    const toolMsgs = body.messages.filter(m => m.role === 'tool');
    if (toolMsgs.length === 0) {
      // Verify the LLM receives full JSON schemas for spawn_agent, wait_agent, delegate_task
      const toolNames = body.tools.map(tl => tl.function.name);
      assert(toolNames.includes('spawn_agent'));
      assert(toolNames.includes('wait_agent'));
      assert(toolNames.includes('send_input'));
      assert(toolNames.includes('close_agent'));
      const spawnTool = body.tools.find(tl => tl.function.name === 'spawn_agent');
      assert(spawnTool.function.parameters.properties.message, 'spawn_agent schema must expose message property to LLM');
      return sse(response, {
        calls: [
          { id: 'spawn-1', name: 'spawn_agent', input: { prompt: 'Explore module A', role: 'explorer' } },
          { id: 'del-single', name: 'delegate_task', input: { task: 'Single string task' } },
        ],
      });
    }
    if (toolMsgs.length === 2) {
      const spawnOut = JSON.parse(toolMsgs[0].content);
      assert.equal(spawnOut.nickname, 'Euclid');
      assert.equal(spawnOut.agent_type, 'explorer');
      assert.match(toolMsgs[1].content, /Child turn/);
      return sse(response, {
        calls: [{ id: 'wait-1', name: 'wait', input: { id: spawnOut.agent_id } }],
      });
    }
    if (toolMsgs.length === 3) {
      const waitOut = JSON.parse(toolMsgs[2].content);
      assert.equal(waitOut.timed_out, false);
      const agentId = Object.keys(waitOut.status)[0];
      assert.match(waitOut.status[agentId].completed, /Child turn/);
      return sse(response, {
        calls: [
          { id: 'send-1', name: 'send_message', input: { id: 'Euclid', prompt: 'Follow-up question', wait: true } },
          { id: 'close-1', name: 'close_agent', input: { id: 'Euclid' } },
        ],
      });
    }
    if (toolMsgs.length >= 4) {
      const sendOut = toolMsgs[3].content;
      assert.match(sendOut, /Child turn 3 report/, `Unexpected send_message output: ${sendOut}`);
    }
    return sse(response, { text: 'All multi-agent steps complete.' });
  });
  const config = configFor(f, p);
  const store = await SessionStore.open(join(f.data, 'sessions'), config);
  t.after(() => store.close());
  const tools = new ToolRegistry(f.cwd, async () => true);
  const subagents = new Subagents(config, store, tools, e => events.push(e));
  subagents.register();
  t.after(() => subagents.closeAll());
  const out = await new Harness(config, store, tools).run('use subagents', AbortSignal.timeout(5000));
  assert.equal(out, 'All multi-agent steps complete.');
  assert.equal(childTurns, 3);
  const summaries = await subagents.listSubagentSummaries();
  assert(summaries.some(s => s.nickname === 'Euclid' && s.status === 'shutdown'));
});

test('Codex tools (update_plan, request_user_input, view_image), $skill-name expansion, /btw side question, and /copy extraction work end-to-end', async t => {
  const { mkdir, writeFile } = await import('node:fs/promises');
  const { discoverSkills, expandFileMentions } = await import('../dist/context.js');
  const { extractCopyableFromMessages } = await import('../dist/cli.js');
  const f = await fixture(t);

  // Create a skill in .codex/skills/audit-sec/SKILL.md
  const skillDir = join(f.cwd, '.codex', 'skills', 'audit-sec');
  await mkdir(skillDir, { recursive: true });
  await writeFile(
    join(skillDir, 'SKILL.md'),
    '---\nname: audit-sec\ndescription: Security audit checklist\n---\nAlways check SQL injection and SSRF.',
  );

  const skills = await discoverSkills(f.cwd);
  assert(skills.some(s => s.name === 'audit-sec' && s.description === 'Security audit checklist'));

  // Create a valid 1x1 PNG file for view_image
  const png1x1 = Buffer.from(
    'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==',
    'base64',
  );
  await writeFile(join(f.cwd, 'sample.png'), png1x1);

  const p = await provider(t, (body, response) => {
    const lastUser = [...body.messages].reverse().find(m => m.role === 'user');
    if (lastUser && typeof lastUser.content === 'string' && lastUser.content.includes('Ephemeral Side Question (/btw)')) {
      return sse(response, { text: 'Side answer: 42' });
    }
    const toolMsgs = body.messages.filter(m => m.role === 'tool');
    if (toolMsgs.length === 0) {
      assert(JSON.stringify(body.messages).includes('Always check SQL injection and SSRF.'));
      return sse(response, {
        calls: [
          {
            id: 'plan-1',
            name: 'update_plan',
            input: {
              explanation: 'Initial steps',
              steps: [
                { step: 'Inspect sample.png', status: 'in_progress' },
                { step: 'Ask user preference', status: 'pending' },
              ],
            },
          },
          {
            id: 'ask-1',
            name: 'ask_user',
            input: {
              id: 'mode_choice',
              question: 'Which mode should we use?',
              options: ['Strict (Recommended)', 'Permissive'],
            },
          },
          {
            id: 'img-1',
            name: 'view_image',
            input: 'sample.png',
          },
        ],
      });
    }
    return sse(response, { text: 'Completed task.\n```ts\nconst ok = true;\n```' });
  });

  const config = configFor(f, p);
  const store = await SessionStore.open(join(f.data, 'sessions'), config);
  t.after(() => store.close());
  const tools = new ToolRegistry(f.cwd, async () => true);
  tools.askUser = async questions => {
    assert.equal(questions.length, 1);
    assert.equal(questions[0].id, 'mode_choice');
    return { mode_choice: ['Strict (Recommended)'] };
  };

  const expanded = await expandFileMentions('Run $audit-sec now', tools.files);
  assert(expanded.attachedPaths.includes('$audit-sec'));

  const h = new Harness(config, store, tools);
  const ans = await h.run('Run $audit-sec now', AbortSignal.timeout(5000));
  assert.match(ans, /Completed task/);
  assert.equal(tools.todos.length, 2);
  assert.equal(tools.todos[0].content, 'Inspect sample.png');

  // Verify /btw askSideQuestion does not mutate session.messages
  const msgCountBefore = store.session.messages.length;
  const side = await h.askSideQuestion('What is the side answer?', AbortSignal.timeout(5000));
  assert.equal(side.text, 'Side answer: 42');
  assert.equal(store.session.messages.length, msgCountBefore);

  // Verify /copy extraction (last response and code block)
  const copiedLast = extractCopyableFromMessages(store.session.messages, 'last');
  assert(copiedLast?.text.includes('Completed task.'));
  const copiedCode = extractCopyableFromMessages(store.session.messages, 'code');
  assert.equal(copiedCode?.text, 'const ok = true;');
});


