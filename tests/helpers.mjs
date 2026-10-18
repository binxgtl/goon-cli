import { createServer } from 'node:http';
import { mkdtemp, mkdir, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { spawn } from 'node:child_process';

export async function fixture(t) {
  const root = await mkdtemp(join(tmpdir(), 'goon-test-'));
  const cwd = join(root, 'workspace');
  await mkdir(cwd);
  t.after(() => rm(root, { recursive: true, force: true, maxRetries: 5, retryDelay: 50 }));
  return { root, cwd, config: join(root, 'config'), data: join(root, 'data') };
}
export async function provider(t, handler) {
  const requests = [];
  const server = createServer(async (request, response) => {
    try {
      let body = '';
      for await (const chunk of request) body += chunk;
      const parsed = JSON.parse(body);
      requests.push(parsed);
      await handler(parsed, response, requests.length, request);
    } catch (error) {
      response.writeHead(500, { 'content-type': 'application/json' });
      response.end(JSON.stringify({ error: { message: String(error) } }));
    }
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  t.after(() => { server.closeAllConnections(); return new Promise(resolve => server.close(resolve)); });
  return { url: `http://127.0.0.1:${server.address().port}/v1`, requests, server };
}
export function sse(response, { text = '', calls = [], reasoning = '', textChunks, finishReason } = {}) {
  response.writeHead(200, { 'content-type': 'text/event-stream' });
  const chunk = delta => response.write(`data: ${JSON.stringify({ id: 'chatcmpl-test', object: 'chat.completion.chunk', created: 1, model: 'test-model', choices: [{ index: 0, delta, finish_reason: null }] })}\n\n`);
  chunk({ role: 'assistant' });
  if (reasoning) chunk({ reasoning_content: reasoning });
  for (const [index, call] of calls.entries()) {
    const args = JSON.stringify(call.input);
    chunk({ tool_calls: [{ index, id: call.id, type: 'function', function: { name: call.name, arguments: args.slice(0, 3) } }] });
    chunk({ tool_calls: [{ index, function: { arguments: args.slice(3) } }] });
  }
  for (const content of textChunks ?? (text ? [text.slice(0, 2), text.slice(2)] : [])) chunk({ content });
  response.write(`data: ${JSON.stringify({ id: 'chatcmpl-test', object: 'chat.completion.chunk', choices: [{ index: 0, delta: {}, finish_reason: finishReason ?? (calls.length ? 'tool_calls' : 'stop') }], usage: { prompt_tokens: 100, completion_tokens: 10, total_tokens: 110 } })}\n\n`);
  response.end('data: [DONE]\n\n');
}
export function cli(f, url, args = [], options = {}) {
  const child = spawn(process.execPath, [resolve(process.env.GOON_TEST_BIN ?? 'bin/goon.mjs'), '--cwd', f.cwd, '--base-url', url, '--model', 'test-model', ...args], {
    env: { ...process.env, XDG_CONFIG_HOME: f.config, XDG_DATA_HOME: f.data, GOON_API_KEY: 'test-secret-never-echo', ...options.env },
    stdio: ['pipe', 'pipe', 'pipe', 'ipc'],
  });
  const origKill = child.kill.bind(child);
  child.kill = (sig = 'SIGTERM') => {
    if (process.platform === 'win32' && (sig === 'SIGINT' || sig === 'SIGTERM') && child.connected) {
      try {
        child.send({ type: 'goon:signal', signal: sig });
        return true;
      } catch {
        // fall through
      }
    }
    return origKill(sig);
  };
  let stdout = '', stderr = '';
  child.stdout.on('data', chunk => { stdout += chunk; });
  child.stderr.on('data', chunk => { stderr += chunk; });
  const timeout = setTimeout(() => origKill('SIGKILL'), 15_000);
  const done = new Promise((resolve, reject) => {
    child.once('error', reject);
    child.once('close', (code, signal) => { clearTimeout(timeout); resolve({ code, signal, stdout, stderr }); });
  });
  child.stdin.end(options.input ?? '');
  return { child, done };
}
