import { parseArgs } from 'node:util';
import { createInterface } from 'node:readline/promises';
import { stdin, stdout, stderr } from 'node:process';
import { join, resolve } from 'node:path';
import { loadConfig, resolveRuntime, saveConfig, configPath, dataDir } from './config.js';
import { safeError } from './errors.js';
import type { HarnessEvent } from './harness.js';

const HELP = `goon cli 0.1.0 — independent coding harness

Usage: goon [options] [prompt]
       goon init --provider openai-compatible --base-url URL --model MODEL
       goon doctor | sessions

Options:
  -p, --prompt TEXT          Run one task (or pipe a prompt on stdin)
  -C, --cwd DIRECTORY        Workspace (defaults to current directory)
      --provider NAME        openai-compatible | openai-responses | anthropic
      --base-url URL         Provider API root, usually ending in /v1
      --model NAME           Exact model ID from your provider
      --key-env NAME         Credential variable (default GOON_API_KEY)
      --resume ID|latest     Resume a saved session in this workspace
      --yes                 Approve file changes and shell commands automatically
      --read-only           Expose only read/list/search tools
      --json                Emit NDJSON events instead of terminal output
      --context-window N    Actual model context window (default 128000)
      --max-output-tokens N  Per-request output limit (default 8192)
      --run-token-budget N   Run token budget (default 100000; 0 disables)
      --timeout-ms N        Model request deadline (default 300000)
  -h, --help                Show help
  -v, --version             Show version

Interactive commands: /help, /exit. Ctrl+C cancels the active run.
No Claude/Z.ai account login. API keys are read from environment variables.
Shell commands run as your OS user; approvals are not an OS sandbox.
`;
// Terminal control bytes are data, never commands for the user's terminal.
// oxlint-disable-next-line eslint/no-control-regex
const cleanTerminal = (text: string) => text.replace(/[\u0000-\u0008\u000b-\u001f\u007f-\u009f]/g, '');

