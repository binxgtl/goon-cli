import test from 'node:test';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { mkdir, writeFile, readdir, readFile } from 'node:fs/promises';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { StreamableHTTPServerTransport } from '@modelcontextprotocol/sdk/server/streamableHttp.js';
import { McpManager, mcpConfigSchema, mcpToolName } from '../dist/mcp.js';
import { ToolRegistry } from '../dist/tools.js';
import { classifyMcpRisk } from '../dist/risk.js';
import { resolveExecutionPolicy } from '../dist/policy.js';
import { mockMcpServer } from './fixtures/mcp-server.mjs';
import { fixture, cli, provider, sse } from './helpers.mjs';

const stdio = extra => mcpConfigSchema.parse({ servers: { local: { transport: 'stdio', command: process.execPath,
  args: [fileURLToPath(new URL('./fixtures/mcp-server.mjs', import.meta.url)), '--stdio'], ...extra } } });
const annotations = extra => mcpConfigSchema.parse({ servers: { ann: { transport: 'stdio', command: process.execPath,
  args: [fileURLToPath(new URL('./fixtures/mcp-server.mjs', import.meta.url)), '--annotations'], ...extra } } });
const call = (toolName, input = {}) => ({ toolName, input, toolCallId: 'fixture' });
const signal = () => new AbortController().signal;

test('MCP stdio discovers pages, validates arguments, preserves approvals and unregisters on close', async t => {
  const f = await fixture(t);
  let approvals = 0;
  const registry = new ToolRegistry(f.cwd, async () => { approvals++; return false; });
  const manager = new McpManager(stdio({ readOnlyTools: ['echo', 'wait', 'fail'] }), f.cwd);
  t.after(() => manager.close());
  await manager.connect(registry);
  assert.equal(manager.tools.length, 4);
  const echo = mcpToolName('local', 'echo'), danger = mcpToolName('local', 'danger-tool');
  assert.equal((await registry.execute(call(echo, { text: 'hello' }), signal())).text, 'hello');
  await assert.rejects(registry.execute(call(echo, { text: 4 }), signal()), /Invalid arguments/);
  assert.equal((await registry.execute(call(danger), signal())).denied, true);
  assert.equal(approvals, 1, 'server readOnlyHint cannot bypass approvals');
  await assert.rejects(registry.execute(call(mcpToolName('local', 'fail')), signal()), /Tool failed/);
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), 100);
  await assert.rejects(registry.execute(call(mcpToolName('local', 'wait')), controller.signal));
  clearTimeout(timer);
  await manager.close();
  assert(!Object.keys(registry.schemas()).includes(echo));
  await manager.close();
});

test('MCP read-only mode uses the user allowlist and stdio forwards only configured secrets', async t => {
  const f = await fixture(t);
  const oldKey = process.env.GOON_API_KEY, oldForwarded = process.env.GOON_TEST_FORWARD;
  process.env.GOON_API_KEY = 'must-not-inherit'; process.env.GOON_TEST_FORWARD = 'explicit-value';
  t.after(() => {
    if (oldKey === undefined) delete process.env.GOON_API_KEY; else process.env.GOON_API_KEY = oldKey;
    if (oldForwarded === undefined) delete process.env.GOON_TEST_FORWARD; else process.env.GOON_TEST_FORWARD = oldForwarded;
  });
  const registry = new ToolRegistry(f.cwd, async () => true, true);
  const manager = new McpManager(stdio({ readOnlyTools: ['echo'], envFrom: { FORWARDED: 'GOON_TEST_FORWARD' } }), f.cwd);
  t.after(() => manager.close());
  await manager.connect(registry);
  assert(!Object.keys(registry.schemas()).includes(mcpToolName('local', 'danger-tool')));
  const result = await registry.execute(call(mcpToolName('local', 'echo'), { text: 'env' }), signal());
  assert.deepEqual(JSON.parse(result.text), { forwarded: 'explicit-value', apiKey: null });
  await manager.close();
});

