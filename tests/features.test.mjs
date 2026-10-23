import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdir, writeFile } from 'node:fs/promises';
import { join } from 'node:path';
import {
  PairManager,
  encodeQrMatrix,
  renderQrTerminal,
  AgentNetwork,
  AgentNetworkBus,
  Subagents,
  SessionStore,
  ToolRegistry,
  discoverWorkflows,
  createWorkflowScaffold,
  removeCustomWorkflow,
  executeWorkflow,
  parseMarkdownWorkflow,
  interpolateWorkflowTemplate,
} from '../dist/index.js';
import { fixture, provider, sse, cli } from './helpers.mjs';

test('QR encoder produces valid ISO/IEC 18004 finder patterns and half-block terminal output', () => {
  const url = 'https://goon-test-tunnel.trycloudflare.com/?t=abc123xyz789';
  const matrix = encodeQrMatrix(url);
  assert(matrix.length >= 21 && matrix.length <= 57);
  assert.equal(matrix.length, matrix[0].length);
  // Top-left 7x7 finder corners must be dark (true)
  assert.equal(matrix[0][0], true);
  assert.equal(matrix[0][6], true);
  assert.equal(matrix[6][0], true);
  assert.equal(matrix[6][6], true);
  assert.equal(matrix[3][3], true);

  const rendered = renderQrTerminal(url, '  ');
  const lines = rendered.split('\n');
  assert(lines.length >= 12);
  assert(lines.every(l => l.startsWith('  ')));
  assert(/[▀▄█]/.test(rendered));
});

test('PairManager serves mobile UI, enforces token auth, streams SSE events, and resolves remote approvals', async t => {
  const receivedPrompts = [];
  const receivedModes = [];
  let stopCalled = 0;

  const pm = new PairManager({
    getState: () => ({
      model: 'test-model',
      provider: 'openai-compatible',
      cwd: '/workspace/demo',
      sessionId: '12345678-abcd',
      mode: 'ask',
      status: 'Ready',
      tokens: 420,
      contextWindow: 128000,
    }),
    onRemotePrompt: p => receivedPrompts.push(p),
    onRemoteModeChange: m => receivedModes.push(m),
    onRemoteStop: () => {
      stopCalled++;
    },
  });

  t.after(() => pm.stop());
  const info = await pm.start({ skipTunnel: true });
  assert.equal(info.running, true);
  assert(info.port > 0);
  assert.equal(info.tunnelStatus, 'fallback_lan');
  assert(pm.formatBanner().includes(info.localUrl));

  // 1. Unauthorized request without token -> 401
  const unauthRes = await fetch(`http://127.0.0.1:${info.port}/api/state`);
  assert.equal(unauthRes.status, 401);

  // 2. Authorized HTML & state requests -> 200
  const htmlRes = await fetch(info.localUrl);
  assert.equal(htmlRes.status, 200);
  const htmlText = await htmlRes.text();
  assert(htmlText.includes('goon remote'));

  const stateRes = await fetch(`http://127.0.0.1:${info.port}/api/state?t=${info.token}`);
  assert.equal(stateRes.status, 200);
  const stateJson = await stateRes.json();
  assert.equal(stateJson.state.model, 'test-model');

  // 3. Remote prompt, mode change, and stop endpoints
  const promptRes = await fetch(`http://127.0.0.1:${info.port}/api/prompt?t=${info.token}`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ prompt: 'Run unit tests from phone' }),
  });
  assert.equal(promptRes.status, 200);
  assert.deepEqual(receivedPrompts, ['Run unit tests from phone']);

  const modeRes = await fetch(`http://127.0.0.1:${info.port}/api/mode?t=${info.token}`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ mode: 'auto' }),
  });
  assert.equal(modeRes.status, 200);
  assert.deepEqual(receivedModes, ['auto']);

  const stopRes = await fetch(`http://127.0.0.1:${info.port}/api/stop?t=${info.token}`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({}),
  });
  assert.equal(stopRes.status, 200);
  assert.equal(stopCalled, 1);

  // 4. Remote tool approval flow
  const ac = new AbortController();
  const pending = pm.requestRemoteApproval('bash', '$ rm -rf dist', ac.signal);
  const stateWithPending = await (await fetch(`http://127.0.0.1:${info.port}/api/state?t=${info.token}`)).json();
  assert.equal(stateWithPending.pendingApprovals.length, 1);
  assert.equal(stateWithPending.pendingApprovals[0].id, pending.id);

  const approveRes = await fetch(`http://127.0.0.1:${info.port}/api/approve?t=${info.token}`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ id: pending.id, decision: 'always' }),
  });
  assert.equal(approveRes.status, 200);
  const decision = await pending.promise;
  assert.equal(decision, 'always');

  // 5. Verify messages & streaming assistant output are synchronized via /api/state
  pm.notifyEvent({ type: 'text', text: 'Hello ' });
  pm.notifyEvent({ type: 'text', text: 'from assistant!' });
  const synced = await (await fetch(`http://127.0.0.1:${info.port}/api/state?t=${info.token}`)).json();
  assert(Array.isArray(synced.entries));
  assert(synced.entries.some(e => e.kind === 'user' && e.text.includes('Run unit tests from phone')));
  assert(synced.entries.some(e => e.kind === 'assistant' && e.text === 'Hello from assistant!'));
});

