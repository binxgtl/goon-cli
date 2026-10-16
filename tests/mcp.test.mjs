import test from 'node:test';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { mkdir, writeFile, readdir, readFile } from 'node:fs/promises';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { StreamableHTTPServerTransport } from '@modelcontextprotocol/sdk/server/streamableHttp.js';
import { McpManager, mcpConfigSchema, mcpToolName } from '../dist/mcp.js';
import { ToolRegistry } from '../dist/tools.js';
import { mockMcpServer } from './fixtures/mcp-server.mjs';
import { fixture, cli, provider, sse } from './helpers.mjs';

const stdio = extra => mcpConfigSchema.parse({ servers: { local: { transport: 'stdio', command: process.execPath,
  args: [fileURLToPath(new URL('./fixtures/mcp-server.mjs', import.meta.url)), '--stdio'], ...extra } } });
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
  running.child.kill('SIGTERM');
  const result = await running.done;
  assert.equal(result.code, 143, result.stderr);
  assert.throws(() => process.kill(pid, 0), { code: 'ESRCH' });
  assert(!(await readdir(join(f.data, 'goon/sessions'))).some(n => n.endsWith('.lock')));
});
