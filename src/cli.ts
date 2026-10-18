import { parseArgs } from 'node:util';
import { createInterface } from 'node:readline/promises';
import { stdin, stdout, stderr } from 'node:process';
import { join, resolve } from 'node:path';
import {
  EFFORT_LEVELS,
  type Config,
  type EffortLevel,
  configPath,
  dataDir,
  loadConfig,
  resolveApiKey,
  resolveRuntime,
  saveConfig,
} from './config.js';
import { safeError } from './errors.js';
import { VERSION } from './version.js';
import type { HarnessEvent } from './harness.js';
import type { SubagentEvent } from './subagents.js';
import { terminalText as cleanTerminal, TerminalUI } from './tui.js';
import type { McpManager } from './mcp.js';
import { EFFORT_DESCRIPTIONS, parseEffortInput, runSetupWizard } from './setup.js';
import { discoverModels } from './provider.js';

const HELP = `goon cli ${VERSION} — independent coding harness

Usage: goon [options] [prompt]
       goon setup
       goon init --provider openai-compatible --base-url URL --model MODEL [--effort LEVEL]
       goon doctor | sessions
       goon mcp list | check

Options:
  -p, --prompt TEXT          Run one task (or pipe a prompt on stdin)
  -C, --cwd DIRECTORY        Workspace (defaults to current directory)
      --provider NAME        openai-compatible | openai-responses | anthropic
      --base-url URL         Provider API root, usually ending in /v1
      --model NAME           Exact model ID from your provider
      --effort LEVEL         Reasoning effort: low | medium | high | xhigh | max
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

Interactive commands:
  /help, /setup, /model, /effort, /mode, /plan, /revert, /undo, /checkpoints,
  /todo, /compact, /diff, /review, /fork, /rename, /export, /init, /status,
  /tools, /sessions, /doctor, /clear, /exit, or !<cmd> for shell.
Shortcuts: Shift+Tab mode, Alt+. / Alt+, effort, Tab autocomplete (@file & /cmd), Ctrl+C cancel.
API keys are read from environment variables or ~/.config/goon/credentials.json.
Shell commands run as your OS user; approvals are not an OS sandbox.
`;

type PermissionMode = 'ask' | 'plan' | 'auto' | 'readonly';

function welcomeBanner(
  model: string,
  provider: string,
  baseUrl: string,
  effort: EffortLevel | undefined,
  cwd: string,
  sessionId: string,
  mode: PermissionMode,
): string {
  const row = (label: string, val: string) => `│  ${label.padEnd(10)}: ${val.slice(0, 52).padEnd(52)} │`;
  return [
    `╭─── ✻ goon cli v${VERSION} ──────────────────────────────────────────────╮`,
    row('Model', `${model} (${provider})`),
    row('Effort', `${effort ?? 'default'} (low | medium | high | xhigh | max)`),
    row('Endpoint', baseUrl),
    row('Workspace', cwd),
    row('Session', sessionId.slice(0, 8)),
    row('Mode', `${mode} (Shift+Tab or /mode | /plan)`),
    `│                                                                   │`,
    row('Commands', '/help · /plan · /revert · /model · /effort · /compact'),
    `╰───────────────────────────────────────────────────────────────────╯`,
    '',
  ].join('\n');
}