test('Custom Workflows: discovery, scaffolding, Markdown parsing, interpolation, and multi-step execution', async t => {
  const f = await fixture(t);

  // 1. Template interpolation
  const interpolated = interpolateWorkflowTemplate(
    'Task: $input | First: $1 | Named: {{target}} | Prev: $prev | Step1: {{steps.s1.output}}',
    {
      input: 'auth module',
      positional: ['auth', 'module'],
      named: { target: 'src/auth.ts' },
      prevOutput: 'prev-ok',
      stepOutputs: { s1: 'step1-done' },
    },
  );
  assert.equal(
    interpolated,
    'Task: auth module | First: auth | Named: src/auth.ts | Prev: prev-ok | Step1: step1-done',
  );

  // 2. Scaffold JSON workflow & discover alongside built-ins
  const created = await createWorkflowScaffold(f.cwd, 'my-pipeline', 'Custom release pipeline');
  assert.equal(created.workflow.name, 'my-pipeline');

  // 3. Markdown workflow parsing & discovery
  const mdDir = join(f.cwd, '.goon', 'workflows');
  await mkdir(mdDir, { recursive: true });
  const mdContent = [
    '---',
    'name: md-flow',
    'description: Markdown multi-step workflow',
    '---',
    '## Step 1: Check Environment [type=shell id=env_check]',
    '```bash',
    'echo "env-ok-$1"',
    '```',
    '## Step 2: Brainstorm [type=network id=net roles=architect|critic rounds=1]',
    'Design $input with context: $prev',
    '## Step 3: Implement [type=prompt id=impl]',
    'Implement $input using:\n{{steps.net.output}}',
  ].join('\n');
  await writeFile(join(mdDir, 'md-flow.md'), mdContent, 'utf8');

  const parsedMd = parseMarkdownWorkflow('md-flow.md', mdContent, 'workspace', join(mdDir, 'md-flow.md'));
  assert.equal(parsedMd.steps.length, 3);
  assert.equal(parsedMd.steps[0].type, 'shell');
  assert.equal(parsedMd.steps[1].type, 'network');
  assert.deepEqual(parsedMd.steps[1].roles, ['architect', 'critic']);
  assert.equal(parsedMd.steps[2].type, 'prompt');

  const all = await discoverWorkflows(f.cwd);
  assert(all.some(w => w.name === 'my-pipeline' && w.scope === 'workspace'));
  assert(all.some(w => w.name === 'md-flow' && w.scope === 'workspace'));
  assert(all.some(w => w.name === 'feature-dev' && w.scope === 'builtin'));

  // 4. Execute multi-step workflow
  const promptsSeen = [];
  const runRes = await executeWorkflow(parsedMd, 'login-page --target=auth', {
    cwd: f.cwd,
    signal: AbortSignal.timeout(10_000),
    runPrompt: async promptText => {
      promptsSeen.push(promptText);
      return 'implemented-ok';
    },
    runNetwork: async (topic, roles, rounds) => {
      assert.match(topic, /env-ok-login-page/);
      assert.deepEqual(roles, ['architect', 'critic']);
      assert.equal(rounds, 1);
      return 'consensus-blueprint-ready';
    },
  });
  assert.equal(runRes.status, 'completed');
  assert.equal(runRes.steps.length, 3);
  assert.equal(runRes.finalOutput, 'implemented-ok');
  assert.match(promptsSeen[0], /consensus-blueprint-ready/);

  // 5. CLI subcommands for workflow list & rm
  const cliList = await cli(f, 'http://127.0.0.1:9', ['workflow', 'list', '--json']).done;
  assert.equal(cliList.code, 0, cliList.stderr);
  const parsedList = JSON.parse(cliList.stdout);
  assert(parsedList.some(w => w.name === 'my-pipeline'));

  await removeCustomWorkflow(f.cwd, 'my-pipeline');
  const afterRm = await discoverWorkflows(f.cwd);
  assert(!afterRm.some(w => w.name === 'my-pipeline'));
});