test('MCP Streamable HTTP uses bearer env credentials and executes real protocol calls', async t => {
  const f = await fixture(t), clients = [];
  const http = createServer(async (request, response) => {
    assert.equal(request.headers.authorization, 'Bearer goon-mcp-test-token');
    if (request.method !== 'POST') { response.writeHead(405).end(); return; }
    const server = mockMcpServer(); clients.push(server);
    const transport = new StreamableHTTPServerTransport({ sessionIdGenerator: undefined, enableJsonResponse: true });
    await server.connect(transport);
    await transport.handleRequest(request, response);
    response.on('close', () => server.close());
  });
  await new Promise(resolve => http.listen(0, '127.0.0.1', resolve));
  t.after(async () => { await Promise.all(clients.map(c => c.close())); http.closeAllConnections(); await new Promise(resolve => http.close(resolve)); });
  process.env.GOON_MCP_TEST_TOKEN = 'goon-mcp-test-token';
  t.after(() => { delete process.env.GOON_MCP_TEST_TOKEN; });
  const config = mcpConfigSchema.parse({ servers: { remote: { transport: 'http', url: `http://127.0.0.1:${http.address().port}/mcp`,
    bearerEnv: 'GOON_MCP_TEST_TOKEN', readOnlyTools: ['echo'] } } });
  const registry = new ToolRegistry(f.cwd, async () => false);
  const manager = new McpManager(config, f.cwd); t.after(() => manager.close());
  await manager.connect(registry);
  assert.equal((await registry.execute(call(mcpToolName('remote', 'echo'), { text: 'remote works' }), signal())).text, 'remote works');
  await manager.close();
});

test('real CLI dispatches MCP tools and startup failure releases session locks', async t => {
  const f = await fixture(t);
  await mkdir(join(f.config, 'goon'), { recursive: true });
  const path = join(f.config, 'goon/mcp.json');
  await writeFile(path, JSON.stringify(stdio({ readOnlyTools: ['echo'] })));
  const echo = mcpToolName('local', 'echo');
  const p = await provider(t, (body, response, n) => {
    if (body.messages.some(m => m.content === 'bypass broken config')) return sse(response, { text: 'Bypassed.' });
    if (n === 1) { assert(body.tools.some(t => t.function.name === echo)); return sse(response, { calls: [{ id: 'echo1', name: echo, input: { text: 'CLI MCP' } }] }); }
    assert.equal(body.messages.at(-1).content, 'CLI MCP'); return sse(response, { text: 'MCP done.' });
  });
  const result = await cli(f, p.url, ['--read-only', '-p', 'use MCP']).done;
  assert.equal(result.code, 0, result.stderr);
  assert.match(result.stdout, /MCP done/);
  await writeFile(path, JSON.stringify({ servers: { broken: { transport: 'stdio', command: 'goon-nonexistent-binary-123' } } }));
  const broken = await cli(f, p.url, ['-p', 'do nothing']).done;
  assert.equal(broken.code, 1);
  assert.deepEqual((await readdir(join(f.data, 'goon/sessions'))).filter(n => n.endsWith('.lock')), []);
  const bypass = await cli(f, p.url, ['--no-mcp', '-p', 'bypass broken config']).done;
  assert.equal(bypass.code, 0, bypass.stderr);
});

test('MCP request timeout leaves no registered tools and redacts configured credentials', async t => {
  const f = await fixture(t);
  process.env.GOON_MCP_ERROR_TOKEN = 'mcp-error-secret';
  t.after(() => { delete process.env.GOON_MCP_ERROR_TOKEN; });
  const http = createServer(async (request, response) => {
    for await (const _ of request) { /* drain the request */ }
    response.writeHead(401, { 'content-type': 'text/plain' }); response.end('Rejected mcp-error-secret');
  });
  await new Promise(resolve => http.listen(0, '127.0.0.1', resolve));
  t.after(() => { http.closeAllConnections(); return new Promise(resolve => http.close(resolve)); });
  const registry = new ToolRegistry(f.cwd, async () => true);
  const config = mcpConfigSchema.parse({ servers: { broken: { transport: 'http', url: `http://127.0.0.1:${http.address().port}/mcp`,
    bearerEnv: 'GOON_MCP_ERROR_TOKEN', timeoutMs: 200 } } });
  const manager = new McpManager(config, f.cwd);
  await assert.rejects(manager.connect(registry), error => !error.message.includes('mcp-error-secret'));
  assert.deepEqual(manager.tools, []);
  const timeout = new McpManager(stdio({ timeoutMs: 100 }), f.cwd);
  // A real child that starts but never responds to initialize.
  timeout.config.servers.local.args = ['-e', 'process.stdin.resume()'];
  const start = Date.now();
  await assert.rejects(timeout.connect(registry), /timed out|timeout/i);
  assert(Date.now() - start < 5000);
  assert(!Object.keys(registry.schemas()).some(n => n.startsWith('mcp_')));
});

