import { parseArgs } from 'node:util';
import { createInterface } from 'node:readline/promises';
import { stdin, stdout, stderr } from 'node:process';
import { join, resolve } from 'node:path';
import { loadConfig, resolveRuntime, saveConfig, configPath, dataDir } from './config.js';
import { safeError } from './errors.js';
import { VERSION } from './version.js';
import type { HarnessEvent } from './harness.js';
import type { SubagentEvent } from './subagents.js';
import { terminalText as cleanTerminal, TerminalUI } from './tui.js';
import type { McpManager } from './mcp.js';

const HELP = `goon cli ${VERSION} — independent coding harness

Usage: goon [options] [prompt]
       goon init --provider openai-compatible --base-url URL --model MODEL
       goon doctor | sessions
       goon mcp list | check

Options:
  -p, --prompt TEXT          Run one task (or pipe a prompt on stdin)
  -C, --cwd DIRECTORY        Workspace (defaults to current directory)
      --provider NAME        openai-compatible | openai-responses | anthropic
      --base-url URL         Provider API root, usually ending in /v1
      --model NAME           Exact model ID from your provider
      --key-env NAME         Credential variable (default GOON_API_KEY)
      --resume ID|latest     Resume a saved session in this workspace
      --yes                 Approve file changes and shell commands automatically
      --read-only           Read tools and research agents; no writes or shell
      --json                Emit NDJSON events instead of terminal output
      --no-tui              Use plain readline instead of the full-screen UI
      --no-mcp              Disable configured MCP servers for this run
      --no-subagents        Disable delegation tools for this run
      --inline-reasoning MODE leading-think (default) | off
      --show-reasoning      Separate reasoning events / labeled stderr output
      --context-window N    Actual model context window (default 128000)
      --max-output-tokens N  Per-request output limit (default 8192)
      --run-token-budget N   Run token budget (default 100000; 0 disables)
      --timeout-ms N        Model request deadline (default 300000)
  -h, --help                Show help
  -v, --version             Show version

Interactive commands: /help, /status, /tools, /exit. Ctrl+C cancels the active run.
No Claude/Z.ai account login. API keys are read from environment variables.
Shell commands run as your OS user; approvals are not an OS sandbox.
`;
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
      'no-tui': { type: 'boolean' }, 'no-mcp': { type: 'boolean' }, 'no-subagents': { type: 'boolean' },
      'inline-reasoning': { type: 'string' }, 'show-reasoning': { type: 'boolean' },
      'context-window': { type: 'string' }, 'max-output-tokens': { type: 'string' },
      'run-token-budget': { type: 'string' }, 'timeout-ms': { type: 'string' },
    } });
    jsonOutput = values.json === true;
    if (values.help) { stdout.write(HELP); return; }
    if (values.version) { stdout.write(`goon cli ${VERSION}\n`); return; }
    const overrides: Record<string, unknown> = {};
    for (const [flag, key] of Object.entries({ provider: 'provider', 'base-url': 'baseUrl', model: 'model', 'key-env': 'keyEnv',
      'context-window': 'contextWindow', 'max-output-tokens': 'maxOutputTokens', 'run-token-budget': 'runTokenBudget', 'timeout-ms': 'timeoutMs', 'inline-reasoning': 'inlineReasoning' })) {
      const value = values[flag as keyof typeof values];
      if (typeof value === 'string') overrides[key] = ['contextWindow', 'maxOutputTokens', 'runTokenBudget', 'timeoutMs'].includes(key) ? Number(value) : value;
    }
    if (values['show-reasoning'] !== undefined) overrides.showReasoning = values['show-reasoning'];
    const config = await loadConfig(overrides);
    if (positionals[0] === 'mcp') {
      const { loadMcpConfig, mcpConfigPath, McpManager } = await import('./mcp.js');
      const mcpConfig = await loadMcpConfig();
      if (positionals[1] === 'list') {
        stdout.write(JSON.stringify({ path: mcpConfigPath(), servers: Object.entries(mcpConfig.servers).map(([name, server]) =>
          ({ name, transport: server.transport, disabled: server.disabled, readOnlyTools: server.readOnlyTools })) }, null, 2) + '\n');
        return;
      }
      if (positionals[1] !== 'check') throw new Error('Usage: goon mcp list | check');
      const manager = new McpManager(mcpConfig, resolve(values.cwd ?? '.'));
      const controller = new AbortController();
      const cancel = (signal: NodeJS.Signals) => { process.exitCode = signal === 'SIGTERM' ? 143 : 130; controller.abort(); };
      process.on('SIGINT', cancel); process.on('SIGTERM', cancel);
      try { stdout.write(JSON.stringify(await manager.connect(undefined, controller.signal), null, 2) + '\n'); }
      finally { process.removeListener('SIGINT', cancel); process.removeListener('SIGTERM', cancel); await manager.close(); }
      return;
    }
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
      stdout.write(JSON.stringify({ version: VERSION, node: process.version, platform: process.platform, arch: process.arch,
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
    let tui: TerminalUI | undefined;
    let manager: McpManager | undefined;
    let controller: AbortController | undefined;
    let denied = false, exiting = false, terminating = false, starting = true;
    const interactive = !prompt;
    const stop = () => {
      if (starting) process.exitCode = 130;
      if (controller && !controller.signal.aborted) controller.abort(new Error('Cancelled by user.'));
      else { exiting = true; rl?.close(); tui?.close(); }
    };
    const terminate = () => {
      terminating = true; exiting = true;
      controller?.abort(new Error('Terminated.'));
      rl?.close(); tui?.close(); process.exitCode = 143;
    };
    process.on('SIGINT', stop); process.on('SIGTERM', terminate);
    try {
      if (interactive && stdin.isTTY && stdout.isTTY && !jsonOutput && !values['no-tui'] && process.env.TERM !== 'dumb') {
        tui = new TerminalUI(`${runtime.model} · ${store.session.id.slice(0, 8)}`, stop);
      } else if (stdin.isTTY) rl = createInterface({ input: stdin, output: stderr, terminal: true });
      rl?.on('SIGINT', stop);
      const display = (text: string) => tui ? tui.write(text) : stderr.write(cleanTerminal(text));
      const question = (label: string, signal?: AbortSignal, remember = false) => tui ? tui.question(label, signal, remember)
        : rl ? rl.question(label, { signal }) : Promise.reject(new Error('Input requires a TTY.'));
      let reasoningSource: 'inline' | 'provider' | undefined;
      const emit = (event: HarnessEvent | SubagentEvent) => {
        if (event.type === 'stopped') denied = true;
        if (jsonOutput) { stdout.write(JSON.stringify(event) + '\n'); return; }
        if (tui) { tui.event(event); return; }
        if (event.type === 'reasoning') {
          if (reasoningSource !== event.source) stderr.write(`\n[reasoning · ${event.source}]\n`);
          stderr.write(cleanTerminal(event.text)); reasoningSource = event.source; return;
        }
        if (reasoningSource) { stderr.write('\n'); reasoningSource = undefined; }
        if (event.type === 'text') stdout.write(cleanTerminal(event.text));
        else if (event.type === 'subagent' && event.state !== 'progress') stderr.write(`\n[agent ${event.id.slice(0, 8)}: ${event.state}]\n`);
        else if (event.type === 'tool' && event.state === 'start') stderr.write(`\n[${event.name}]\n`);
        else if (event.type === 'compaction') stderr.write('\n[Context compacted; original history archived.]\n');
        else if (event.type === 'stopped') stderr.write(`\n${event.reason}\n`);
      };
      const { ToolRegistry } = await import('./tools.js');
      const tools = new ToolRegistry(runtime.cwd, async request => {
        if (values.yes) return true;
        if (!rl && !tui) return false;
        const preview = cleanTerminal(JSON.stringify(request.input, null, 2).split(runtime.apiKey).join('[REDACTED]').slice(0, 1600));
        display(`\nAllow ${request.tool}?\n${preview}\n`);
        const reply = await question('[y/N] ', request.signal);
        return /^(y|yes)$/i.test(reply.trim());
      }, values['read-only'] === true);
      if (!values['no-mcp']) {
        const { loadMcpConfig, McpManager } = await import('./mcp.js');
        controller = new AbortController();
        tui?.setStatus('Connecting MCP');
        manager = new McpManager(await loadMcpConfig(), runtime.cwd);
        await manager.connect(tools, controller.signal);
        controller = undefined;
      }
      if (!values['no-subagents']) {
        const { Subagents } = await import('./subagents.js');
        new Subagents(runtime, store, tools, emit).register();
      }
      const { Harness } = await import('./harness.js');
      const harness = new Harness(runtime, store, tools, emit);
      starting = false;
      if (jsonOutput) stdout.write(JSON.stringify({ type: 'session', id: store.session.id, cwd: runtime.cwd }) + '\n');
      else display(`goon · ${runtime.model} · session ${store.session.id}\n`);
      const run = async (input: string) => {
        if (Buffer.byteLength(input) > 1024 * 1024) throw new Error('Prompt exceeds 1 MiB.');
        denied = false;
        controller = new AbortController();
        tui?.setStatus('Running');
        if (tui) tui.write(`\n> ${input}\n`);
        try {
          const text = await harness.run(input, controller.signal);
          if (jsonOutput) stdout.write(JSON.stringify({ type: 'done', text, denied, sessionId: store.session.id }) + '\n');
          else if (tui) tui.write('\n'); else stdout.write('\n');
          if (denied && !interactive) process.exitCode = 2;
        } catch (error) {
          if (!interactive) process.exitCode = controller.signal.aborted ? terminating ? 143 : 130 : 1;
          const message = safeError(error, apiKey);
          if (jsonOutput) stdout.write(JSON.stringify({ type: 'error', message }) + '\n');
          else display(`\n${message}\n`);
        } finally { controller = undefined; tui?.setStatus('Ready'); }
      };
      if (prompt) await run(prompt);
      else {
        if (!rl && !tui) throw new Error('Interactive mode requires a TTY.');
        display('Type /help for commands.\n');
        tui?.setStatus('Ready');
        while (!exiting) {
          let input: string;
          try { input = (await question(tui ? 'goon> ' : '\ngoon> ', undefined, true)).trim(); }
          catch { break; }
          if (!input) continue;
          if (['/exit', '/quit'].includes(input)) break;
          if (input === '/help') { display('Enter a task. /tools lists tools. /status shows session. /exit quits. Ctrl+C cancels; PgUp/PgDn scroll the TUI.\n'); continue; }
          if (input === '/tools') { display(Object.keys(tools.schemas()).join('\n') + '\n'); continue; }
          if (input === '/status') { display(`Session ${store.session.id}\nModel ${runtime.model}\nWorkspace ${runtime.cwd}\nSession tokens ${store.session.tokens}\nMCP tools ${manager?.tools.length ?? 0}\n`); continue; }
          await run(input);
        }
      }
    } finally {
      process.removeListener('SIGINT', stop); process.removeListener('SIGTERM', terminate);
      try { tui?.close(); rl?.close(); await manager?.close(); }
      finally { await store.close(); }
    }
  } catch (error) {
    const message = cleanTerminal(safeError(error, apiKey));
    if (jsonOutput) stdout.write(JSON.stringify({ type: 'error', message }) + '\n');
    else stderr.write(`goon: ${message}\n`);
    if (process.exitCode !== 130 && process.exitCode !== 143) process.exitCode = 1;
  }
}