test('Agent Network: P2P message bus between subagents and multi-round Brainstorm / Council deliberation', async t => {
  const f = await fixture(t);

  // Part A: Direct P2P bus messaging & blackboard
  const bus = new AgentNetworkBus();
  bus.registerNode({ id: 'a1', nickname: 'Euclid', role: 'explorer', status: 'running' });
  bus.registerNode({ id: 'a2', nickname: 'Archimedes', role: 'worker', status: 'running' });

  bus.send({ from: 'Euclid', to: 'Archimedes', kind: 'finding', topic: 'auth', content: 'Found bug in line 42' });
  bus.send({ from: 'Euclid', to: '*', kind: 'idea', content: 'Use token bucket rate limiting' });
  bus.postBlackboard({
    author: 'Euclid',
    role: 'explorer',
    topic: 'Architecture',
    content: 'Proposed schema v2',
    kind: 'idea',
  });

  // Euclid should not see its own messages as unread
  assert.equal(bus.readInbox('Euclid').length, 0);
  // Archimedes should receive both the direct message and the '*' broadcast
  const archInbox = bus.drainUnreadPromptBlock('Archimedes');
  assert.match(archInbox, /Found bug in line 42/);
  assert.match(archInbox, /Use token bucket rate limiting/);
  // Subsequent drain is empty since messages were marked read
  assert.equal(bus.drainUnreadPromptBlock('Archimedes'), '');
  assert.match(bus.formatBoardSummary(), /Proposed schema v2/);

  // Part B: Full multi-round AgentNetwork brainstorm + P2P tool invocation
  let netCalls = 0;
  const p = await provider(t, (body, response) => {
    netCalls++;
    const userMsg = body.messages.find(m => m.role === 'user')?.content ?? '';
    if (typeof userMsg === 'string' && userMsg.includes('Lead Moderator')) {
      assert.match(userMsg, /Architect proposal R1/);
      assert.match(userMsg, /Critic proposal R1/);
      assert.match(userMsg, /Architect debate R2/);
      return sse(response, { text: '### Consensus\nUse clean event-driven architecture.' });
    }
    if (typeof userMsg === 'string' && userMsg.includes('Instructions for Round 1:')) {
      const isArch = userMsg.startsWith('You are **Architect**');
      if (!body.messages.some(m => m.role === 'tool') && isArch) {
        return sse(response, {
          calls: [
            {
              id: 'p_send_1',
              name: 'peer_send',
              input: { to: 'Critic', message: 'What edge cases do you see in SSE?', kind: 'question' },
            },
          ],
        });
      }
      return sse(response, { text: isArch ? 'Architect proposal R1' : 'Critic proposal R1' });
    }
    if (typeof userMsg === 'string' && userMsg.includes('Instructions for Round 2:')) {
      const isCritic = userMsg.startsWith('You are **Critic**');
      if (isCritic) {
        // Critic must automatically receive Architect's direct P2P question in its Round 2 inbox!
        assert.match(userMsg, /What edge cases do you see in SSE\?/);
      }
      return sse(response, { text: isCritic ? 'Critic debate R2' : 'Architect debate R2' });
    }
    return sse(response, { text: 'Fallback' });
  });

  const config = {
    cwd: f.cwd,
    dataDir: f.data,
    provider: 'openai-compatible',
    model: 'test-model',
    baseUrl: p.url,
    apiKey: 'fake',
    contextWindow: 128000,
    maxOutputTokens: 8192,
    timeoutMs: 5000,
    runTokenBudget: 100000,
  };
  const store = await SessionStore.open(join(f.data, 'sessions'), config);
  t.after(() => store.close());

  const net = new AgentNetwork(config, store, bus);
  const result = await net.brainstorm({
    topic: 'Design real-time SSE pairing',
    roles: ['architect', 'critic'],
    rounds: 2,
    exploreCodebase: true,
    signal: AbortSignal.timeout(10_000),
  });

  assert.equal(result.rounds, 2);
  assert.equal(result.participants.length, 2);
  assert.equal(result.contributions.length, 4);
  assert.match(result.synthesis, /Use clean event-driven architecture/);
  assert(netCalls >= 6);

  // Part C: Verify Subagents registers agent_network and peer_* tools on parent registry
  const tools = new ToolRegistry(f.cwd, async () => true);
  const sub = new Subagents(config, store, tools);
  sub.register();
  t.after(() => sub.closeAll());
  const activeSchemas = Object.keys(tools.schemas());
  assert(activeSchemas.includes('agent_network'));
  assert(activeSchemas.includes('peer_send'));
  assert(activeSchemas.includes('peer_broadcast'));
  assert(activeSchemas.includes('peer_read'));
  assert(activeSchemas.includes('peer_list'));
});