test('SIGTERM during MCP initialization preserves exit status, kills the server and unlocks the session', async t => {
  const f = await fixture(t);
  await mkdir(join(f.config, 'goon'), { recursive: true });
  await writeFile(join(f.config, 'goon/mcp.json'), JSON.stringify({ servers: { waiting: {
    transport: 'stdio', command: process.execPath, args: ['-e', "require('node:fs').writeFileSync('mcp-ready', String(process.pid)); process.stdin.resume()"],
  } } }));
  const running = cli(f, 'http://127.0.0.1:1/v1', ['-p', 'never contact model']);
  let pid;
  for (let i = 0; i < 200; i++) {
    try { pid = Number(await readFile(join(f.cwd, 'mcp-ready'), 'utf8')); break; }
    catch { await new Promise(resolve => setTimeout(resolve, 10)); }
  }
  assert(pid, 'MCP child did not start');
  t.after(() => { try { process.kill(pid, 'SIGKILL'); } catch {} });
  running.child.kill('SIGTERM');
  const result = await running.done;
  if (process.platform === 'win32' && result.code === null) {
    assert.equal(result.signal, 'SIGTERM');
    try { process.kill(pid, 'SIGKILL'); } catch {}
  } else {
    assert.equal(result.code, 143, result.stderr);
    assert.throws(() => process.kill(pid, 0), { code: 'ESRCH' });
    assert(!(await readdir(join(f.data, 'goon/sessions'))).some(n => n.endsWith('.lock')));
  }
});

test('MCP stdio handles REA-scale server (207 tools, >2.7MB tools/list payload, >20KB Draft 2020-12 schema, mcpServers config)', async t => {
  const f = await fixture(t);
  let approvals = 0;
  const registry = new ToolRegistry(f.cwd, async () => { approvals++; return false; });
  const config = mcpConfigSchema.parse({
    mcpServers: {
      rea: {
        command: process.execPath,
        args: [fileURLToPath(new URL('./fixtures/mcp-server.mjs', import.meta.url)), '--rea-scale'],
        trust: true,
      },
    },
  });
  const manager = new McpManager(config, f.cwd);
  t.after(() => manager.close());
  await manager.connect(registry);
  assert.equal(manager.tools.length, 207);
  assert.equal(manager.statuses.length, 1);
  assert.equal(manager.statuses[0].status, 'ok');
  assert.equal(manager.statuses[0].toolCount, 207);
  assert(manager.tools.every(tool => tool.readOnly === false), 'trust no longer implies readOnly');
  const firstTool = mcpToolName('rea', 'rea_tool_1');
  const lastTool = mcpToolName('rea', 'rea_tool_207');
  assert(Object.keys(registry.schemas()).includes(firstTool));
  assert(Object.keys(registry.schemas()).includes(lastTool));
  const res = await registry.execute(call(lastTool, { query: 'hello-rea', mode: 'fast' }), signal());
  assert.equal(res.text, 'called:rea_tool_207:hello-rea');
  assert.equal(approvals, 0, 'trust: true auto-approves server tools');
  await manager.close();
});