export async function main(argv = process.argv.slice(2)): Promise<void> {
  let apiKey: string | undefined;
  let jsonOutput = false;
  try {
    const { values, positionals } = parseArgs({ args: argv, allowPositionals: true, strict: true, options: {
      help: { type: 'boolean', short: 'h' }, version: { type: 'boolean', short: 'v' },
      prompt: { type: 'string', short: 'p' }, cwd: { type: 'string', short: 'C' },
      provider: { type: 'string' }, 'base-url': { type: 'string' }, model: { type: 'string' },
      'key-env': { type: 'string' }, resume: { type: 'string' }, yes: { type: 'boolean' },
      'read-only': { type: 'boolean' }, json: { type: 'boolean' },
      'context-window': { type: 'string' }, 'max-output-tokens': { type: 'string' },
      'run-token-budget': { type: 'string' }, 'timeout-ms': { type: 'string' },
    } });
    jsonOutput = values.json === true;
    if (values.help) { stdout.write(HELP); return; }
    if (values.version) { stdout.write('goon cli 0.1.0\n'); return; }
    const overrides: Record<string, unknown> = {};
    for (const [flag, key] of Object.entries({ provider: 'provider', 'base-url': 'baseUrl', model: 'model', 'key-env': 'keyEnv',
      'context-window': 'contextWindow', 'max-output-tokens': 'maxOutputTokens', 'run-token-budget': 'runTokenBudget', 'timeout-ms': 'timeoutMs' })) {
      const value = values[flag as keyof typeof values];
      if (typeof value === 'string') overrides[key] = ['contextWindow', 'maxOutputTokens', 'runTokenBudget', 'timeoutMs'].includes(key) ? Number(value) : value;
    }
    const config = await loadConfig(overrides);
    if (positionals[0] === 'init') {
      if (!config.model) throw new Error('Specify --model and optionally --base-url/--provider.');
      await saveConfig(config);
      stdout.write(`Saved ${configPath()}\nSet ${config.keyEnv} in your shell, then run goon in your project.\n`);
      return;
    }
    if (positionals[0] === 'doctor') {
      const { runCommand } = await import('./process.js');
      const executable = async (name: string) => {
        try { const r = await runCommand(name, ['--version'], { cwd: resolve(values.cwd ?? '.'), signal: AbortSignal.timeout(5000), timeoutMs: 5000 }); return r.code === 0; }
        catch { return false; }
      };
      const [bash, rg] = await Promise.all([executable('bash'), executable('rg')]);
      stdout.write(JSON.stringify({ version: '0.1.0', node: process.version, platform: process.platform, arch: process.arch,
        config: configPath(), provider: config.provider, model: config.model ?? null, baseUrl: config.baseUrl ?? 'provider default',
        credentialVariable: config.keyEnv, credentialPresent: Boolean(process.env[config.keyEnv]), bash, ripgrep: rg,
        searchFallback: !rg, data: dataDir(), networkTested: false }, null, 2) + '\n');
      if (!bash || !config.model || !process.env[config.keyEnv]) process.exitCode = 1;
      return;
    }
    const { SessionStore } = await import('./session.js');
    if (positionals[0] === 'sessions') {
      const list = await SessionStore.list(join(dataDir(), 'sessions'));
      stdout.write(values.json ? JSON.stringify(list) + '\n' : list.map(s => `${s.id}  ${s.updatedAt}  ${s.model}  ${s.cwd}`).join('\n') + '\n');
      return;
    }
    if (values.prompt && positionals.length) throw new Error('Use either --prompt or positional prompt text.');
    let prompt = values.prompt ?? (positionals.length ? positionals.join(' ') : undefined);
    if (!prompt && !stdin.isTTY) {
      let bytes = 0; const chunks: Buffer[] = [];
      for await (const chunk of stdin) {
        const buffer = Buffer.from(chunk); bytes += buffer.length;
        if (bytes > 1024 * 1024) throw new Error('Piped prompt exceeds 1 MiB.');
        chunks.push(buffer);
      }
      prompt = Buffer.concat(chunks).toString('utf8').trim();
      if (!prompt) throw new Error('Empty input. Supply --prompt or run in an interactive terminal.');
    }
    const runtime = await resolveRuntime(config, values.cwd ?? process.cwd());
    apiKey = runtime.apiKey;
    const store = await SessionStore.open(join(runtime.dataDir, 'sessions'), runtime, values.resume);
    let rl: ReturnType<typeof createInterface> | undefined;
    let controller: AbortController | undefined;
    let denied = false, exiting = false, terminating = false;
    const interactive = !prompt;
    if (stdin.isTTY) rl = createInterface({ input: stdin, output: stderr, terminal: true });
    const emit = (event: HarnessEvent) => {
      if (event.type === 'stopped') denied = true;
      if (jsonOutput) { stdout.write(JSON.stringify(event) + '\n'); return; }
      if (event.type === 'text') stdout.write(cleanTerminal(event.text));
      else if (event.type === 'tool' && event.state === 'start') stderr.write(`\n[${event.name}]\n`);
      else if (event.type === 'compaction') stderr.write('\n[Context compacted; original history archived.]\n');
      else if (event.type === 'stopped') stderr.write(`\n${event.reason}\n`);
    };
    const { ToolRegistry } = await import('./tools.js');
    const tools = new ToolRegistry(runtime.cwd, async request => {
      if (values.yes) return true;
      if (!rl) return false;
      const preview = cleanTerminal(JSON.stringify(request.input, null, 2).split(runtime.apiKey).join('[REDACTED]').slice(0, 1600));
      const reply = await rl.question(`\nAllow ${request.tool}?\n${preview}\n[y/N] `, { signal: request.signal });
      return /^(y|yes)$/i.test(reply.trim());
    }, values['read-only'] === true);
    const { Harness } = await import('./harness.js');
    const harness = new Harness(runtime, store, tools, emit);
    const stop = () => {
      if (controller && !controller.signal.aborted) controller.abort(new Error('Cancelled by user.'));
      else { exiting = true; rl?.close(); }
    };
    const terminate = () => {
      terminating = true; exiting = true;
      controller?.abort(new Error('Terminated.'));
      rl?.close(); process.exitCode = 143;
    };
    process.on('SIGINT', stop);
    process.on('SIGTERM', terminate);
    rl?.on('SIGINT', stop);
    if (jsonOutput) stdout.write(JSON.stringify({ type: 'session', id: store.session.id, cwd: runtime.cwd }) + '\n');
    else stderr.write(`goon · ${runtime.model} · session ${store.session.id}\n`);
    const run = async (input: string) => {
      if (Buffer.byteLength(input) > 1024 * 1024) throw new Error('Prompt exceeds 1 MiB.');
      denied = false;
      controller = new AbortController();
      try {
        const text = await harness.run(input, controller.signal);
        if (jsonOutput) stdout.write(JSON.stringify({ type: 'done', text, denied, sessionId: store.session.id }) + '\n');
        else stdout.write('\n');
        if (denied && !interactive) process.exitCode = 2;
      } catch (error) {
        if (!interactive) process.exitCode = controller.signal.aborted ? terminating ? 143 : 130 : 1;
        const message = safeError(error, apiKey);
        if (jsonOutput) stdout.write(JSON.stringify({ type: 'error', message }) + '\n');
        else stderr.write(`\n${cleanTerminal(message)}\n`);
      } finally { controller = undefined; }
    };
    try {
      if (prompt) await run(prompt);
      else {
        if (!rl) throw new Error('Interactive mode requires a TTY.');
        stderr.write('Type /help for commands.\n');
        while (!exiting) {
          let input: string;
          try { input = (await rl.question('\ngoon> ')).trim(); }
          catch { break; }
          if (!input) continue;
          if (['/exit', '/quit'].includes(input)) break;
          if (input === '/help') { stderr.write('Enter a task. /exit quits. Ctrl+C cancels a run.\n'); continue; }
          await run(input);
        }
      }
    } finally {
      process.removeListener('SIGINT', stop); process.removeListener('SIGTERM', terminate);
      rl?.close(); await store.close();
    }
  } catch (error) {
    const message = cleanTerminal(safeError(error, apiKey));
    if (jsonOutput) stdout.write(JSON.stringify({ type: 'error', message }) + '\n');
    else stderr.write(`goon: ${message}\n`);
    process.exitCode = 1;
  }
}