export async function main(argv = process.argv.slice(2)): Promise<void> {
  let apiKey: string | undefined;
  let jsonOutput = false;
  try {
    const { values, positionals } = parseArgs({ args: argv, allowPositionals: true, strict: true, options: {
      help: { type: 'boolean', short: 'h' }, version: { type: 'boolean', short: 'v' },
      prompt: { type: 'string', short: 'p' }, cwd: { type: 'string', short: 'C' },
      provider: { type: 'string' }, 'base-url': { type: 'string' }, model: { type: 'string' },
      effort: { type: 'string' },
      'key-env': { type: 'string' }, resume: { type: 'string' }, yes: { type: 'boolean' },
      plan: { type: 'boolean' },
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
    for (const [flag, key] of Object.entries({ provider: 'provider', 'base-url': 'baseUrl', model: 'model', effort: 'effort', 'key-env': 'keyEnv',
      'context-window': 'contextWindow', 'max-output-tokens': 'maxOutputTokens', 'run-token-budget': 'runTokenBudget', 'timeout-ms': 'timeoutMs', 'inline-reasoning': 'inlineReasoning' })) {
      const value = values[flag as keyof typeof values];
      if (typeof value === 'string') overrides[key] = ['contextWindow', 'maxOutputTokens', 'runTokenBudget', 'timeoutMs'].includes(key) ? Number(value) : value;
    }
    if (values['show-reasoning'] !== undefined) overrides.showReasoning = values['show-reasoning'];
    let config: Config = await loadConfig(overrides);
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
    if (positionals[0] === 'setup') {
      await runSetupWizard(config);
      return;
    }
    if (positionals[0] === 'init') {
      if (!config.model && stdin.isTTY && !jsonOutput) {
        await runSetupWizard(config);
        return;
      }
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
      const [bash, rg, resolvedKey] = await Promise.all([executable('bash'), executable('rg'), resolveApiKey(config)]);
      stdout.write(JSON.stringify({ version: VERSION, node: process.version, platform: process.platform, arch: process.arch,
        config: configPath(), provider: config.provider, model: config.model ?? null, effort: config.effort ?? 'default',
        baseUrl: config.baseUrl ?? 'provider default', credentialVariable: config.keyEnv, credentialPresent: Boolean(resolvedKey),
        bash, ripgrep: rg, searchFallback: !rg, data: dataDir(), networkTested: false }, null, 2) + '\n');
      if (!bash || !config.model || !resolvedKey) process.exitCode = 1;
      return;
    }
    const { SessionStore } = await import('./session.js');
    if (positionals[0] === 'sessions') {
      const list = await SessionStore.list(join(dataDir(), 'sessions'));
      stdout.write(values.json ? JSON.stringify(list) + '\n' : list.map(s => `${s.id}  ${s.updatedAt}  ${s.model}${s.title ? `  [${s.title}]` : ''}  ${s.cwd}`).join('\n') + '\n');
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
    // Auto-launch interactive setup wizard on first interactive run if unconfigured
    if (!prompt && stdin.isTTY && !jsonOutput && (!config.model || !(await resolveApiKey(config)))) {
      config = await runSetupWizard(config);
    }
    let runtime = await resolveRuntime(config, values.cwd ?? process.cwd());
    apiKey = runtime.apiKey;
    const store = await SessionStore.open(join(runtime.dataDir, 'sessions'), runtime, values.resume);
    let rl: ReturnType<typeof createInterface> | undefined;
    let tui: TerminalUI | undefined;
    let manager: McpManager | undefined;
    let controller: AbortController | undefined;
    let denied = false, exiting = false, terminating = false, starting = true;
    let mode: PermissionMode = values['read-only'] ? 'readonly' : values.plan ? 'plan' : values.yes ? 'auto' : 'ask';
    let prePlanMode: PermissionMode = values.yes ? 'auto' : 'ask';

    const sessionAllowedTools = new Set<string>();
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
        const titleLabel = store.session.title ? `${runtime.model} · ${store.session.title}` : `${runtime.model} · ${store.session.id.slice(0, 8)}`;
        tui = new TerminalUI(titleLabel, stop);
        tui.setMode(mode);
        tui.setEffort(runtime.effort);
      } else if (stdin.isTTY) rl = createInterface({ input: stdin, output: stderr, terminal: true });
      rl?.on('SIGINT', stop);
      const display = (text: string) => tui ? tui.write(text) : stderr.write(cleanTerminal(text));
      const question = (label: string, signal?: AbortSignal, remember = false) => tui ? tui.question(label, signal, remember)
        : rl ? rl.question(label, { signal }) : Promise.reject(new Error('Input requires a TTY.'));
      let reasoningSource: 'inline' | 'provider' | undefined;
      const emit = (event: HarnessEvent | SubagentEvent) => {
        if (event.type === 'stopped') denied = true;
        if (event.type === 'plan_exit') {
          mode = prePlanMode;
          tui?.setMode(mode);
        }
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
        else if (event.type === 'todo') stderr.write(`\n[todo: ${event.todos.filter(t => t.status === 'completed').length}/${event.todos.length} completed]\n`);
        else if (event.type === 'plan_exit') stderr.write('\n[Plan approved — exited plan mode]\n');
        else if (event.type === 'compaction') stderr.write('\n[Context compacted; original history archived.]\n');
        else if (event.type === 'stopped') stderr.write(`\n${event.reason}\n`);
      };
      const { ToolRegistry, formatTodos } = await import('./tools.js');
      const { Subagents } = await import('./subagents.js');
      const { Harness } = await import('./harness.js');

      const buildHarness = async (currentMode: PermissionMode) => {
        const reg = new ToolRegistry(runtime.cwd, async request => {
          if (currentMode === 'auto' || sessionAllowedTools.has(request.tool)) return true;
          if (!rl && !tui) return false;
          if (request.tool === 'exit_plan_mode' && request.input && typeof request.input === 'object' && 'plan' in request.input) {
            const planStr = String((request.input as { plan: unknown }).plan);
            display(`\n╭─── Proposed Implementation Plan ────────────────────────────────╮\n${planStr}\n╰─────────────────────────────────────────────────────────────────╯\nApprove plan and exit plan mode?\n`);
            const reply = (await question('[y/N] ', request.signal)).trim().toLowerCase();
            return reply === 'y' || reply === 'yes';
          }
          const diff = await reg.files.previewDiff(request.tool, request.input);
          const rawPreview = diff ?? JSON.stringify(request.input, null, 2);
          const preview = cleanTerminal(rawPreview.split(runtime.apiKey).join('[REDACTED]').slice(0, 2000));
          display(`\nAllow ${request.tool}?\n${preview}\n`);
          const reply = (await question('[y/N/a(lways)] ', request.signal)).trim().toLowerCase();
          if (reply === 'a' || reply === 'always') {
            sessionAllowedTools.add(request.tool);
            return true;
          }
          return reply === 'y' || reply === 'yes';
        }, currentMode === 'readonly');
        reg.planMode = currentMode === 'plan';
        if (!values['no-mcp'] && manager) {
          await manager.close();
          manager = undefined;
        }
        if (!values['no-mcp']) {
          const { loadMcpConfig, McpManager } = await import('./mcp.js');
          controller = new AbortController();
          tui?.setStatus('Connecting MCP');
          manager = new McpManager(await loadMcpConfig(), runtime.cwd);
          await manager.connect(reg, controller.signal);
          controller = undefined;
        }
        if (!values['no-subagents']) {
          new Subagents(runtime, store, reg, emit).register();
        }
        return { tools: reg, harness: new Harness(runtime, store, reg, emit) };
      };

      let { tools, harness } = await buildHarness(mode);
      if (tui) {
        void tools.files.listWorkspaceFiles().then(files => tui?.setWorkspaceFiles(files)).catch(() => {});
        tui.onToggleMode = () => {
          const order: PermissionMode[] = ['ask', 'plan', 'auto', 'readonly'];
          const next = order[(order.indexOf(mode) + 1) % order.length];
          if (next === 'plan' && mode !== 'plan') prePlanMode = mode;
          mode = next;
          tools.readOnly = mode === 'readonly';
          tools.planMode = mode === 'plan';
          tui?.setMode(mode);
          display(`\n[Mode switched to: ${mode}]\n`);
        };
        tui.onCycleEffort = direction => {
          const ring: (EffortLevel | undefined)[] = [undefined, ...EFFORT_LEVELS];
          const curIdx = ring.indexOf(runtime.effort);
          const nextEffort = ring[(curIdx + direction + ring.length) % ring.length];
          void (async () => {
            config = await loadConfig({ ...config, effort: nextEffort });
            await saveConfig(config);
            runtime = await resolveRuntime(config, runtime.cwd);
            tui?.setEffort(runtime.effort);
            const built = await buildHarness(mode);
            tools = built.tools;
            harness = built.harness;
            display(`\n[Reasoning effort set to: ${runtime.effort ?? 'default'}]\n`);
          })();
        };
      }
      starting = false;
      if (jsonOutput) stdout.write(JSON.stringify({ type: 'session', id: store.session.id, cwd: runtime.cwd }) + '\n');
      else if (interactive) display(welcomeBanner(runtime.model, runtime.provider, runtime.baseUrl, runtime.effort, runtime.cwd, store.session.id, mode));
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
        } finally {
          controller = undefined;
          tui?.setStatus('Ready');
          if (tui) void tools.files.listWorkspaceFiles().then(files => tui?.setWorkspaceFiles(files)).catch(() => {});
        }
      };
      if (prompt) await run(prompt);
      else {
        if (!rl && !tui) throw new Error('Interactive mode requires a TTY.');
        display('Type /help for commands, @file to attach files, !<cmd> for shell, or enter a task.\n');
        tui?.setStatus('Ready');
        while (!exiting) {
          let input: string;
          try { input = (await question(tui ? 'goon> ' : '\ngoon> ', undefined, true)).trim(); }
          catch { break; }
          if (!input) continue;
          if (['/exit', '/quit'].includes(input)) break;
          if (input === '/clear') {
            tui?.clear();
            display(welcomeBanner(runtime.model, runtime.provider, runtime.baseUrl, runtime.effort, runtime.cwd, store.session.id, mode));
            continue;
          }
          if (input === '/help') {
            display(
              '\nCommands:\n' +
              '  /setup, /config                   Interactive provider, URL, key, model & effort setup\n' +
              '  /model [id] [--effort=<level>]    Discover/pick model from /models or set model ID\n' +
              '  /effort [low|medium|high|xhigh|max|default] Switch reasoning effort (or Alt+./Alt+,)\n' +
              '  /mode [ask|plan|auto|readonly]    Switch mode (or press Shift+Tab)\n' +
              '  /plan [off|execute|<prompt>]      Enter/exit Plan Mode or execute approved plan\n' +
              '  /revert, /undo [code|msg|both] [N] Revert last N turns (both messages AND modified files!)\n' +
              '  /checkpoints                      List turn checkpoints & modified files in this session\n' +
              '  /todo [clear]                     View or clear the current task checklist\n' +
              '  /compact [focus instructions]     Summarize & archive earlier context history\n' +
              '  /diff                             Show git status and unified diff in workspace\n' +
              '  /review [instructions]            Run an agentic code review on current changes\n' +
              '  /fork [title]                     Clone current session into a new forked session ID\n' +
              '  /rename <title>                   Set a title for the current session\n' +
              '  /export [filename.md]             Export session transcript to a Markdown file\n' +
              '  /init                             Generate or update AGENTS.md for this project\n' +
              '  /status                           Show session, model, effort & context window stats\n' +
              '  /tools                            List active tools in current mode\n' +
              '  /sessions                         List saved workspace sessions\n' +
              '  /doctor                           Check environment health\n' +
              '  /clear                            Clear screen (or press Ctrl+L)\n' +
              '  /exit                             Exit goon (or press Ctrl+D on empty input)\n' +
              '  !<command>                        Execute shell command directly & record in context\n' +
              '  @<filepath>                       Attach workspace file directly in prompt (Tab completes)\n'
            );
            continue;
          }

          if (input === '/setup' || input === '/config') {
            config = await runSetupWizard(config, {
              write: text => display(text),
              ask: label => question(label),
            });
            runtime = await resolveRuntime(config, runtime.cwd);
            apiKey = runtime.apiKey;
            store.session.model = runtime.model;
            tui?.setTitle(`${runtime.model} · ${store.session.id.slice(0, 8)}`);
            tui?.setEffort(runtime.effort);
            const built = await buildHarness(mode);
            tools = built.tools;
            harness = built.harness;
            continue;
          }
          if (input === '/model' || input.startsWith('/model ')) {
            const rawArgs = input.slice(6).trim();
            let nextModel: string | undefined;
            let nextEffort = config.effort;
            let effortChanged = false;
            if (rawArgs) {
              const tokens = rawArgs.split(/\s+/);
              for (const tok of tokens) {
                if (tok.startsWith('--effort=') || tok.startsWith('--reasoning=')) {
                  const val = tok.split('=')[1] ?? '';
                  nextEffort = parseEffortInput(val, undefined);
                  effortChanged = true;
                } else if (!tok.startsWith('-')) {
                  nextModel = tok;
                }
              }
            } else {
              display(`\nCurrent model: ${runtime.model} (effort: ${runtime.effort ?? 'default'} · ${runtime.provider} · ${runtime.baseUrl})\nProbing ${runtime.baseUrl}/models ...\n`);
              const discovered = await discoverModels(runtime.baseUrl, runtime.apiKey, runtime.provider);
              if (discovered.length > 0) {
                const shown = discovered.slice(0, 30);
                for (const [idx, m] of shown.entries()) {
                  const meta = m.contextWindow ? ` (ctx: ${Math.round(m.contextWindow / 1024)}k)` : '';
                  display(`  [${idx + 1}] ${m.id}${meta}\n`);
                }
                const pick = (await question(`Select [1-${shown.length}] or enter model ID (Enter to keep ${runtime.model}): `)).trim();
                const pickIdx = Number(pick) - 1;
                if (pick && Number.isInteger(pickIdx) && pickIdx >= 0 && pickIdx < shown.length) {
                  nextModel = shown[pickIdx].id;
                } else if (pick) {
                  nextModel = pick;
                }
              } else {
                const typed = (await question(`Enter model ID (Enter to keep ${runtime.model}): `)).trim();
                if (typed) nextModel = typed;
              }
            }
            if (!nextModel && !effortChanged) continue;
            config = await loadConfig({
              ...config,
              ...(nextModel ? { model: nextModel } : {}),
              ...(effortChanged ? { effort: nextEffort } : {}),
            });
            await saveConfig(config);
            runtime = await resolveRuntime(config, runtime.cwd);
            store.session.model = runtime.model;
            tui?.setTitle(`${runtime.model} · ${store.session.id.slice(0, 8)}`);
            tui?.setEffort(runtime.effort);
            const built = await buildHarness(mode);
            tools = built.tools;
            harness = built.harness;
            display(`Active model: ${runtime.model} (effort: ${runtime.effort ?? 'default'})\n`);
            continue;
          }
          if (input === '/effort' || input.startsWith('/effort ')) {
            let arg = input.slice(7).trim();
            if (!arg) {
              display(`\nReasoning Effort (current: ${runtime.effort ?? 'default'}):\n`);
              display(`  ${!runtime.effort ? '●' : ' '} [1] default  — Use provider/model default\n`);
              for (const [idx, level] of EFFORT_LEVELS.entries()) {
                display(`  ${runtime.effort === level ? '●' : ' '} [${idx + 2}] ${level.padEnd(8)} — ${EFFORT_DESCRIPTIONS[level]}\n`);
              }
              arg = (await question(`Select [1-${EFFORT_LEVELS.length + 1}] or effort name: `)).trim();
              if (!arg) continue;
            }
            const clean = arg.toLowerCase();
            const isClear = ['default', 'off', 'none', '0', '1'].includes(clean);
            const parsed = parseEffortInput(arg, undefined);
            if (!isClear && !parsed) {
              display(`Invalid effort "${arg}". Choose from: low, medium, high, xhigh, max, default.\n`);
              continue;
            }
            config = await loadConfig({ ...config, effort: isClear ? undefined : parsed });
            await saveConfig(config);
            runtime = await resolveRuntime(config, runtime.cwd);
            tui?.setEffort(runtime.effort);
            const built = await buildHarness(mode);
            tools = built.tools;
            harness = built.harness;
            display(`Reasoning effort set to: ${runtime.effort ?? 'default'}\n`);
            continue;
          }
          if (input === '/mode' || input.startsWith('/mode ')) {
            const arg = input.slice(5).trim().toLowerCase();
            if (!arg) {
              const order: PermissionMode[] = ['ask', 'plan', 'auto', 'readonly'];
              const next = order[(order.indexOf(mode) + 1) % order.length];
              if (next === 'plan' && mode !== 'plan') prePlanMode = mode;
              mode = next;
            } else if (arg === 'ask' || arg === 'plan' || arg === 'auto' || arg === 'readonly' || arg === 'read-only') {
              const next: PermissionMode = arg === 'read-only' ? 'readonly' : arg;
              if (next === 'plan' && mode !== 'plan') prePlanMode = mode;
              mode = next;
            } else {
              display('Usage: /mode [ask | plan | auto | readonly]\n');
              continue;
            }
            tools.readOnly = mode === 'readonly';
            tools.planMode = mode === 'plan';
            tui?.setMode(mode);
            display(`Mode set to: ${mode}\n`);
            continue;
          }
          if (input === '/plan' || input.startsWith('/plan ')) {
            const arg = input.slice(5).trim();
            const lower = arg.toLowerCase();
            if (!arg) {
              if (mode === 'plan') {
                mode = prePlanMode;
                tools.planMode = false;
                tools.readOnly = mode === 'readonly';
                tui?.setMode(mode);
                display(`Exited plan mode -> restored mode: ${mode}\n`);
              } else {
                prePlanMode = mode;
                mode = 'plan';
                tools.planMode = true;
                tools.readOnly = false;
                tui?.setMode(mode);
                display('Plan mode active (read-only exploration + structured planning; call /plan off or /plan execute when ready).\n');
              }
              continue;
            }
            if (lower === 'off' || lower === 'exit') {
              mode = prePlanMode;
              tools.planMode = false;
              tools.readOnly = mode === 'readonly';
              tui?.setMode(mode);
              display(`Plan mode off -> restored mode: ${mode}\n`);
              continue;
            }
            if (lower === 'execute' || lower === 'approve' || lower === 'run') {
              mode = prePlanMode;
              tools.planMode = false;
              tools.readOnly = mode === 'readonly';
              tui?.setMode(mode);
              display(`Plan mode exited (${mode}) — executing plan...\n`);
              await run('Proceed to implement the proposed plan step by step. Use todo_write to track tasks and run verification checks.');
              continue;
            }
            if (mode !== 'plan') {
              prePlanMode = mode;
              mode = 'plan';
              tools.planMode = true;
              tools.readOnly = false;
              tui?.setMode(mode);
              display('Plan mode active.\n');
            }
            await run(arg);
            continue;
          }
          if (input === '/checkpoints' || input === '/revert list' || input === '/undo list') {
            const cps = store.session.checkpoints ?? [];
            if (!cps.length) {
              display('No turn checkpoints recorded in this session yet.\n');
            } else {
              display(`\nTurn Checkpoints (${cps.length}):\n`);
              for (const cp of cps) {
                const fileDesc = cp.files.length ? `files: ${cp.files.map(f => f.path).join(', ')}` : 'no file changes';
                display(`  #${cp.id} [${cp.createdAt.slice(11, 19)}] "${cp.prompt.slice(0, 60)}" (${fileDesc})\n`);
              }
            }
            continue;
          }
          if (input === '/revert' || input.startsWith('/revert ') || input === '/undo' || input.startsWith('/undo ')) {
            const raw = input.replace(/^\/(?:revert|undo)\b/, '').trim();
            const parts = raw ? raw.split(/\s+/) : [];
            let scope: 'both' | 'code' | 'messages' = 'both';
            let steps = 1;
            for (const p of parts) {
              const lp = p.toLowerCase();
              if (lp === 'code' || lp === 'files' || lp === 'file' || lp === 'source') scope = 'code';
              else if (lp === 'msg' || lp === 'messages' || lp === 'chat') scope = 'messages';
              else if (lp === 'both' || lp === 'all') scope = 'both';
              else {
                const n = Number(p);
                if (Number.isInteger(n) && n > 0) steps = n;
              }
            }
            try {
              const res = await harness.revert({ steps, scope });
              display(
                `\n↺ Reverted ${res.turnsReverted} turn(s) [scope: ${scope}] (${res.messagesRemoved} messages removed)\n` +
                (res.restoredFiles.length ? `  Restored source files: ${res.restoredFiles.join(', ')}\n` : '') +
                (res.deletedFiles.length ? `  Deleted newly created files: ${res.deletedFiles.join(', ')}\n` : '') +
                (!res.restoredFiles.length && !res.deletedFiles.length && scope !== 'messages' ? `  No source files were modified in the reverted turn(s).\n` : '') +
                (res.revertedPrompts.length ? `  Reverted prompt: "${res.revertedPrompts[0].slice(0, 80)}"\n` : '')
              );
            } catch (error) {
              display(`${safeError(error, apiKey)}\n`);
            }
            continue;
          }
          if (input === '/todo' || input.startsWith('/todo ')) {
            const arg = input.slice(5).trim().toLowerCase();
            if (arg === 'clear' || arg === 'reset') {
              tools.todos = [];
              store.session.todos = [];
              await store.save();
              tui?.event({ type: 'todo', todos: [] });
              display('Cleared todo checklist.\n');
            } else {
              display(`\n${formatTodos(tools.todos)}\n`);
            }
            continue;
          }
          if (input === '/fork' || input.startsWith('/fork ')) {
            const title = input.slice(5).trim() || undefined;
            const forkedId = await store.forkToNewFile(title);
            display(`Forked current session (${store.session.messages.length} messages) -> new session ID: ${forkedId}\nResume anytime with: goon --resume ${forkedId}\n`);
            continue;
          }
          if (input.startsWith('/rename ')) {
            const newTitle = input.slice(8).trim();
            if (newTitle) {
              store.session.title = newTitle;
              await store.save();
              tui?.setTitle(`${runtime.model} · ${newTitle}`);
              display(`Session renamed to: ${newTitle}\n`);
            }
            continue;
          }
          if (input === '/export' || input.startsWith('/export ')) {
            const { writeFile } = await import('node:fs/promises');
            const outName = input.slice(7).trim() || `goon-session-${store.session.id.slice(0, 8)}.md`;
            const targetFile = await tools.files.path(outName);
            const lines: string[] = [
              `# Goon Session ${store.session.title ?? store.session.id}`,
              `- **Model**: ${runtime.model} (${runtime.provider})`,
              `- **Updated**: ${store.session.updatedAt}`,
              '',
            ];
            for (const m of store.session.messages) {
              lines.push(`## ${m.role.toUpperCase()}`);
              lines.push(typeof m.content === 'string' ? m.content : JSON.stringify(m.content, null, 2));
              lines.push('');
            }
            await writeFile(targetFile, lines.join('\n'), 'utf8');
            display(`Exported session transcript to ${outName}\n`);
            continue;
          }
          if (input.startsWith('!')) {
            const cmd = input.slice(1).trim();
            if (!cmd) {
              display('Usage: !<command> (runs a shell command directly in workspace and records output in session context)\n');
              continue;
            }
            const { runCommand } = await import('./process.js');
            display(`\n$ ${cmd}\n`);
            try {
              const res = await runCommand('bash', ['-c', cmd], { cwd: runtime.cwd, signal: AbortSignal.timeout(60_000), timeoutMs: 60_000 });
              display(`${res.output || '(no output)'}\n[Exit code: ${res.code}]\n`);
              store.session.messages.push({
                role: 'user',
                content: `<user_shell_command>\n<command>${cmd}</command>\n<result>\nExit code: ${res.code}\n${res.output.slice(0, 8000)}\n</result>\n</user_shell_command>`,
              });
              await store.save();
            } catch (error) {
              display(`Shell command failed: ${safeError(error, apiKey)}\n`);
            }
            continue;
          }
          if (input === '/compact' || input.startsWith('/compact ')) {
            const focus = input.slice(8).trim() || undefined;
            controller = new AbortController();
            tui?.setStatus('Compacting');
            try {
              const res = await harness.compact(controller.signal, focus);
              display(`Context compacted (compaction #${res.compactions}, ${res.tokens} tokens used).\n`);
            } catch (error) {
              display(`${safeError(error, apiKey)}\n`);
            } finally {
              controller = undefined;
              tui?.setStatus('Ready');
            }
            continue;
          }
          if (input === '/diff') {
            const { runCommand } = await import('./process.js');
            try {
              const [statusRes, diffRes] = await Promise.all([
                runCommand('git', ['status', '--short'], { cwd: runtime.cwd, signal: AbortSignal.timeout(10_000), timeoutMs: 10_000 }),
                runCommand('git', ['diff', '--no-ext-diff'], { cwd: runtime.cwd, signal: AbortSignal.timeout(10_000), timeoutMs: 10_000 }),
              ]);
              const statusOut = statusRes.output.trim();
              const diffOut = diffRes.output.trim();
              if (!statusOut && !diffOut) {
                display('Working tree is clean (no uncommitted changes).\n');
              } else {
                if (statusOut) display(`\nGit Status:\n${statusOut}\n`);
                if (diffOut) display(`\n${diffOut.slice(0, 16_000)}\n`);
              }
            } catch (error) {
              display(`Failed to run git diff: ${safeError(error, apiKey)}\n`);
            }
            continue;
          }
          if (input === '/review' || input.startsWith('/review ')) {
            const extra = input.slice(7).trim();
            const reviewPrompt =
              `Review the current changes in this repository (run git status and git diff or inspect modified files). ` +
              `Focus on correctness, bugs, security issues, edge cases, and missing tests. Report findings ordered by severity with exact file and line references.` +
              (extra ? `\nAdditional review focus: ${extra}` : '');
            await run(reviewPrompt);
            continue;
          }
          if (input === '/init') {
            await run(
              'Inspect this workspace (package manifests, build/test scripts, directory structure, existing docs) and create or update AGENTS.md with concise, accurate commands for building, linting, typechecking, testing, and key architectural invariants.'
            );
            continue;
          }
          if (input === '/tools') {
            const names = Object.keys(tools.schemas());
            display(names.map(n => `  ${tools.isReadOnly(n) ? '● [read]' : '◆ [exec]'} ${n}`).join('\n') + '\n');
            continue;
          }
          if (input === '/sessions') {
            const list = await SessionStore.list(join(dataDir(), 'sessions'));
            display((list.length ? list.map(s => `  ${s.id.slice(0, 8)}  ${s.updatedAt}  ${s.model}${s.title ? `  [${s.title}]` : ''}  ${s.cwd}`).join('\n') : '  No saved sessions.') + '\n');
            continue;
          }
          if (input === '/doctor') {
            const { runCommand } = await import('./process.js');
            const ok = async (cmd: string) => {
              try { return (await runCommand(cmd, ['--version'], { cwd: runtime.cwd, signal: AbortSignal.timeout(5000), timeoutMs: 5000 })).code === 0; }
              catch { return false; }
            };
            const [b, r] = await Promise.all([ok('bash'), ok('rg')]);
            display(`Provider: ${runtime.provider} | Model: ${runtime.model} | Effort: ${runtime.effort ?? 'default'} | BaseURL: ${runtime.baseUrl}\nBash: ${b ? 'OK' : 'Missing'} | Ripgrep: ${r ? 'OK' : 'JS fallback'} | API Key: Configured\n`);
            continue;
          }
          if (input === '/status') {
            const { estimatedTokens } = await import('./context.js');
            const usedEst = estimatedTokens(store.session.messages, '');
            const pct = Math.min(100, Math.round((usedEst / runtime.contextWindow) * 100));
            display(
              `Session        ${store.session.id}${store.session.title ? ` (${store.session.title})` : ''}\n` +
              `Provider       ${runtime.provider} (${runtime.baseUrl})\n` +
              `Model          ${runtime.model}\n` +
              `Effort         ${runtime.effort ?? 'default'}\n` +
              `Mode           ${mode}\n` +
              `Workspace      ${runtime.cwd}\n` +
              `Checkpoints    ${store.session.checkpoints?.length ?? 0} turn(s)\n` +
              `Context usage  ~${usedEst} / ${runtime.contextWindow} tokens (${pct}%)\n` +
              `Compactions    ${store.session.compactions}\n` +
              `Session tokens ${store.session.tokens}\n` +
              `MCP tools      ${manager?.tools.length ?? 0}\n`
            );
            continue;
          }
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