test('MCP supports Codex config.toml import and enabledTools/disabledTools filtering', async t => {
  const { parseCodexMcpServers } = await import('../dist/mcp.js');
  const toml = `
model = "gpt-5.4"

[mcp_servers.rea]
command = "rea"
args = ["mcp", "serve"]
startup_timeout_sec = 45
tool_timeout_sec = 120
enabled_tools = ["rea_tool_1", "rea_tool_2"]
disabled_tools = ["rea_tool_2"]
default_tools_approval_mode = "approve"

[mcp_servers.rea.env]
REA_MODE = "production"
`;
  const parsed = parseCodexMcpServers(toml);
  assert(parsed.rea);
  assert.equal(parsed.rea.transport, 'stdio');
  assert.equal(parsed.rea.command, 'rea');
  assert.deepEqual(parsed.rea.args, ['mcp', 'serve']);
  assert.equal(parsed.rea.timeoutMs, 45_000);
  assert.equal(parsed.rea.toolTimeoutMs, 120_000);
  assert.equal(parsed.rea.trust, true);
  assert.deepEqual(parsed.rea.env, { REA_MODE: 'production' });

  const f = await fixture(t);
  const registry = new ToolRegistry(f.cwd, async () => true);
  const filteredConfig = mcpConfigSchema.parse({
    mcpServers: {
      rea: {
        command: process.execPath,
        args: [fileURLToPath(new URL('./fixtures/mcp-server.mjs', import.meta.url)), '--rea-scale'],
        enabledTools: ['rea_tool_1', 'rea_tool_2', 'rea_tool_3'],
        disabledTools: ['rea_tool_2'],
      },
    },
  });
  const manager = new McpManager(filteredConfig, f.cwd);
  t.after(() => manager.close());
  await manager.connect(registry);
  assert.deepEqual(manager.tools.map(tl => tl.name).sort(), [
    mcpToolName('rea', 'rea_tool_1'),
    mcpToolName('rea', 'rea_tool_3'),
  ].sort());
  await manager.close();
});

test('MCP annotations may only escalate; readOnlyTools is the only low-risk override', () => {
  const base = stdio().servers.local;
  const risk = (annotation, extra = {}) => classifyMcpRisk('s', 'tool', { ...annotation }, { ...base, ...extra });
  assert.equal(risk({ readOnlyHint: true }).risk, 'medium', 'readOnlyHint is a medium floor');
  assert.equal(risk({ readOnlyHint: true }).net, false);
  assert.equal(risk({ destructiveHint: true }).risk, 'high');
  assert.equal(risk({ openWorldHint: true }).risk, 'high');
  assert.equal(risk({ openWorldHint: true }).net, true);
  assert.equal(risk(undefined).risk, 'medium', 'no annotation stays medium');
  assert.equal(risk(undefined, { trust: true }).risk, 'medium', 'trust does not raise risk');
  assert.equal(risk({ destructiveHint: true }, { trust: true }).risk, 'high', 'destructive wins over trust');
  assert.equal(risk({ openWorldHint: true }, { trust: true }).net, true);
  assert.equal(risk({ readOnlyHint: true }, { readOnlyTools: ['tool'] }).risk, 'low');
  assert.equal(risk({ destructiveHint: true }, { readOnlyTools: ['tool'] }).risk, 'low');
  assert.equal(risk(undefined, { net: true }).net, true, 'server net flag marks the tool');
  const httpConfig = mcpConfigSchema.parse({ servers: { remote: { transport: 'http', url: 'http://127.0.0.1:1/mcp' } } }).servers.remote;
  assert.equal(classifyMcpRisk('remote', 'tool', {}, httpConfig).net, false, 'HTTP transport alone is not net');
});

test('MCP spec keeps risk/net/trusted/annotations and gates openWorld tools before approval', async t => {
  const f = await fixture(t);
  const deniedPolicy = resolveExecutionPolicy({ allowNet: false, allowNetHosts: [] });
  let approvals = 0;
  const registry = new ToolRegistry(f.cwd, async () => { approvals++; return true; });
  registry.policy = deniedPolicy;
  const manager = new McpManager(annotations(), f.cwd, deniedPolicy);
  try {
    await manager.connect(registry);
    assert.equal(manager.tools.length, 5);
    const byName = Object.fromEntries(manager.tools.map(tool => [tool.originalName, tool]));
    assert.equal(byName['plain-tool'].risk, 'medium');
    assert.equal(byName['plain-tool'].net, false);
    assert.equal(byName['plain-tool'].trusted, false);
    assert.deepEqual(byName['readonly-tool'].annotations, { readOnlyHint: true });
    assert.equal(byName['readonly-tool'].risk, 'medium');
    assert.equal(byName['destructive-tool'].risk, 'high');
    assert.equal(byName['open-tool'].risk, 'high');
    assert.equal(byName['open-tool'].net, true);
    assert.equal(byName['destructive-open-tool'].risk, 'high');
    assert.equal(byName['destructive-open-tool'].net, true);
    assert.equal(byName['plain-tool'].readOnly, false, 'trust/annotations never imply readOnly');

    const denied = await registry.execute(call(mcpToolName('ann', 'open-tool')), signal());
    assert.equal(denied.denied, true);
    assert.match(denied.text, /network/i);
    assert.equal(approvals, 0, 'openWorld denial happens before approval');

    const plain = await registry.execute(call(mcpToolName('ann', 'plain-tool')), signal());
    assert.equal(plain.text, 'annotated:plain-tool');
    assert.equal(approvals, 1, 'non-network MCP tools still use approval');
  } finally {
    await manager.close();
  }
});

