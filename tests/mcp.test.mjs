import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdir, writeFile, readdir, readFile } from 'node:fs/promises';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { fixture, cli, provider, sse } from './helpers.mjs';

const stdio = extra => ({ servers: { local: { transport: 'stdio', command: process.execPath,
  args: [fileURLToPath(new URL('./fixtures/mcp-server.mjs', import.meta.url)), '--stdio'], ...extra } } });
test('real CLI dispatches MCP tools and startup failure releases session locks', async t => {
  const f = await fixture(t);
  await mkdir(join(f.config, 'goon'), { recursive: true });
  const path = join(f.config, 'goon/mcp.json');
  await writeFile(path, JSON.stringify(stdio({ readOnlyTools: ['echo'] })));
  const p = await provider(t, (body, response, n) => {
    if (body.messages.some(m => m.content === 'bypass broken config')) return sse(response, { text: 'Bypassed.' });
    const echo = body.tools.map(t => t.function.name).find(name => /^mcp_local_echo/.test(name));
    if (n === 1) { assert(echo, 'the MCP echo tool is offered'); return sse(response, { calls: [{ id: 'echo1', name: echo, input: { text: 'CLI MCP' } }] }); }
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