test('MCP trusted server auto-approves medium/low but never high risk', async t => {
  const f = await fixture(t);
  let approvals = 0;
  const registry = new ToolRegistry(f.cwd, async () => { approvals++; return false; });
  const policy = resolveExecutionPolicy({ allowNet: false, allowNetHosts: [] });
  registry.policy = policy;
  const manager = new McpManager(annotations({ trust: true }), f.cwd, policy);
  try {
    await manager.connect(registry);
    assert.equal(manager.tools.every(tool => tool.trusted), true);

    const readonly = await registry.execute(call(mcpToolName('ann', 'readonly-tool')), signal());
    assert.equal(readonly.text, 'annotated:readonly-tool');
    assert.equal(approvals, 0, 'trust auto-approves medium risk');

    const destructive = await registry.execute(call(mcpToolName('ann', 'destructive-tool')), signal());
    assert.equal(destructive.denied, true);
    assert.equal(approvals, 1, 'destructive annotations escalate to high and still prompt');
  } finally {
    await manager.close();
  }
});

test('MCP server allowNet exempts its tools, global allow-net exempts every server', async t => {
  const f = await fixture(t);
  let approvals = 0;
  const deniedPolicy = resolveExecutionPolicy({ allowNet: false, allowNetHosts: [] });
  const registry = new ToolRegistry(f.cwd, async () => { approvals++; return false; });
  registry.policy = deniedPolicy;
  const blocked = new McpManager(annotations({ readOnlyTools: ['open-tool'] }), f.cwd, deniedPolicy);
  try {
    await blocked.connect(registry);
    assert.equal((await registry.execute(call(mcpToolName('ann', 'open-tool')), signal())).denied, true);
  } finally {
    await blocked.close();
  }

  const serverAllowed = new McpManager(annotations({ readOnlyTools: ['open-tool'], allowNet: true }), f.cwd, deniedPolicy);
  try {
    await serverAllowed.connect(registry);
    const result = await registry.execute(call(mcpToolName('ann', 'open-tool')), signal());
    assert.equal(result.text, 'annotated:open-tool');
    assert.equal(approvals, 0);
  } finally {
    await serverAllowed.close();
  }

  const globalRegistry = new ToolRegistry(f.cwd, async () => { approvals++; return false; });
  globalRegistry.policy = resolveExecutionPolicy({ allowNet: true, allowNetHosts: [] });
  const globallyAllowed = new McpManager(annotations({ readOnlyTools: ['open-tool'] }), f.cwd, globalRegistry.policy);
  try {
    await globallyAllowed.connect(globalRegistry);
    assert.equal((await globalRegistry.execute(call(mcpToolName('ann', 'open-tool')), signal())).text, 'annotated:open-tool');
  } finally {
    await globallyAllowed.close();
  }
});

test('MCP trust does not expose tools in --read-only mode', async t => {
  const f = await fixture(t);
  const registry = new ToolRegistry(f.cwd, async () => true, true);
  const manager = new McpManager(annotations({ trust: true }), f.cwd);
  try {
    await manager.connect(registry);
    assert.equal(manager.tools.length, 5);
    assert(manager.tools.every(tool => tool.readOnly === false), 'trust does not imply readOnly');
    assert(!Object.keys(registry.schemas()).some(name => name.startsWith('mcp_')), 'trust cannot expose tools in read-only');
  } finally {
    await manager.close();
  }
});
