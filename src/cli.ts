import { parseArgs } from 'node:util';
import { createInterface } from 'node:readline/promises';
import { stdin, stdout, stderr } from 'node:process';
import { existsSync, fstatSync, readFileSync, statSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import {
  DEFAULT_GATEWAY_URL,
  EFFORT_LEVELS,
  type Config,
  type EffortLevel,
  configPath,
  dataDir,
  loadConfig,
  normalizeGatewayUrl,
  resolveApiKey,
  resolveRuntime,
  saveConfig,
} from './config.js';
import { safeError } from './errors.js';
import { VERSION } from './version.js';
import { HELP } from './help.js';
import type { HarnessEvent } from './harness.js';
import type { SubagentEvent } from './subagents.js';
import { InputCancelledError, terminalText as cleanTerminal, TerminalUI } from './tui.js';
import type { McpManager } from './mcp.js';
import { EFFORT_DESCRIPTIONS, parseEffortInput, runSetupWizard } from './setup.js';
import { discoverModels, formatModelBadge, resolveModelCapabilities, type DiscoveredModel } from './provider.js';
import { bashApprovalPrefix, isApprovedBashPrefix } from './approval.js';


type PermissionMode = 'ask' | 'plan' | 'auto' | 'readonly';

/**
 * Welcome lines shown when a session opens. Plain text (it is also printed in readline mode); the
 * TUI recognises the `◆ goon v…` title and styles the block quietly. No box: lines wrap freely at
 * any width, so long model IDs and paths never break an outline.
 */
export function welcomeBanner(
  model: string,
  provider: string,
  baseUrl: string,
  effort: EffortLevel | undefined,
  cwd: string,
  sessionId: string,
  mode: PermissionMode,
): string {
  const host = baseUrl.replace(/^https?:\/\//, '').replace(/\/+$/, '');
  const home = process.env.HOME;
  const workspace = home && cwd.startsWith(`${home}/`) ? `~${cwd.slice(home.length)}` : cwd;
  return [
    `◆ goon v${VERSION}`,
    `  model ${model}   effort ${effort ?? 'default'}   mode ${mode === 'auto' ? 'auto-approve' : mode}`,
    `  gateway ${host} (${provider})`,
    `  workspace ${workspace}   session ${sessionId.slice(0, 8)}`,
    '  /help commands   @ files   $ skills   ! shell   shift+tab mode',
    '',
  ].join('\n');
}

/** Reads the current git branch (or short detached SHA) without spawning git; undefined outside a repo. */
export function readGitBranch(cwd: string): string | undefined {
  try {
    let dir = resolve(cwd);
    for (let depth = 0; depth < 64; depth++) {
      const dotGit = join(dir, '.git');
      if (existsSync(dotGit)) {
        let gitDir = dotGit;
        if (statSync(dotGit).isFile()) {
          const pointer = /^gitdir:\s*(.+)$/m.exec(readFileSync(dotGit, 'utf8'));
          if (!pointer) return undefined;
          gitDir = resolve(dir, pointer[1].trim());
        }
        const head = readFileSync(join(gitDir, 'HEAD'), 'utf8').trim();
        const ref = /^ref:\s*refs\/heads\/(.+)$/.exec(head);
        if (ref) return ref[1];
        return /^[0-9a-f]{7,}$/i.test(head) ? head.slice(0, 7) : undefined;
      }
      const parent = dirname(dir);
      if (parent === dir) return undefined;
      dir = parent;
    }
  } catch {
    return undefined;
  }
  return undefined;
}

/** Short folder name for lists: the last path segment (home shown as ~). */
function folderLabel(cwd: string): string {
  const clean = cwd.replace(/[\\/]+$/, '');
  if (process.env.HOME && clean === process.env.HOME) return '~';
  return clean.split(/[\\/]/).pop() || clean;
}

function formatRelativeTime(isoDate: string): string {
  const ts = Date.parse(isoDate);
  if (!Number.isFinite(ts)) return isoDate.slice(0, 16).replace('T', ' ');
  const diffSec = Math.max(0, Math.floor((Date.now() - ts) / 1000));
  if (diffSec < 60) return 'just now';
  const mins = Math.floor(diffSec / 60);
  if (mins < 60) return `${mins}m ago`;
  const hrs = Math.floor(mins / 60);
  if (hrs < 24) return `${hrs}h ago`;
  const days = Math.floor(hrs / 24);
  if (days < 30) return `${days}d ago`;
  return isoDate.slice(0, 10);
}

export function extractCopyableFromMessages(
  messages: Array<{ role: string; content: unknown }>,
  selector = '',
): { text: string; label: string } | undefined {
  const assistantOutputs: string[] = [];
  for (const m of messages) {
    if (m.role !== 'assistant') continue;
    let text = '';
    if (typeof m.content === 'string') {
      text = m.content;
    } else if (Array.isArray(m.content)) {
      text = m.content
        .map(p => (p && typeof p === 'object' && 'type' in p && p.type === 'text' && 'text' in p && typeof p.text === 'string' ? p.text : ''))
        .filter(Boolean)
        .join('\n');
    }
    const cleaned = text.replace(/^<think>[\s\S]*?<\/think>\s*/i, '').trim();
    if (cleaned) assistantOutputs.push(cleaned);
  }
  if (!assistantOutputs.length) return undefined;
  const arg = selector.trim().toLowerCase();
  if (arg === 'code' || arg === 'block' || arg === 'snippet') {
    for (let i = assistantOutputs.length - 1; i >= 0; i--) {
      const blocks = [...assistantOutputs[i].matchAll(/```[^\r\n]*\r?\n([\s\S]*?)```/g)];
      if (blocks.length > 0) {
        const lastBlock = blocks[blocks.length - 1][1].trimEnd();
        if (lastBlock) return { text: lastBlock, label: 'last code block' };
      }
    }
  }
  const idxNum = Number(arg);
  if (Number.isInteger(idxNum) && idxNum >= 1 && idxNum <= assistantOutputs.length) {
    const picked = assistantOutputs[assistantOutputs.length - idxNum];
    return { text: picked, label: idxNum === 1 ? 'last response' : `response #${idxNum} from latest` };
  }
  return { text: assistantOutputs[assistantOutputs.length - 1], label: 'last response' };
}

/** Clipboard helpers in preference order for the current platform (Termux, Wayland, X11, macOS, Windows). */
export function clipboardCandidates(platform = process.platform, env: NodeJS.ProcessEnv = process.env): Array<{ bin: string; args: string[] }> {
  if (platform === 'win32') {
    return [{ bin: 'powershell.exe', args: ['-NoProfile', '-NonInteractive', '-Command', '[Console]::InputEncoding = [System.Text.Encoding]::UTF8; Set-Clipboard -Value ([Console]::In.ReadToEnd())'] }];
  }
  if (platform === 'darwin') return [{ bin: 'pbcopy', args: [] }];
  const candidates: Array<{ bin: string; args: string[] }> = [];
  if (env.TERMUX_VERSION || env.PREFIX?.includes('com.termux')) candidates.push({ bin: 'termux-clipboard-set', args: [] });
  if (env.WAYLAND_DISPLAY) candidates.push({ bin: 'wl-copy', args: [] });
  candidates.push({ bin: 'xclip', args: ['-selection', 'clipboard'] }, { bin: 'xsel', args: ['--clipboard', '--input'] });
  return candidates;
}

async function copyTextToClipboard(text: string): Promise<boolean> {
  let copied = false;
  if (stdout.isTTY && text.length <= 100_000) {
    // OSC 52 works over SSH and in most modern terminals, even without a clipboard helper.
    const b64 = Buffer.from(text, 'utf8').toString('base64');
    stdout.write(`\x1b]52;c;${b64}\x07`);
    copied = true;
  }
  const { spawn } = await import('node:child_process');
  const { childEnv } = await import('./process.js');
  for (const cmd of clipboardCandidates()) {
    try {
      await new Promise<void>((resolve, reject) => {
        const child = spawn(cmd.bin, cmd.args, { stdio: ['pipe', 'ignore', 'ignore'], windowsHide: true, env: childEnv() });
        const timer = setTimeout(() => { child.kill(); reject(new Error('clipboard helper timed out')); }, 3000);
        child.on('error', error => { clearTimeout(timer); reject(error); });
        child.on('close', code => { clearTimeout(timer); if (code === 0) resolve(); else reject(new Error(`exit ${code}`)); });
        child.stdin.on('error', () => {});
        child.stdin.end(text, 'utf8');
      });
      return true;
    } catch {
      // try the next helper
    }
  }
  return copied;
}

/**
 * Piped data is read only from a real pipe or file (`cmd | goon -p ...`, `goon -p ... < file`).
 * Sockets and inherited descriptors that never close (IDE/agent spawns, CI runners) are ignored so
 * `-p` can never hang waiting for an EOF that does not come.
 */
function pipedStdin(): boolean {
  if (stdin.isTTY) return false;
  try {
    const info = fstatSync(0);
    return info.isFIFO() || info.isFile();
  } catch {
    return false;
  }
}

async function readStdinLimited(limit: number): Promise<string> {
  let bytes = 0;
  const chunks: Buffer[] = [];
  for await (const chunk of stdin) {
    const buffer = Buffer.from(chunk);
    bytes += buffer.length;
    if (bytes > limit) throw new Error(`Piped input exceeds ${Math.round(limit / 1024)} KiB.`);
    chunks.push(buffer);
  }
  return Buffer.concat(chunks).toString('utf8');
}

/** Environment problems `goon doctor` reports in addition to its pass/fail checks. */
async function doctorWarnings(config: Config, baseUrl: string, modelIds: string[]): Promise<string[]> {
  const warnings: string[] = [];
  const [major, minor] = process.versions.node.split('.').map(Number);
  if (major < 22 || (major === 22 && minor < 16)) warnings.push(`Node ${process.version} is older than the supported 22.16.`);
  const insecure = insecureBaseUrlWarning(baseUrl);
  if (insecure) warnings.push(insecure);
  if (config.model && modelIds.length > 0 && !modelIds.includes(config.model)) {
    warnings.push(`Model "${config.model}" is not listed by ${baseUrl}/models.`);
  }
  if (process.platform !== 'win32') {
    const { credentialsPath } = await import('./config.js');
    try {
      const mode = statSync(credentialsPath()).mode & 0o777;
      if (mode & 0o077) warnings.push(`${credentialsPath()} is readable by other users (mode ${mode.toString(8)}); run: chmod 600 "${credentialsPath()}"`);
    } catch {
      // no stored credentials
    }
  }
  return warnings;
}

/** API keys sent over plain HTTP are readable by anyone on the network path (except loopback). */
export function insecureBaseUrlWarning(baseUrl: string | undefined): string | undefined {
  if (!baseUrl) return undefined;
  try {
    const url = new URL(baseUrl);
    const loopback = ['localhost', '127.0.0.1', '[::1]', '::1'].includes(url.hostname) || url.hostname.endsWith('.localhost');
    if (url.protocol === 'http:' && !loopback) {
      return `Warning: ${url.host} uses plain http; your API key is sent unencrypted. Use https:// unless this is a trusted local network.`;
    }
  } catch {
    // invalid URLs are rejected by config validation
  }
  return undefined;
}

export async function main(argv = process.argv.slice(2)): Promise<void> {
  let apiKey: string | undefined;
  let jsonOutput = false;
  try {
    if (argv[0] === 'mcp' && argv[1] === 'add') {
      const { loadMcpConfig, saveMcpConfig, mcpServerSchema } = await import('./mcp.js');
      const rest = argv.slice(2);
      const dashDashIdx = rest.indexOf('--');
      const flagPart = dashDashIdx >= 0 ? rest.slice(0, dashDashIdx) : rest;
      const cmdPart = dashDashIdx >= 0 ? rest.slice(dashDashIdx + 1) : [];
      const addParsed = parseArgs({
        args: flagPart,
        allowPositionals: true,
        strict: false,
        options: {
          url: { type: 'string' },
          'bearer-env': { type: 'string' },
          'bearer-token-env-var': { type: 'string' },
          env: { type: 'string', multiple: true },
          trust: { type: 'boolean' },
          'timeout-ms': { type: 'string' },
        },
      });
      const name = addParsed.positionals[0];
      if (!name) {
        throw new Error('Usage: goon mcp add <name> (--url <URL> [--bearer-env ENV] | -- <command> [args...])');
      }
      const envMap: Record<string, string> = {};
      for (const pair of (addParsed.values.env as string[] | undefined) ?? []) {
        const eq = pair.indexOf('=');
        if (eq > 0) envMap[pair.slice(0, eq).trim()] = pair.slice(eq + 1);
      }
      const urlVal = typeof addParsed.values.url === 'string' ? addParsed.values.url : undefined;
      const commandTokens = cmdPart.length ? cmdPart : addParsed.positionals.slice(1);
      if (!urlVal && !commandTokens.length) {
        throw new Error('Specify either --url <URL> or -- <command> [args...] when adding an MCP server.');
      }
      const bearerEnv =
        (typeof addParsed.values['bearer-env'] === 'string' ? addParsed.values['bearer-env'] : undefined) ??
        (typeof addParsed.values['bearer-token-env-var'] === 'string' ? addParsed.values['bearer-token-env-var'] : undefined);
      const timeoutMs =
        typeof addParsed.values['timeout-ms'] === 'string' ? Number(addParsed.values['timeout-ms']) : undefined;
      const serverEntry = mcpServerSchema.parse(
        urlVal
          ? {
              transport: 'http',
              url: urlVal,
              ...(bearerEnv ? { bearerEnv } : {}),
              ...(addParsed.values.trust === true ? { trust: true } : {}),
              ...(timeoutMs ? { timeoutMs } : {}),
            }
          : {
              transport: 'stdio',
              command: commandTokens[0],
              args: commandTokens.slice(1),
              ...(Object.keys(envMap).length ? { env: envMap } : {}),
              ...(addParsed.values.trust === true ? { trust: true } : {}),
              ...(timeoutMs ? { timeoutMs } : {}),
            },
      );
      const mcpConfig = await loadMcpConfig();
      mcpConfig.servers[name] = serverEntry;
      await saveMcpConfig(mcpConfig);
      stdout.write(`Added MCP server "${name}" (${serverEntry.transport}).\n`);
      return;
    }

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
      'output-last-message': { type: 'string', short: 'o' },
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
      const { loadMcpConfig, saveMcpConfig, mcpConfigPath, McpManager, importCodexMcpServers } = await import('./mcp.js');
      const mcpConfig = await loadMcpConfig();
      const sub = positionals[1];
      if (sub === 'list') {
        stdout.write(JSON.stringify({ path: mcpConfigPath(), servers: Object.entries(mcpConfig.servers).map(([name, server]) =>
          ({ name, transport: server.transport, disabled: server.disabled, trust: server.trust, readOnlyTools: server.readOnlyTools })) }, null, 2) + '\n');
        return;
      }
      if (sub === 'get') {
        const target = positionals[2];
        if (!target || !mcpConfig.servers[target]) throw new Error(`MCP server "${target ?? ''}" not found.`);
        stdout.write(JSON.stringify({ name: target, ...mcpConfig.servers[target] }, null, 2) + '\n');
        return;
      }
      if (sub === 'remove' || sub === 'rm') {
        const target = positionals[2];
        if (!target || !mcpConfig.servers[target]) throw new Error(`MCP server "${target ?? ''}" not found.`);
        delete mcpConfig.servers[target];
        await saveMcpConfig(mcpConfig);
        stdout.write(`Removed MCP server "${target}".\n`);
        return;
      }
      if (sub === 'import-codex') {
        const filterNames = positionals.slice(2);
        const res = await importCodexMcpServers(undefined, filterNames.length ? filterNames : undefined);
        stdout.write(JSON.stringify(res, null, 2) + '\n');
        return;
      }
      if (sub !== 'check') {
        throw new Error('Usage: goon mcp list | check [name] | get <name> | add <name> ... | remove <name> | import-codex [name...]');
      }
      const targetFilter = positionals[2];
      const filteredConfig = targetFilter
        ? {
            servers: Object.fromEntries(
              Object.entries(mcpConfig.servers).filter(([k]) => k === targetFilter),
            ),
          }
        : mcpConfig;
      const manager = new McpManager(filteredConfig, resolve(values.cwd ?? '.'));
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
      const targetUrl = normalizeGatewayUrl(config.baseUrl ?? DEFAULT_GATEWAY_URL);
      const discovered = await discoverModels(targetUrl, resolvedKey ?? '', config.provider);
      const modelIds = discovered.map((m) => m.id);
      const modelAvailable = config.model ? (modelIds.length === 0 || modelIds.includes(config.model)) : false;
      const warnings = await doctorWarnings(config, targetUrl, modelIds);
      stdout.write(JSON.stringify({ version: VERSION, node: process.version, platform: process.platform, arch: process.arch,
        config: configPath(), provider: config.provider, model: config.model ?? null, effort: config.effort ?? 'default',
        baseUrl: config.baseUrl ?? 'provider default', credentialVariable: config.keyEnv, credentialPresent: Boolean(resolvedKey),
        bash, ripgrep: rg, searchFallback: !rg, data: dataDir(), networkTested: true, gatewayReachable: modelIds.length > 0,
        discoveredModels: modelIds, modelAvailable, warnings }, null, 2) + '\n');
      if (!bash || !config.model || !resolvedKey) process.exitCode = 1;
      return;
    }
    const {
      SessionStore,
      sameWorkspace,
      GOAL_USAGE,
      goalStatusLabel,
      formatGoalUsageSummary,
      formatGoalStatusIndicator,
      formatGoalSummaryLines,
      parseGoalDraftArgs,
    } = await import('./session.js');
    if (positionals[0] === 'sessions') {
      const list = await SessionStore.list(join(dataDir(), 'sessions'));
      stdout.write(values.json ? JSON.stringify(list) + '\n' : list.map(s => `${s.id}  ${s.updatedAt}  ${s.model}${s.title ? `  [${s.title}]` : ''}  ${s.cwd}`).join('\n') + '\n');
      return;
    }
    let resumeTarget = values.resume;
    let launchResumePicker = false;
    let promptPositionals = positionals;
    if (positionals[0] === 'resume') {
      const subTarget = positionals[1]?.trim();
      if (subTarget) {
        resumeTarget = subTarget;
        promptPositionals = positionals.slice(2);
      } else {
        launchResumePicker = true;
        promptPositionals = [];
      }
    }
    if (values.prompt && promptPositionals.length) throw new Error('Use either --prompt or positional prompt text.');
    let prompt = values.prompt ?? (promptPositionals.length ? promptPositionals.join(' ') : undefined);
    if (prompt && pipedStdin()) {
      // `git diff | goon -p "review"` / `goon -p "explain" < log`: attach piped data as context.
      const piped = await readStdinLimited(1024 * 1024);
      if (piped.trim()) prompt = `${prompt}\n\n<stdin>\n${piped.replace(/\s+$/, '')}\n</stdin>`;
    }
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
    const interactive = !prompt;
    const outputLastMessage = typeof values['output-last-message'] === 'string' ? values['output-last-message'] : undefined;
    if (outputLastMessage && interactive) throw new Error('--output-last-message requires a prompt (-p or piped input).');
    let viewMode: 'normal' | 'verbose' | 'transcript' =
      config.viewMode ?? (config.showReasoning ? 'verbose' : 'normal');
    if (interactive && !jsonOutput) {
      config.showReasoning = true;
    }
    let runtime = await resolveRuntime(config, values.cwd ?? process.cwd());
    apiKey = runtime.apiKey;
    const insecureWarning = insecureBaseUrlWarning(runtime.baseUrl);
    if (insecureWarning && !jsonOutput) stderr.write(`goon: ${insecureWarning}\n`);
    const sessionsRootDir = join(runtime.dataDir, 'sessions');
    if (launchResumePicker && !resumeTarget) {
      // `goon resume` works from any folder: with one saved session it resumes it directly,
      // otherwise the picker opens (This folder / All projects) on a fresh, discardable session.
      const resumable = (await SessionStore.list(sessionsRootDir)).filter(s => (s.messagesCount ?? 0) > 0 || Boolean(s.title));
      if (resumable.length === 1) {
        resumeTarget = resumable[0].id;
        launchResumePicker = false;
      } else if (!resumable.length) {
        launchResumePicker = false;
      }
    }
    if (resumeTarget && resumeTarget !== 'latest' && !values.cwd) {
      // Resuming by ID from another folder switches to that session's workspace instead of failing
      // with "belongs to a different workspace" (an explicit --cwd still wins).
      const all = await SessionStore.list(sessionsRootDir);
      const wanted = resumeTarget.toLowerCase();
      const isIndex = /^\d{1,3}$/.test(wanted);
      const match = all.find(s => s.id.toLowerCase() === wanted)
        ?? (!isIndex && wanted.length >= 4
          ? all.find(s => sameWorkspace(s.cwd, runtime.cwd) && s.id.toLowerCase().startsWith(wanted)) ?? all.find(s => s.id.toLowerCase().startsWith(wanted))
          : undefined);
      if (match && !sameWorkspace(match.cwd, runtime.cwd)) {
        const { realpath } = await import('node:fs/promises');
        const folder = await realpath(match.cwd).catch(() => undefined);
        if (folder) {
          runtime = await resolveRuntime(config, folder);
          resumeTarget = match.id;
        }
      }
    }
    let store = await SessionStore.open(sessionsRootDir, runtime, resumeTarget);
    let rl: ReturnType<typeof createInterface> | undefined;
    let tui: TerminalUI | undefined;
    let manager: McpManager | undefined;
    let subagents: import('./subagents.js').Subagents | undefined;
    let controller: AbortController | undefined;
    let denied = false, exiting = false, terminating = false, starting = true;
    let mode: PermissionMode = values['read-only']
      ? 'readonly'
      : values.plan
        ? 'plan'
        : values.yes
          ? 'auto'
          : (config.approvalMode ?? 'ask');
    let prePlanMode: PermissionMode = values.yes || config.approvalMode === 'auto' ? 'auto' : 'ask';

    const sessionAllowedTools = new Set<string>();
    const sessionAllowedBashPrefixes = new Set<string>();
    // Terminal bell when goon needs the user (approval, question) or a long turn finishes, so a
    // developer who switched windows notices. Disable with GOON_NOTIFY=0.
    const notifyEnabled = process.env.GOON_NOTIFY !== '0' && process.env.GOON_NOTIFY !== 'off';
    const notifyAttention = () => {
      if (notifyEnabled && stdout.isTTY && !jsonOutput) stdout.write('\x07');
    };
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
    const onIpcSignal = (msg: unknown) => {
      if (msg && typeof msg === 'object' && (msg as { type?: string }).type === 'goon:signal') {
        const sig = (msg as { signal?: string }).signal;
        if (sig === 'SIGINT') stop();
        else if (sig === 'SIGTERM') terminate();
      }
    };
    process.on('SIGINT', stop); process.on('SIGTERM', terminate); process.on('message', onIpcSignal);
    try {
      if (interactive && stdin.isTTY && stdout.isTTY && !jsonOutput && !values['no-tui'] && process.env.TERM !== 'dumb') {
        const titleLabel = store.session.title ? `${runtime.model} · ${store.session.title}` : `${runtime.model} · ${store.session.id.slice(0, 8)}`;
        tui = new TerminalUI(titleLabel, stop);
        tui.setMode(mode);
        tui.setViewMode(viewMode);
        tui.setEffort(runtime.effort);
        tui.setTokens(store.session.tokens);
      } else if (stdin.isTTY) rl = createInterface({ input: stdin, output: stderr, terminal: true });
      rl?.on('SIGINT', stop);
      const {
        estimatedTokens,
        buildGoalContinuationPrompt,
        buildGoalObjectiveUpdatedPrompt,
        buildUserGoalContextMessage,
      } = await import('./context.js');
      const syncTuiHeader = (resetContext = false) => {
        if (!tui) return;
        tui.setWorkspace(runtime.cwd, readGitBranch(runtime.cwd));
        tui.setContextWindow(runtime.contextWindow);
        tui.setGoal(store.session.goal);
        if (resetContext) {
          tui.setContextUsage(store.session.messages.length ? estimatedTokens(store.session.messages, '') : undefined);
        }
      };
      syncTuiHeader(true);
      const display = (text: string) => tui ? tui.write(text) : stderr.write(cleanTerminal(text));
      const question = (label: string, signal?: AbortSignal, remember = false) => tui ? tui.question(label, signal, remember)
        : rl ? rl.question(label, { signal }) : Promise.reject(new Error('Input requires a TTY.'));
      // Plain readline mode: read a secret without echoing it.
      const questionHidden = async (label: string): Promise<string> => {
        if (!rl) return question(label);
        const writer = rl as unknown as { _writeToOutput: (text: string) => void };
        const original = writer._writeToOutput;
        stderr.write(label);
        writer._writeToOutput = (text: string) => { if (/^[\r\n]+$/.test(text)) original.call(rl, text); };
        try { return await rl.question(''); }
        finally { writer._writeToOutput = original; }
      };
      let reasoningSource: 'inline' | 'provider' | undefined;
      const emit = (event: HarnessEvent | SubagentEvent) => {
        if (event.type === 'stopped') denied = true;
        if (event.type === 'plan_exit') {
          mode = prePlanMode;
          tui?.setMode(mode);
        }
        if (event.type === 'retry') {
          if (tui) tui.setNotice(`↻ API error (${event.message.slice(0, 64)}) — retrying (${event.attempt}/${event.maxRetries})...`);
          else if (!jsonOutput) stderr.write(`\n[Retry ${event.attempt}/${event.maxRetries}: ${cleanTerminal(event.message)}]\n`);
          return;
        }
        if (event.type === 'tool_stream') {
          if (tui) tui.event(event);
          return;
        }
        if (jsonOutput) { stdout.write(JSON.stringify(event) + '\n'); return; }
        if (tui) { tui.event(event); return; }
        if (event.type === 'reasoning') {
          if (viewMode === 'normal') return;
          if (reasoningSource !== event.source) stderr.write(`\n[reasoning · ${event.source}]\n`);
          stderr.write(cleanTerminal(event.text)); reasoningSource = event.source; return;
        }
        if (reasoningSource) { stderr.write('\n'); reasoningSource = undefined; }
        if (event.type === 'text') stdout.write(cleanTerminal(event.text));
        else if (event.type === 'subagent' && event.state !== 'progress') stderr.write(`\n[agent ${event.id.slice(0, 8)}: ${event.state}]\n`);
        else if (event.type === 'tool' && event.state === 'start') stderr.write(`\n[${event.name}]\n`);
        else if (event.type === 'todo') stderr.write(`\n[todo: ${event.todos.filter(t => t.status === 'completed').length}/${event.todos.length} completed]\n`);
        else if (event.type === 'goal') stderr.write(`\n[${event.goal ? formatGoalStatusIndicator(event.goal) : 'Goal cleared'}]\n`);
        else if (event.type === 'plan_exit') stderr.write('\n[Plan approved — exited plan mode]\n');
        else if (event.type === 'compaction') stderr.write('\n[Context compacted; original history archived.]\n');
        else if (event.type === 'stopped') stderr.write(`\n${event.reason}\n`);
      };
      const { ToolRegistry, formatTodos } = await import('./tools.js');
      const { Subagents } = await import('./subagents.js');
      const { Harness } = await import('./harness.js');

      const buildHarness = async (currentMode: PermissionMode, options: { reconnectMcp?: boolean } = {}) => {
        const reg = new ToolRegistry(runtime.cwd, async request => {
          // Only the live mode counts. The mode captured when this harness was built must not keep
          // auto-approving after the user switches back to ask/plan (Shift+Tab, /mode, /auto off).
          if (mode === 'auto' || sessionAllowedTools.has(request.tool)) return true;
          if (request.tool === 'bash' && isApprovedBashPrefix(request.input, sessionAllowedBashPrefixes)) return true;
          if (!rl && !tui) return false;
          if (request.tool === 'exit_plan_mode' && request.input && typeof request.input === 'object' && 'plan' in request.input) {
            const planStr = String((request.input as { plan: unknown }).plan);
            const planPrompt = `\n≡ Proposed plan\n${planStr.split(/\r?\n/).map(l => (l.trim() ? `  ${l}` : l)).join('\n')}\n\n  Approve plan and exit plan mode?\n`;
            const reply = tui
              ? (await tui.askTransient(planPrompt, '[y/N] ', request.signal)).trim().toLowerCase()
              : (display(planPrompt), (await question('[y/N] ', request.signal)).trim().toLowerCase());
            return reply === 'y' || reply === 'yes';
          }
          const diff = await reg.files.previewDiff(request.tool, request.input);
          let rawPreview: string;
          if (diff) {
            const diffLines = diff.split('\n');
            rawPreview = diffLines.length > 80
              ? `${diffLines.slice(0, 80).join('\n')}\n... (+${diffLines.length - 80} more diff lines)`
              : diff;
          } else if (request.tool === 'bash' && request.input && typeof request.input === 'object' && 'command' in request.input) {
            const cmd = String((request.input as { command: unknown }).command ?? '').trim();
            rawPreview = `  $ ${cmd}`;
          } else {
            rawPreview = JSON.stringify(request.input, null, 2);
          }
          const preview = cleanTerminal(rawPreview.split(runtime.apiKey).join('[REDACTED]').slice(0, 8000));
          const prefix = request.tool === 'bash' ? bashApprovalPrefix(request.input) : undefined;
          const prefixHint = prefix ? `\n  p = always allow commands starting with "${prefix}" (no ; & | \` $ < > in them)` : '';
          const approvalBlock = `\n▲ Allow ${request.tool}?\n${preview}${prefixHint}\n`;
          const label = prefix ? '[y/N/p(refix)/a(lways)] ' : '[y/N/a(lways)] ';
          notifyAttention();
          const reply = tui
            ? (await tui.askTransient(approvalBlock, label, request.signal)).trim().toLowerCase()
            : (display(approvalBlock), (await question(label, request.signal)).trim().toLowerCase());
          if (reply === 'a' || reply === 'always') {
            sessionAllowedTools.add(request.tool);
            return true;
          }
          if (prefix && (reply === 'p' || reply === 'prefix')) {
            sessionAllowedBashPrefixes.add(prefix);
            return true;
          }
          return reply === 'y' || reply === 'yes';
        }, currentMode === 'readonly');
        reg.planMode = currentMode === 'plan';
        if (rl || tui) {
          reg.askUser = async (questions, callSignal) => {
            const out: Record<string, string[]> = {};
            for (const q of questions) {
              const optLines = q.options.map(
                (o, idx) => `  ${idx + 1}. ${o.label}${o.description ? ` — ${o.description}` : ''}`,
              );
              const qBlock = `\n◇ ${q.header || 'Question'}\n  ${q.question}\n${optLines.join('\n')}\n`;
              const promptLabel = `Select [1-${q.options.length}] or type answer (Enter for 1): `;
              const rawReply = tui
                ? (await tui.askTransient(qBlock, promptLabel, callSignal, { escHint: 'picks 1' })).trim()
                : (display(qBlock), (await question(promptLabel, callSignal)).trim());
              const num = Number(rawReply);
              if (!rawReply) {
                out[q.id] = [q.options[0]?.label ?? ''];
              } else if (Number.isInteger(num) && num >= 1 && num <= q.options.length) {
                out[q.id] = [q.options[num - 1].label];
              } else {
                out[q.id] = [rawReply];
              }
            }
            return out;
          };
        }
        // Running MCP servers are reused across harness rebuilds (effort, model, session switches);
        // they restart only when asked to (/mcp reload) or when the workspace directory changed.
        const reuseMcp = !values['no-mcp'] && manager?.connected && !options.reconnectMcp && manager.cwd === runtime.cwd;
        if (reuseMcp) {
          manager!.attach(reg);
        } else if (!values['no-mcp'] && manager) {
          await manager.close();
          manager = undefined;
        }
        if (!values['no-mcp'] && !reuseMcp) {
          const { loadMcpConfig, McpManager } = await import('./mcp.js');
          controller = new AbortController();
          try {
            manager = new McpManager(await loadMcpConfig(), runtime.cwd);
            const connecting = manager.connect(reg, controller.signal);
            // Restores the previous status afterwards so rebuilds (/setup, /model, /cd…) never stick on Working.
            await (tui ? tui.withStatus('Connecting MCP', () => connecting) : connecting);
            for (const st of manager.statuses) {
              if (st.status === 'error') {
                display(`[MCP warning: server "${st.name}" failed to start: ${st.error ?? 'unknown error'}]\n`);
              }
            }
          } catch (error) {
            if (!interactive || controller.signal.aborted) {
              controller = undefined;
              throw error;
            }
            display(`[MCP warning: ${safeError(error, apiKey)}]\n`);
          } finally {
            controller = undefined;
          }
        }
        if (subagents) {
          await subagents.closeAll();
          subagents = undefined;
        }
        if (!values['no-subagents']) {
          subagents = new Subagents(runtime, store, reg, emit);
          subagents.register();
        }
        return { tools: reg, harness: new Harness(runtime, store, reg, emit) };
      };

      const applyPermissionMode = (nextMode: PermissionMode) => {
        if (nextMode === 'plan' && mode !== 'plan') prePlanMode = mode;
        mode = nextMode;
        tools.readOnly = mode === 'readonly';
        tools.planMode = mode === 'plan';
        tui?.setMode(mode);
        config.approvalMode = mode;
        void saveConfig(config).catch(() => {});
        const label = mode === 'auto' ? 'auto-approve (all tools & commands auto-approved)' : mode;
        if (tui) tui.setNotice(`Mode: ${label}`);
        display(`\n[Mode switched to: ${label}]\n`);
      };

      let { tools, harness } = await buildHarness(mode);
      let rebuildPending = false;
      const rebuildHarness = async (options: { reconnectMcp?: boolean } = {}) => {
        const built = await buildHarness(mode, options);
        tools = built.tools;
        harness = built.harness;
      };
      // Rebuilding closes the MCP servers and subagents the running turn uses, so while a turn is
      // active the rebuild waits until it ends (the new settings apply from the next turn).
      const requestRebuild = async (): Promise<boolean> => {
        if (controller || harness.busy) {
          rebuildPending = true;
          return false;
        }
        rebuildPending = false;
        await rebuildHarness();
        return true;
      };
      const applyPendingRebuild = async () => {
        if (rebuildPending && !controller && !harness.busy) {
          rebuildPending = false;
          await rebuildHarness();
        }
      };
      const applyEffort = async (next: EffortLevel | undefined) => {
        config = await loadConfig({ ...config, effort: next });
        await saveConfig(config);
        runtime = await resolveRuntime(config, runtime.cwd);
        tui?.setEffort(runtime.effort);
        syncTuiHeader();
        const applied = await requestRebuild();
        const suffix = applied ? '' : ' (applies from the next turn)';
        if (tui) tui.setNotice(`Reasoning effort: ${runtime.effort ?? 'default'}${suffix}`);
        display(`\n[Reasoning effort set to: ${runtime.effort ?? 'default'}${suffix}]\n`);
      };
      if (tui) {
        void tools.files.listWorkspaceFiles().then(files => tui?.setWorkspaceFiles(files)).catch(() => {});
        tui.onCopy = async () => {
          const found = extractCopyableFromMessages(store.session.messages);
          if (!found) {
            tui?.setNotice('Nothing to copy yet (no assistant output in this session).');
            return;
          }
          const ok = await copyTextToClipboard(found.text);
          const lines = found.text.split(/\r?\n/).length;
          tui?.setNotice(
            ok
              ? `Copied ${found.label} (${found.text.length} chars · ${lines} lines) to clipboard`
              : `Failed to copy ${found.label} to system clipboard`,
          );
        };
        tui.onToggleMode = () => {
          const order: PermissionMode[] = ['ask', 'auto', 'plan', 'readonly'];
          const next = order[(order.indexOf(mode) + 1) % order.length];
          applyPermissionMode(next);
        };
        tui.onCycleEffort = direction => {
          const ring: (EffortLevel | undefined)[] = [undefined, ...EFFORT_LEVELS];
          const curIdx = ring.indexOf(runtime.effort);
          const nextEffort = ring[(curIdx + direction + ring.length) % ring.length];
          void applyEffort(nextEffort).catch(error => display(`\n[Effort change failed: ${safeError(error, apiKey)}]\n`));
        };
      }
      const renderResumedHistory = (showBanner = true) => {
        const msgs = store.session.messages;
        if (!msgs.length) return;
        if (tui) {
          const tail = msgs.slice(-30);
          for (const m of tail) {
            if (m.role === 'user') {
              const rawText = typeof m.content === 'string'
                ? m.content
                : Array.isArray(m.content)
                  ? m.content.map(p => ('text' in p && typeof p.text === 'string' ? p.text : p.type === 'image' ? '[Image]' : '')).filter(Boolean).join(' ')
                  : '';
              if (rawText.startsWith('<codex_internal_context source="')) {
                continue;
              }
              const cleaned = rawText
                .replace(/<referenced_file\s+path="([^"]+)"[\s\S]*?<\/referenced_file>/g, '[@$1]')
                .replace(/\r?\n/g, ' ')
                .trim();
              if (cleaned) {
                tui.markTurnStart();
                display(`\n> ${cleaned}\n`);
              }
            } else if (m.role === 'assistant') {
              if (typeof m.content === 'string') {
                const txt = m.content.trim();
                if (txt) display(`\n[answer]\n${txt}\n`);
              } else if (Array.isArray(m.content)) {
                for (const part of m.content) {
                  if (part.type === 'reasoning' && 'text' in part && typeof part.text === 'string' && part.text.trim()) {
                    display(`\n[reasoning]\n${part.text.trim()}\n`);
                  } else if (part.type === 'text' && 'text' in part && typeof part.text === 'string' && part.text.trim()) {
                    display(`\n[answer]\n${part.text.trim()}\n`);
                  }
                }
              }
            }
          }
          if (showBanner) {
            const titleSuffix = store.session.title ? ` (${store.session.title})` : '';
            display(
              `\n↻ Resumed session ${store.session.id.slice(0, 8)}${titleSuffix} · ${msgs.length} messages · ${store.session.checkpoints?.length ?? 0} checkpoints\n`,
            );
          }
          if (store.session.todos?.length) {
            tui.event({ type: 'todo', todos: store.session.todos });
          }
          return;
        }
        const lines: string[] = [
          `\n↻ Resumed session ${store.session.id.slice(0, 8)}${store.session.title ? ` (${store.session.title})` : ''}: ${msgs.length} messages, ${store.session.checkpoints?.length ?? 0} checkpoints`,
        ];
        const previewMsgs = msgs.filter(m => m.role === 'user' || m.role === 'assistant').slice(-6);
        for (const m of previewMsgs) {
          const text = typeof m.content === 'string'
            ? m.content
            : Array.isArray(m.content)
              ? m.content.map(p => ('text' in p && typeof p.text === 'string' ? p.text : p.type === 'tool-call' ? `[tool:${p.toolName}]` : '')).filter(Boolean).join(' ')
              : '';
          if (text.startsWith('<codex_internal_context source="')) {
            continue;
          }
          const cleaned = text.replace(/<referenced_file[\s\S]*?<\/referenced_file>/g, '[attached file]').replace(/\s+/g, ' ').trim();
          if (cleaned) {
            lines.push(`  ${m.role === 'user' ? '›' : '◆'} ${cleaned.slice(0, 140)}${cleaned.length > 140 ? '…' : ''}`);
          }
        }
        display(`${lines.join('\n')}\n`);
      };

      starting = false;
      if (jsonOutput) stdout.write(JSON.stringify({ type: 'session', id: store.session.id, cwd: runtime.cwd }) + '\n');
      else if (interactive) {
        display(welcomeBanner(runtime.model, runtime.provider, runtime.baseUrl, runtime.effort, runtime.cwd, store.session.id, mode));
        renderResumedHistory();
      }
      else display(`goon · ${runtime.model} · session ${store.session.id}\n`);
      let goalContinuationDeferred = false;

      const runTurn = async (
        input: string,
        opts?: { isGoalContinuation?: boolean; continuationStep?: number },
      ): Promise<{ aborted: boolean; hadError: boolean; text: string }> => {
        if (Buffer.byteLength(input) > 1024 * 1024) throw new Error('Prompt exceeds 1 MiB.');
        await applyPendingRebuild();
        denied = false;
        let aborted = false;
        let hadError = false;
        let outText = '';
        const turnStarted = Date.now();
        controller = new AbortController();
        tui?.setStatus('Running');
        if (tui) {
          tui.markTurnStart();
          if (opts?.isGoalContinuation) {
            const g = store.session.goal;
            const stepLabel = opts.continuationStep && opts.continuationStep > 1 ? ` · turn ${opts.continuationStep}` : '';
            const statusInd = g ? formatGoalStatusIndicator(g) : 'Pursuing goal';
            tui.write(`\n◆ ${statusInd}${stepLabel}${g ? ` — ${g.objective}` : ''}\n`);
          } else {
            tui.write(`\n> ${input}\n`);
          }
        } else if (opts?.isGoalContinuation && !jsonOutput) {
          const g = store.session.goal;
          const stepLabel = opts.continuationStep && opts.continuationStep > 1 ? ` · turn ${opts.continuationStep}` : '';
          display(`\n[◆ Pursuing goal${stepLabel}${g ? `: ${g.objective}` : ''}]\n`);
        }
        try {
          outText = await harness.run(input, controller.signal);
          if (outputLastMessage) {
            const { writeFile } = await import('node:fs/promises');
            await writeFile(resolve(outputLastMessage), outText.endsWith('\n') ? outText : `${outText}\n`, 'utf8');
          }
          if (jsonOutput) stdout.write(JSON.stringify({ type: 'done', text: outText, denied, sessionId: store.session.id }) + '\n');
          else if (tui) tui.write('\n'); else stdout.write('\n');
          if (denied && !interactive) process.exitCode = 2;
        } catch (error) {
          aborted = controller.signal.aborted;
          hadError = true;
          if (!interactive) process.exitCode = aborted ? terminating ? 143 : 130 : 1;
          const message = safeError(error, apiKey);
          if (jsonOutput) stdout.write(JSON.stringify({ type: 'error', message }) + '\n');
          else display(`\n${message}\n`);
          if (aborted && !exiting && !terminating && store.session.goal?.status === 'active') {
            goalContinuationDeferred = true;
            if (tui) tui.setNotice('Goal continuation paused after interrupt — run /goal resume to continue.');
            else display('[Goal continuation paused after interrupt — run /goal resume to continue.]\n');
          }
        } finally {
          controller = undefined;
          if (interactive && Date.now() - turnStarted >= 15_000) notifyAttention();
          tui?.setStatus('Ready');
          syncTuiHeader();
          if (tui) void tools.files.listWorkspaceFiles().then(files => tui?.setWorkspaceFiles(files)).catch(() => {});
        }
        return { aborted, hadError, text: outText };
      };

      const continueGoalIfIdle = async () => {
        if (controller || exiting || terminating || denied || tools.planMode) return;
        let consecutiveEmptyTurns = 0;
        for (let step = 1; ; step++) {
          if (exiting || terminating || denied || tools.planMode || goalContinuationDeferred) break;
          if (tui?.hasQueuedPrompts()) break;
          const g = store.session.goal;
          if (!g || g.status !== 'active') break;
          if (g.tokenBudget !== undefined && (g.tokensUsed ?? 0) >= g.tokenBudget) {
            g.status = 'budget_limited';
            g.updatedAt = new Date().toISOString();
            await store.save();
            syncTuiHeader();
            const summary = formatGoalStatusIndicator(g);
            if (tui) tui.setNotice(summary);
            else if (!jsonOutput) display(`\n[${summary}]\n`);
            break;
          }
          const contPrompt = buildGoalContinuationPrompt(g, !tools.planMode && !tools.readOnly);
          const res = await runTurn(contPrompt, { isGoalContinuation: true, continuationStep: step });
          if (res.aborted || res.hadError || denied) break;
          const afterGoal = store.session.goal;
          if (!afterGoal || afterGoal.status !== 'active') {
            if (afterGoal) {
              const summary = formatGoalStatusIndicator(afterGoal);
              if (tui) tui.setNotice(summary);
              if (!jsonOutput) display(`\n[${summary} — ${afterGoal.objective}]\n`);
            }
            break;
          }
          if (!res.text.trim()) {
            consecutiveEmptyTurns++;
            if (consecutiveEmptyTurns >= 3) {
              afterGoal.status = 'blocked';
              afterGoal.updatedAt = new Date().toISOString();
              await store.save();
              syncTuiHeader();
              const summary = formatGoalStatusIndicator(afterGoal);
              if (tui) tui.setNotice(summary);
              if (!jsonOutput) display(`\n[${summary} — stalled after 3 empty continuation turns]\n`);
              break;
            }
          } else {
            consecutiveEmptyTurns = 0;
          }
        }
      };

      const run = async (input: string) => {
        goalContinuationDeferred = false;
        const res = await runTurn(input);
        if (!res.aborted && !res.hadError && !denied && store.session.goal?.status === 'active') {
          await continueGoalIfIdle();
        }
      };

      const executeImmediateCommand = async (rawCmd: string): Promise<boolean> => {
        const cmdInput = rawCmd.trim();
        if (!cmdInput) return true;
        if (['/exit', '/quit'].includes(cmdInput)) {
          exiting = true;
          controller?.abort(new Error('Exited by user.'));
          rl?.close();
          tui?.close();
          return true;
        }
        if (cmdInput === '/clear') {
          tui?.clear();
          display(welcomeBanner(runtime.model, runtime.provider, runtime.baseUrl, runtime.effort, runtime.cwd, store.session.id, mode));
          return true;
        }
        if (cmdInput === '/help') {
          display(
            '\nCommands (can also run while agent is working):\n' +
            '  /auto [on|off], /yolo             Toggle Auto-Approve mode (or press Shift+Tab)\n' +
            '  /transcript [on|off]              Toggle Codex full-screen Transcript mode (or press Ctrl+T)\n' +
            '  /view [normal|verbose|transcript] Switch view: normal (hide thinking) | verbose | transcript\n' +
            '  /copy [last|code|<N>]             Copy last response, code block, or Nth response (or press Ctrl+O)\n' +
            '  /btw <question>, /side <question> Ask an ephemeral side question without modifying history\n' +
            '  /goal [--budget <N>] [<obj>|edit|pause|resume|complete|clear] Autonomous goal loop (Codex /goal)\n' +
            '  /recap                            Generate a live summary of current session progress\n' +
            '  /skills [list|<name>]             List or inspect SKILL.md skills (use $skill-name in prompt)\n' +
            '  /memories [list|add <text>|rm <id>|clear] Manage persistent user memories across sessions\n' +
            '  /presets [list|save <name> <prompt>|run <name>|rm <name>] Manage reusable prompt presets\n' +
            '  /mention <filepath>               Insert @filepath mention into the composer\n' +
            '  /cd <dir>, /pwd                   Change or show active workspace directory\n' +
            '  /mode [ask|auto|plan|readonly]    Switch approval mode (or press Shift+Tab)\n' +
            '  /setup, /config, /provider        Interactive provider, URL, key, model & effort setup\n' +
            '  /key                              Replace the stored API key (input hidden)\n' +
            '  /prompts [show <name>]            List custom prompt files; run one with /prompts:<name> [args]\n' +
            '  /model [id] [--effort=<level>]    Discover/pick model from /models or set model ID\n' +
            '  /effort [low|medium|high|xhigh|max|default] Switch reasoning effort (or Alt+./Alt+,)\n' +
            '  /plan [off|execute|<prompt>]      Enter/exit Plan Mode or execute approved plan\n' +
            '  /revert, /undo [code|msg|both] [N] Rewind last N turns & restore prompt to composer (or press Esc Esc)\n' +
            '  /checkpoints                      Interactive picker to rewind to any earlier turn checkpoint\n' +
            '  /todo [clear]                     View or clear the current task checklist\n' +
            '  /resume [latest|<number>|<id>]    Pick a session from this folder or all projects (←→ / Tab switch)\n' +
            '  /new [title]                      Start a fresh session in this workspace\n' +
            '  /delete [<id>|current]            Delete a saved session or current session\n' +
            '  /compact [focus instructions]     Summarize & archive earlier context history\n' +
            '  /diff                             Show git status, staged/unstaged diff & untracked files\n' +
            '  /review [branch <b?>|commit <c?>|<instructions>] Run code review on diff, branch, or commit\n' +
            '  /fork [title]                     Clone current session into a new forked session ID\n' +
            '  /rename <title>                   Set a title for the current session\n' +
            '  /export [filename.md]             Export session transcript to a Markdown file\n' +
            '  /init                             Generate or update AGENTS.md for this project\n' +
            '  /status, /debug-config, /warnings Show session status, config layers, or runtime warnings\n' +
            '  /mcp [list|check|import-codex|reload] Inspect, check, reload, or import Codex MCP servers\n' +
            '  /subagents [list|show <id>|<task>] List spawned subagents, view transcript, or run a subagent\n' +
            '  /tools                            List active tools in current mode\n' +
            '  /sessions                         List saved workspace sessions\n' +
            '  /doctor                           Check environment health\n' +
            '  /clear                            Clear screen (or press Ctrl+L)\n' +
            '  /exit                             Exit goon (or press Ctrl+D on empty input)\n' +
            '  !<command>                        Execute shell command directly & record in context\n' +
            '  @<filepath> / $<skill-name>       Attach workspace file or invoke SKILL.md in prompt\n'
          );
          return true;
        }
        if (cmdInput === '/copy' || cmdInput.startsWith('/copy ')) {
          const selector = cmdInput.slice(5).trim();
          const found = extractCopyableFromMessages(store.session.messages, selector);
          if (!found) {
            if (tui) tui.setNotice('Nothing to copy yet (no assistant output in this session).');
            else display('Nothing to copy yet (no assistant output in this session).\n');
            return true;
          }
          const ok = await copyTextToClipboard(found.text);
          const lines = found.text.split(/\r?\n/).length;
          const msg = ok
            ? `Copied ${found.label} (${found.text.length} chars · ${lines} lines) to clipboard.`
            : `Copied ${found.label} (${found.text.length} chars · ${lines} lines).`;
          if (tui) tui.setNotice(msg);
          display(`${msg}\n`);
          return true;
        }
        if (cmdInput === '/btw' || cmdInput.startsWith('/btw ') || cmdInput === '/side' || cmdInput.startsWith('/side ')) {
          const questionText = cmdInput.replace(/^\/(?:btw|side)\b/, '').trim();
          if (!questionText) {
            display('Usage: /btw <question> (asks a side question without modifying the session history)\n');
            return true;
          }
          if (tui) tui.setNotice('Answering side question (/btw)...');
          else display('\n[Answering side question (/btw)...]\n');
          void (async () => {
            try {
              const res = await harness.askSideQuestion(questionText, AbortSignal.timeout(runtime.timeoutMs));
              // A "◇" cell with indented lines renders as one quiet block in the TUI.
              display(`\n◇ Side answer: ${questionText}\n${res.text.split('\n').map(line => `  ${line}`).join('\n')}\n`);
              if (tui) tui.setNotice('Side question (/btw) answered (history unchanged).');
            } catch (error) {
              display(`\n[Side question failed: ${safeError(error, apiKey)}]\n`);
            }
          })();
          return true;
        }
        if (cmdInput === '/goal' || cmdInput.startsWith('/goal ')) {
          const arg = cmdInput.slice(5).trim();
          const lower = arg.toLowerCase();
          const now = new Date().toISOString();
          if (!arg) {
            const g = store.session.goal;
            if (!g) {
              display(`No active session goal.\n${GOAL_USAGE}\n`);
            } else {
              display(`\n${formatGoalSummaryLines(g, store.session.tokens)}\n`);
            }
            return true;
          }
          if (lower === 'clear' || lower === 'off' || lower === 'none') {
            if (!store.session.goal) {
              display('No active session goal.\n');
              return true;
            }
            store.session.goal = undefined;
            harness.addContextMessage({
              role: 'user',
              content: buildUserGoalContextMessage({ cleared: true }),
            });
            await store.save();
            syncTuiHeader();
            if (tui) tui.setNotice('Goal cleared.');
            display('Goal cleared.\n');
            return true;
          }
          if (lower === 'pause' || lower === 'resume' || lower === 'complete' || lower === 'done') {
            const g = store.session.goal;
            if (!g) {
              display(`No session goal set yet.\n${GOAL_USAGE}\n`);
              return true;
            }
            const nextStatus =
              lower === 'pause' ? 'paused' : lower === 'resume' ? 'active' : 'complete';
            if (nextStatus === 'active' && g.status === 'budget_limited' && g.tokenBudget !== undefined && (g.tokensUsed ?? 0) >= g.tokenBudget) {
              g.tokenBudget = undefined;
            }
            g.status = nextStatus;
            g.updatedAt = now;
            if (nextStatus === 'active') goalContinuationDeferred = false;
            harness.addContextMessage({
              role: 'user',
              content: buildUserGoalContextMessage({ status: nextStatus }),
            });
            await store.save();
            syncTuiHeader();
            const indicator = formatGoalStatusIndicator(g);
            if (tui) tui.setNotice(indicator);
            display(`\n${formatGoalSummaryLines(g, store.session.tokens)}\n`);
            if (nextStatus === 'active' && !controller) {
              if (mode === 'plan') {
                display('Note: Plan mode is active. Switch to ask or auto mode (/mode ask or /auto) to pursue the goal autonomously.\n');
              } else {
                await continueGoalIfIdle();
              }
            }
            return true;
          }
          if (lower === 'edit' || lower.startsWith('edit ')) {
            const g = store.session.goal;
            if (!g) {
              display(`No session goal set yet.\n${GOAL_USAGE}\n`);
              return true;
            }
            const editRest = arg.slice(4).trim();
            if (!editRest) {
              if (controller) {
                display('Usage while working: /goal edit <objective>\n');
                return true;
              }
              return false;
            }
            const parsedEdit = parseGoalDraftArgs(editRest);
            if ('error' in parsedEdit) {
              display(`${parsedEdit.error}\n`);
              return true;
            }
            if (parsedEdit.tokenBudget !== undefined) {
              g.tokenBudget = parsedEdit.tokenBudget;
              if (g.status === 'budget_limited' && (g.tokensUsed ?? 0) < parsedEdit.tokenBudget) {
                g.status = 'active';
              }
            }
            if (parsedEdit.objective) {
              g.objective = parsedEdit.objective;
            }
            g.updatedAt = now;
            harness.addContextMessage({
              role: 'user',
              content: buildUserGoalContextMessage({
                ...(parsedEdit.objective ? { objective: g.objective } : {}),
                status: g.status,
              }),
            });
            if (parsedEdit.objective) {
              harness.addContextMessage({
                role: 'user',
                content: buildGoalObjectiveUpdatedPrompt(g),
              });
            }
            await store.save();
            syncTuiHeader();
            if (tui) tui.setNotice(formatGoalStatusIndicator(g));
            display(`\n${formatGoalSummaryLines(g, store.session.tokens)}\n`);
            if (g.status === 'active' && !controller && mode !== 'plan') {
              goalContinuationDeferred = false;
              await continueGoalIfIdle();
            }
            return true;
          }
          const parsed = parseGoalDraftArgs(arg);
          if ('error' in parsed) {
            display(`${parsed.error}\n`);
            return true;
          }
          if (!parsed.objective) {
            if (parsed.tokenBudget !== undefined && store.session.goal) {
              const g = store.session.goal;
              g.tokenBudget = parsed.tokenBudget;
              if (g.status === 'budget_limited' && (g.tokensUsed ?? 0) < parsed.tokenBudget) {
                g.status = 'active';
              }
              g.updatedAt = now;
              await store.save();
              syncTuiHeader();
              if (tui) tui.setNotice(formatGoalStatusIndicator(g));
              display(`\n${formatGoalSummaryLines(g, store.session.tokens)}\n`);
              if (g.status === 'active' && !controller && mode !== 'plan') {
                goalContinuationDeferred = false;
                await continueGoalIfIdle();
              }
              return true;
            }
            display(`${GOAL_USAGE}\n`);
            return true;
          }
          const hadExistingGoal = Boolean(store.session.goal);
          store.session.goal = {
            goalId: `goal_${Date.now().toString(36)}`,
            objective: parsed.objective,
            status: 'active',
            createdAt: now,
            updatedAt: now,
            tokensAtStart: store.session.tokens,
            tokensUsed: 0,
            timeUsedSeconds: 0,
            ...(parsed.tokenBudget !== undefined ? { tokenBudget: parsed.tokenBudget } : {}),
          };
          goalContinuationDeferred = false;
          harness.addContextMessage({
            role: 'user',
            content: buildUserGoalContextMessage({ objective: parsed.objective, status: 'active' }),
          });
          if (controller && hadExistingGoal) {
            harness.addContextMessage({
              role: 'user',
              content: buildGoalObjectiveUpdatedPrompt(store.session.goal),
            });
          }
          await store.save();
          syncTuiHeader();
          if (tui) tui.setNotice(formatGoalStatusIndicator(store.session.goal));
          display(`\n${formatGoalSummaryLines(store.session.goal, store.session.tokens)}\n`);
          if (mode === 'plan') {
            display('Note: Plan mode is active. Switch to ask or auto mode (/mode ask or /auto) to pursue the goal autonomously.\n');
          } else if (!controller) {
            await continueGoalIfIdle();
          }
          return true;
        }
        if (cmdInput === '/pwd' || cmdInput === '/cwd') {
          display(`Workspace directory: ${runtime.cwd}\n`);
          return true;
        }
        if (cmdInput === '/prompts' || cmdInput === '/prompts list' || cmdInput.startsWith('/prompts show ')) {
          const { discoverPrompts } = await import('./prompts.js');
          const prompts = await discoverPrompts(runtime.cwd);
          if (cmdInput.startsWith('/prompts show ')) {
            const name = cmdInput.slice('/prompts show '.length).trim().replace(/^prompts:/, '');
            const found = prompts.find(p => p.name.toLowerCase() === name.toLowerCase());
            display(found ? `\n/prompts:${found.name} (${found.path}):\n${found.body.slice(0, 4000)}\n` : `Prompt "${name}" not found. Run /prompts to list them.\n`);
            return true;
          }
          if (!prompts.length) {
            display('No prompt files found. Create e.g. .goon/prompts/review.md or ~/.config/goon/prompts/review.md,\nthen run /prompts:review [args] ($ARGUMENTS and $1..$9 are substituted).\n');
          } else {
            display(`Custom prompts (${prompts.length}) — run with /prompts:<name> [args]:\n${prompts.map(p => `  /prompts:${p.name.padEnd(20)} [${p.scope}] ${p.description}`).join('\n')}\n`);
          }
          return true;
        }
        if (cmdInput === '/skills' || cmdInput.startsWith('/skills ')) {
          const { discoverSkills } = await import('./context.js');
          const { readFile } = await import('node:fs/promises');
          const arg = cmdInput.slice(7).trim();
          const skills = await discoverSkills(runtime.cwd);
          if (!arg || arg.toLowerCase() === 'list') {
            if (!skills.length) {
              display('No SKILL.md skills found in .agents/skills, .codex/skills, skills/, or ~/.config/goon/skills.\n');
            } else {
              const lines = skills.map(s => `  $${s.name.padEnd(22)} [${s.scope}] — ${s.description}`);
              display(`Available Skills (${skills.length}) — mention $<name> in any prompt to activate:\n${lines.join('\n')}\n`);
            }
            return true;
          }
          const cleanName = arg.replace(/^\$/, '').toLowerCase();
          const matched = skills.find(s => s.name.toLowerCase() === cleanName);
          if (!matched) {
            display(`Skill "${arg}" not found. Run /skills to list discovered skills.\n`);
            return true;
          }
          const content = await readFile(matched.path, 'utf8');
          display(`\nSkill $${matched.name} (${matched.path}):\n${content.slice(0, 4000)}\n`);
          return true;
        }
        if (cmdInput === '/memories' || cmdInput === '/memory' || cmdInput.startsWith('/memories ') || cmdInput.startsWith('/memory ')) {
          const { loadMemories, saveMemories, memoriesPath } = await import('./context.js');
          const rest = cmdInput.replace(/^\/memor(?:ies|y)\b/, '').trim();
          const entries = await loadMemories();
          if (!rest || rest.toLowerCase() === 'list') {
            if (!entries.length) {
              display(`No saved memories (${memoriesPath()}).\nUse "/memories add <note>" to save a persistent preference across sessions.\n`);
            } else {
              const lines = entries.map((m, i) => `  [${i + 1}] ${m.text} (${m.createdAt.slice(0, 10)})`);
              display(`Saved Memories (${entries.length}):\n${lines.join('\n')}\n`);
            }
            return true;
          }
          if (rest.toLowerCase() === 'clear' || rest.toLowerCase() === 'reset') {
            await saveMemories([]);
            display('Cleared all saved memories.\n');
            return true;
          }
          if (rest.toLowerCase().startsWith('rm ') || rest.toLowerCase().startsWith('remove ') || rest.toLowerCase().startsWith('del ')) {
            const target = rest.replace(/^(?:rm|remove|del)\s+/i, '').trim();
            const idx = Number(target) - 1;
            if (Number.isInteger(idx) && idx >= 0 && idx < entries.length) {
              const removed = entries.splice(idx, 1)[0];
              await saveMemories(entries);
              display(`Removed memory: "${removed.text}"\n`);
            } else {
              display(`Invalid memory index "${target}". Use /memories list.\n`);
            }
            return true;
          }
          const note = rest.toLowerCase().startsWith('add ') ? rest.slice(4).trim() : rest;
          entries.push({
            id: String(Date.now()),
            text: note,
            createdAt: new Date().toISOString(),
          });
          await saveMemories(entries);
          display(`Saved memory #${entries.length}: "${note}"\n`);
          return true;
        }
        if (cmdInput === '/presets' || cmdInput === '/preset' || cmdInput.startsWith('/presets ') || cmdInput.startsWith('/preset ')) {
          const { loadPresets, savePresets, presetsPath } = await import('./context.js');
          const rest = cmdInput.replace(/^\/presets?\b/, '').trim();
          const presets = await loadPresets();
          if (!rest || rest.toLowerCase() === 'list') {
            const list = Object.values(presets);
            if (!list.length) {
              display(`No saved prompt presets (${presetsPath()}).\nUse "/presets save <name> <prompt>" and "/presets run <name>".\n`);
            } else {
              const lines = list.map(p => `  ${p.name.padEnd(18)} — ${p.prompt.replace(/\s+/g, ' ').slice(0, 80)}`);
              display(`Saved Presets (${list.length}):\n${lines.join('\n')}\n`);
            }
            return true;
          }
          if (rest.toLowerCase().startsWith('save ') || rest.toLowerCase().startsWith('add ')) {
            const body = rest.replace(/^(?:save|add)\s+/i, '').trim();
            const sp = body.search(/\s/);
            if (sp <= 0) {
              display('Usage: /presets save <name> <prompt text>\n');
              return true;
            }
            const name = body.slice(0, sp).trim();
            const promptText = body.slice(sp + 1).trim();
            presets[name] = { name, prompt: promptText, updatedAt: new Date().toISOString() };
            await savePresets(presets);
            display(`Saved preset "${name}". Run it with: /presets run ${name}\n`);
            return true;
          }
          if (rest.toLowerCase().startsWith('rm ') || rest.toLowerCase().startsWith('remove ')) {
            const name = rest.replace(/^(?:rm|remove)\s+/i, '').trim();
            if (!presets[name]) {
              display(`Preset "${name}" not found.\n`);
              return true;
            }
            delete presets[name];
            await savePresets(presets);
            display(`Removed preset "${name}".\n`);
            return true;
          }
          // Let "/presets run <name>" fall through to main loop when idle so it executes `run(preset.prompt)`
          return false;
        }
        if (cmdInput === '/mention' || cmdInput.startsWith('/mention ')) {
          const target = cmdInput.slice(8).trim();
          if (!target) {
            display('Usage: /mention <filepath> (or type @ in the composer to autocomplete workspace files)\n');
            return true;
          }
          const formatted = target.includes(' ') ? `@"${target}" ` : `@${target} `;
          if (tui) {
            tui.insertText(formatted);
            tui.setNotice(`Attached ${formatted.trim()} into composer.`);
          } else {
            display(`Tip: Include ${formatted.trim()} in your next prompt to attach the file.\n`);
          }
          return true;
        }
        if (cmdInput === '/debug-config') {
          const { credentialsPath } = await import('./config.js');
          const { mcpConfigPath } = await import('./mcp.js');
          const { memoriesPath, presetsPath } = await import('./context.js');
          const caps = resolveModelCapabilities(runtime.model);
          display(
            `\nResolved Configuration & Layers:\n` +
            `  Config file      : ${configPath()}\n` +
            `  Credentials file : ${credentialsPath()} (keyEnv: ${runtime.keyEnv})\n` +
            `  MCP config file  : ${mcpConfigPath()}\n` +
            `  Memories file    : ${memoriesPath()}\n` +
            `  Presets file     : ${presetsPath()}\n` +
            `  Data directory   : ${runtime.dataDir}\n` +
            `  Workspace (cwd)  : ${runtime.cwd}\n` +
            `  Provider & URL   : ${runtime.provider} (${runtime.baseUrl})\n` +
            `  Model & Effort   : ${runtime.model} (effort: ${runtime.effort ?? 'default'})\n` +
            `  Capabilities     : ctx=${runtime.contextWindow}, maxOut=${runtime.maxOutputTokens}, vision=${caps.supportsVision !== false}, defaultEffort=${caps.defaultEffort ?? 'default'}\n` +
            `  Approval & View  : mode=${mode}, view=${viewMode}, showReasoning=${config.showReasoning}\n` +
            `  Registered Tools : ${Object.keys(tools.schemas()).length} active (${manager?.tools.length ?? 0} from MCP)\n`
          );
          return true;
        }
        if (cmdInput === '/warnings') {
          const warns = [...(manager?.warnings ?? [])];
          for (const st of manager?.statuses ?? []) {
            if (st.status === 'error' && st.error) {
              const msg = `MCP ${st.name}: ${st.error}`;
              if (!warns.includes(msg)) warns.push(msg);
            }
          }
          if (!warns.length) {
            display('No active runtime or MCP warnings.\n');
          } else {
            display(`Warnings (${warns.length}):\n${warns.map(w => `  • ${w}`).join('\n')}\n`);
          }
          return true;
        }
        if (cmdInput === '/auto' || cmdInput.startsWith('/auto ') || cmdInput === '/yolo') {
          const arg = cmdInput === '/yolo' ? 'on' : cmdInput.slice(5).trim().toLowerCase();
          let next: PermissionMode;
          if (!arg || arg === 'toggle') {
            next = mode === 'auto' ? 'ask' : 'auto';
          } else if (arg === 'on' || arg === 'true' || arg === '1' || arg === 'yes') {
            next = 'auto';
          } else if (arg === 'off' || arg === 'false' || arg === '0' || arg === 'ask') {
            next = 'ask';
          } else {
            display('Usage: /auto [on | off] (or /yolo)\n');
            return true;
          }
          applyPermissionMode(next);
          return true;
        }
        if (cmdInput === '/mode' || cmdInput.startsWith('/mode ')) {
          const arg = cmdInput.slice(5).trim().toLowerCase();
          let next: PermissionMode;
          if (!arg) {
            const order: PermissionMode[] = ['ask', 'auto', 'plan', 'readonly'];
            next = order[(order.indexOf(mode) + 1) % order.length];
          } else if (
            arg === 'ask' ||
            arg === 'plan' ||
            arg === 'auto' ||
            arg === 'auto-approve' ||
            arg === 'yolo' ||
            arg === 'readonly' ||
            arg === 'read-only'
          ) {
            next =
              arg === 'read-only'
                ? 'readonly'
                : arg === 'auto-approve' || arg === 'yolo'
                  ? 'auto'
                  : arg;
          } else {
            display('Usage: /mode [ask | auto | plan | readonly]\n');
            return true;
          }
          applyPermissionMode(next);
          return true;
        }
        if (cmdInput === '/transcript' || cmdInput.startsWith('/transcript ')) {
          const arg = cmdInput.slice(11).trim().toLowerCase();
          if (tui) {
            const nextOverlay =
              arg === 'on' || arg === 'true'
                ? true
                : arg === 'off' || arg === 'false'
                  ? false
                  : !tui.isTranscriptOverlay();
            tui.setTranscriptOverlay(nextOverlay);
            if (nextOverlay) {
              viewMode = 'transcript';
              tui.setViewMode('transcript');
            }
          } else {
            viewMode = viewMode === 'transcript' ? 'normal' : 'transcript';
            display(`Transcript mode: ${viewMode === 'transcript' ? 'ON' : 'OFF'}\n`);
          }
          return true;
        }
        if (cmdInput === '/view' || cmdInput.startsWith('/view ')) {
          const arg = cmdInput.slice(5).trim().toLowerCase();
          if (!arg) {
            const order: Array<'normal' | 'verbose' | 'transcript'> = ['normal', 'verbose', 'transcript'];
            viewMode = order[(order.indexOf(viewMode) + 1) % order.length];
          } else if (arg === 'normal' || arg === 'compact' || arg === 'default' || arg === 'hide') {
            viewMode = 'normal';
          } else if (arg === 'verbose' || arg === 'all' || arg === 'full' || arg === 'show') {
            viewMode = 'verbose';
          } else if (arg === 'transcript' || arg === 'tx') {
            viewMode = 'transcript';
          } else {
            display('Usage: /view [normal | verbose | transcript] (or press Ctrl+T for full-screen transcript)\n');
            return true;
          }
          config.viewMode = viewMode;
          config.showReasoning = viewMode !== 'normal';
          await saveConfig(config);
          tui?.setViewMode(viewMode);
          if (tui && arg === 'transcript') {
            tui.setTranscriptOverlay(true);
          } else if (tui && (arg === 'normal' || arg === 'verbose')) {
            tui.setTranscriptOverlay(false);
          }
          const desc =
            viewMode === 'transcript'
              ? 'transcript (show thinking, uncollapsed tool calls, full outputs & diffs · Ctrl+T full-screen)'
              : viewMode === 'verbose'
                ? 'verbose (show thinking & extended details)'
                : 'normal (hide thinking blocks)';
          if (tui) tui.setNotice(`View mode: ${desc}`);
          else display(`View mode set to: ${desc}\n`);
          return true;
        }
        if (cmdInput.startsWith('/effort ')) {
          const arg = cmdInput.slice(7).trim();
          const clean = arg.toLowerCase();
          const isClear = ['default', 'off', 'none', '0', '1'].includes(clean);
          const parsed = parseEffortInput(arg, undefined);
          if (!isClear && !parsed) {
            display(`Invalid effort "${arg}". Choose from: low, medium, high, xhigh, max, default.\n`);
            return true;
          }
          // Previously only the saved config changed; the running harness kept the old effort.
          await applyEffort(isClear ? undefined : parsed);
          return true;
        }
        if (cmdInput === '/todo' || cmdInput.startsWith('/todo ')) {
          const arg = cmdInput.slice(5).trim().toLowerCase();
          if (arg === 'clear' || arg === 'reset') {
            tools.todos = [];
            store.session.todos = [];
            await store.save();
            tui?.event({ type: 'todo', todos: [] });
            display('Cleared todo checklist.\n');
          } else {
            display(`\n${formatTodos(tools.todos)}\n`);
          }
          return true;
        }
        if (cmdInput === '/diff') {
          const { runCommand } = await import('./process.js');
          try {
            const [statusRes, diffRes, stagedRes] = await Promise.all([
              runCommand('git', ['status', '--short'], { cwd: runtime.cwd, signal: AbortSignal.timeout(10_000), timeoutMs: 10_000 }),
              runCommand('git', ['diff', '--no-ext-diff'], { cwd: runtime.cwd, signal: AbortSignal.timeout(10_000), timeoutMs: 10_000 }),
              runCommand('git', ['diff', '--cached', '--no-ext-diff'], { cwd: runtime.cwd, signal: AbortSignal.timeout(10_000), timeoutMs: 10_000 }),
            ]);
            const statusOut = statusRes.output.trim();
            const diffOut = [stagedRes.output.trim(), diffRes.output.trim()].filter(Boolean).join('\n\n');
            if (!statusOut && !diffOut) {
              display('Working tree is clean (no uncommitted changes).\n');
            } else {
              if (statusOut) display(`\nGit Status:\n${statusOut}\n`);
              if (diffOut) display(`\n${diffOut.slice(0, 16_000)}\n`);
            }
          } catch (error) {
            display(`Failed to run git diff: ${safeError(error, apiKey)}\n`);
          }
          return true;
        }
        if (cmdInput === '/mcp' || cmdInput === '/mcp list') {
          const { loadMcpConfig, mcpConfigPath } = await import('./mcp.js');
          const cfg = await loadMcpConfig();
          const entries = Object.entries(cfg.servers);
          const statusMap = new Map((manager?.statuses ?? []).map(s => [s.name, s]));
          if (!entries.length) {
            display(`No MCP servers configured in ${mcpConfigPath()}.\nTip: Run "/mcp import-codex" or "goon mcp add <name> -- <cmd> [args...]"\n`);
          } else {
            const lines = entries.map(([name, s]) => {
              const target = s.transport === 'stdio' ? `${s.command}${s.args.length ? ' ' + s.args.join(' ') : ''}` : s.url;
              const st = statusMap.get(name);
              const badge = s.disabled
                ? 'disabled'
                : st?.status === 'ok'
                  ? `connected (${st.toolCount} tools)`
                  : st?.status === 'error'
                    ? `error: ${st.error}`
                    : 'configured';
              return `  ${name}  [${s.transport}]  (${badge})  ${target}`;
            });
            display(`MCP Servers (${entries.length}):\n${lines.join('\n')}\nActive MCP tools: ${manager?.tools.length ?? 0}\n`);
          }
          return true;
        }
        if (cmdInput === '/subagents' || cmdInput === '/agents' || cmdInput === '/subagents list' || cmdInput.startsWith('/subagents show ') || cmdInput.startsWith('/agent show ')) {
          const rest = cmdInput.replace(/^\/(?:subagents|agents|agent)\s*/, '').trim();
          if (!rest || rest === 'list') {
            const list = await (subagents?.listSubagentSummaries() ?? Promise.resolve([]));
            if (!list.length) {
              display('  No subagents spawned in this session yet.\n  Tip: Use "/agent <task>" or ask the agent to spawn subagents (spawn_agent / delegate_task).\n');
            } else {
              const lines = list.map((s) =>
                `  ${s.nickname} (${s.id})  [${s.role}]  ${s.status}  (${s.messagesCount} msgs)  ${s.updatedAt}\n    Task: ${s.task}${s.summary ? `\n    Summary: ${s.summary}` : ''}`,
              );
              display(`Subagents (${list.length}):\n${lines.join('\n')}\n`);
            }
            return true;
          }
          if (rest.startsWith('show ')) {
            const target = rest.slice(5).trim();
            const transcript = await subagents?.readSubagentTranscript(target);
            display((transcript ?? `Subagent "${target}" not found.`) + '\n');
            return true;
          }
        }
        if (cmdInput === '/tools') {
          const names = Object.keys(tools.schemas());
          display(names.map(n => `  ${tools.isReadOnly(n) ? '● [read]' : '◆ [exec]'} ${n}`).join('\n') + '\n');
          return true;
        }
        if (cmdInput === '/sessions') {
          const list = await SessionStore.list(join(runtime.dataDir, 'sessions'));
          display((list.length ? list.map(s => {
            const cur = s.id === store.session.id ? ' [current]' : '';
            const label = s.title ? ` [${s.title}]` : s.preview ? ` "${s.preview}"` : '';
            return `  ${s.id.slice(0, 8)}${cur}  (${s.messagesCount ?? 0} msgs)  ${s.updatedAt}  ${s.model}${label}  ${s.cwd}`;
          }).join('\n') : '  No saved sessions.') + '\n');
          return true;
        }
        if (cmdInput === '/doctor') {
          const { runCommand } = await import('./process.js');
          const ok = async (cmd: string) => {
            try { return (await runCommand(cmd, ['--version'], { cwd: runtime.cwd, signal: AbortSignal.timeout(5000), timeoutMs: 5000 })).code === 0; }
            catch { return false; }
          };
          const [b, r, discovered] = await Promise.all([
            ok('bash'),
            ok('rg'),
            discoverModels(runtime.baseUrl, runtime.apiKey, runtime.provider),
          ]);
          const modelInList = discovered.some((m) => m.id === runtime.model);
          const gatewayStatus = discovered.length > 0
            ? `OK (${discovered.length} models: ${discovered.map((m) => m.id).join(', ')} · active model ${modelInList ? 'verified' : 'NOT listed'})`
            : 'Unreachable or no /models response';
          display(
            `Provider: ${runtime.provider} | Model: ${runtime.model} | Effort: ${runtime.effort ?? 'default'} | BaseURL: ${runtime.baseUrl}\n` +
            `Bash: ${b ? 'OK' : 'Missing'} | Ripgrep: ${r ? 'OK' : 'JS fallback'} | API Key: Configured\n` +
            `Gateway (/models): ${gatewayStatus}\n`,
          );
          return true;
        }
        if (cmdInput === '/status') {
          const usedEst = estimatedTokens(store.session.messages, '');
          const pct = Math.min(100, Math.round((usedEst / runtime.contextWindow) * 100));
          const goalLine = store.session.goal
            ? `Goal           [${goalStatusLabel(store.session.goal.status)}] ${store.session.goal.objective} (${formatGoalUsageSummary(store.session.goal)})\n`
            : '';
          display(
            `Session        ${store.session.id}${store.session.title ? ` (${store.session.title})` : ''}\n` +
            goalLine +
            `Provider       ${runtime.provider} (${runtime.baseUrl})\n` +
            `Model          ${runtime.model}\n` +
            `Effort         ${runtime.effort ?? 'default'}\n` +
            `Mode           ${mode === 'auto' ? 'auto-approve' : mode}\n` +
            `View           ${viewMode}\n` +
            `Workspace      ${runtime.cwd}\n` +
            `Checkpoints    ${store.session.checkpoints?.length ?? 0} turn(s)\n` +
            `Context usage  ~${usedEst} / ${runtime.contextWindow} tokens (${pct}%)\n` +
            `Compactions    ${store.session.compactions}\n` +
            `Session tokens ${store.session.tokens}\n` +
            `MCP tools      ${manager?.tools.length ?? 0}\n`
          );
          return true;
        }
        if (cmdInput.startsWith('/rename ')) {
          const newTitle = cmdInput.slice(8).trim();
          if (newTitle) {
            store.session.title = newTitle;
            await store.save();
            tui?.setTitle(`${runtime.model} · ${newTitle}`);
            display(`Session renamed to: ${newTitle}\n`);
          }
          return true;
        }
        if (cmdInput === '/export' || cmdInput.startsWith('/export ')) {
          const { writeFile } = await import('node:fs/promises');
          const outName = cmdInput.slice(7).trim() || `goon-session-${store.session.id.slice(0, 8)}.md`;
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
          return true;
        }
        if (cmdInput === '/fork' || cmdInput.startsWith('/fork ')) {
          const title = cmdInput.slice(5).trim() || undefined;
          const forkedId = await store.forkToNewFile(title);
          display(`Forked current session (${store.session.messages.length} messages) -> new session ID: ${forkedId}\nSwitch to it now with: /resume ${forkedId.slice(0, 8)} (or goon --resume ${forkedId.slice(0, 8)})\n`);
          return true;
        }
        if (cmdInput.startsWith('!')) {
          const { sanitizeBashCommand } = await import('./tools.js');
          const rawShell = cmdInput.slice(1).trim();
          if (!rawShell) {
            display('Usage: !<command> (runs a shell command directly in workspace and records output in session context)\n');
            return true;
          }
          const cmd = sanitizeBashCommand(rawShell);
          const { runCommand } = await import('./process.js');
          display(`\n$ ${cmd}\n`);
          // When idle, Ctrl+C cancels this command instead of quitting goon.
          const shellController = new AbortController();
          const ownsController = !controller;
          if (ownsController) controller = shellController;
          try {
            const res = await runCommand('bash', ['-c', cmd], { cwd: runtime.cwd, signal: AbortSignal.any([shellController.signal, AbortSignal.timeout(600_000)]), timeoutMs: 600_000 });
            display(`${res.output || '(no output)'}\n[Exit code: ${res.code}]\n`);
            harness.addContextMessage({
              role: 'user',
              content: `<user_shell_command>\n<command>${cmd}</command>\n<result>\nExit code: ${res.code}\n${res.output.slice(0, 8000)}\n</result>\n</user_shell_command>`,
            });
            await store.save();
          } catch (error) {
            display(`Shell command failed: ${safeError(error, apiKey)}\n`);
          } finally {
            if (ownsController && controller === shellController) controller = undefined;
          }
          return true;
        }
        return false;
      };

      if (tui) {
        tui.onImmediateCommand = async cmd => {
          const handled = await executeImmediateCommand(cmd);
          if (!handled) {
            // Command requires interactive prompts or turn completion (e.g. /revert, /model picker, /resume picker)
            if (cmd.startsWith('/revert') || cmd.startsWith('/undo') || cmd === '/checkpoints') {
              controller?.abort(new Error('Interrupted for revert.'));
            }
            // Previously only the notice was shown and the command was silently dropped.
            tui?.queuePrompt(cmd);
            tui?.setNotice(`Queued command "${cmd}" to run as soon as current step finishes.`);
          }
        };
      }

      if (prompt) await run(prompt);
      else {
        if (!rl && !tui) throw new Error('Interactive mode requires a TTY.');
        tui?.setStatus('Ready');
        let initialAutoCommand: string | undefined = launchResumePicker ? '/resume' : undefined;
        while (!exiting) {
          let input: string;
          if (initialAutoCommand) {
            input = initialAutoCommand;
            initialAutoCommand = undefined;
          } else {
            try { input = (await question(tui ? 'goon> ' : '\ngoon> ', undefined, true)).trim(); }
            catch { break; }
          }
          if (!input) continue;
          await applyPendingRebuild();
          if (await executeImmediateCommand(input)) {
            if (exiting) break;
            continue;
          }

          if (input.toLowerCase() === '/goal edit') {
            const g = store.session.goal;
            if (!g) {
              display(`No session goal set yet.\n${GOAL_USAGE}\n`);
              continue;
            }
            const promptBlock = `\n◇ Edit Goal Objective\n  Current: ${g.objective}\n`;
            const nextObj = tui
              ? (await tui.askTransient(promptBlock, 'New objective (Enter to cancel): ', undefined, { escHint: 'cancel' })).trim()
              : (display(promptBlock), (await question('New objective (Enter to cancel): ')).trim());
            if (!nextObj) {
              if (tui) tui.setNotice('Cancelled /goal edit.');
              else display('Cancelled /goal edit.\n');
              continue;
            }
            await executeImmediateCommand(`/goal edit ${nextObj}`);
            continue;
          }

          if (input === '/key') {
            const { saveCredential } = await import('./config.js');
            const entered = (tui
              ? await tui.question(`New API key for ${runtime.keyEnv} (hidden, Esc to cancel): `, undefined, false, { escCancels: true, mask: true }).catch(() => '')
              : await questionHidden(`New API key for ${runtime.keyEnv} (hidden): `)).trim();
            if (!entered) {
              display('API key unchanged.\n');
              continue;
            }
            await saveCredential(runtime.keyEnv, entered);
            if (process.env[runtime.keyEnv]?.trim()) {
              display(`Saved to credentials.json, but ${runtime.keyEnv} is set in the environment and takes precedence.\n`);
            }
            runtime = await resolveRuntime(config, runtime.cwd);
            apiKey = runtime.apiKey;
            await rebuildHarness();
            display(`API key for ${runtime.keyEnv} updated (${entered.slice(0, 4)}…${entered.slice(-2)}).\n`);
            continue;
          }
          if (input.startsWith('/prompts:')) {
            const { discoverPrompts, expandPromptTemplate } = await import('./prompts.js');
            const match = /^\/prompts:([A-Za-z0-9][A-Za-z0-9_.-]*)\s*([\s\S]*)$/.exec(input);
            const prompts = await discoverPrompts(runtime.cwd);
            const found = match ? prompts.find(p => p.name.toLowerCase() === match[1].toLowerCase()) : undefined;
            if (!found) {
              display(`Prompt "${match?.[1] ?? input.slice(9)}" not found. Run /prompts to list them.\n`);
              continue;
            }
            await run(expandPromptTemplate(found.body, match![2]));
            continue;
          }
          if (input === '/setup' || input === '/config' || input === '/provider') {
            try {
              config = await runSetupWizard(config, {
                write: text => display(text),
                // Esc cancels the whole wizard (nothing is persisted until the final step).
                ask: label => (tui ? tui.question(label, undefined, false, { escCancels: true }) : question(label)),
                askSecret: label => (tui ? tui.question(label, undefined, false, { escCancels: true, mask: true }) : questionHidden(label)),
              });
            } catch (error) {
              if (error instanceof InputCancelledError) {
                display('\n↺ Setup cancelled — configuration unchanged.\n');
                continue;
              }
              throw error;
            }
            if (interactive && !jsonOutput) {
              config.showReasoning = true;
            }
            runtime = await resolveRuntime(config, runtime.cwd);
            apiKey = runtime.apiKey;
            store.session.model = runtime.model;
            tui?.setTitle(`${runtime.model} · ${store.session.id.slice(0, 8)}`);
            tui?.setEffort(runtime.effort);
            syncTuiHeader();
            await rebuildHarness();
            continue;
          }
          if (input === '/model' || input.startsWith('/model ')) {
            const rawArgs = input.slice(6).trim();
            let nextModel: string | undefined;
            let selectedModel: DiscoveredModel | undefined;
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
              display(`\nCurrent model: ${runtime.model} (ctx: ${Math.round(runtime.contextWindow / 1024)}k · out: ${Math.round(runtime.maxOutputTokens / 1000)}k · effort: ${runtime.effort ?? 'default'} · ${runtime.provider} · ${runtime.baseUrl})\nProbing ${runtime.baseUrl}/models ...\n`);
              const discovered = await discoverModels(runtime.baseUrl, runtime.apiKey, runtime.provider);
              if (discovered.length > 0) {
                const shown = discovered.slice(0, 30);
                for (const [idx, m] of shown.entries()) {
                  display(`  [${idx + 1}] ${m.id}${formatModelBadge(m)}\n`);
                }
                const pick = (await question(`Select [1-${shown.length}] or enter model ID (Enter to keep ${runtime.model}): `)).trim();
                const pickIdx = Number(pick) - 1;
                if (pick && Number.isInteger(pickIdx) && pickIdx >= 0 && pickIdx < shown.length) {
                  selectedModel = shown[pickIdx];
                  nextModel = selectedModel.id;
                } else if (pick) {
                  nextModel = pick;
                  selectedModel = discovered.find((m) => m.id === pick);
                }
              } else {
                const typed = (await question(`Enter model ID (Enter to keep ${runtime.model}): `)).trim();
                if (typed) nextModel = typed;
              }
            }
            if (!nextModel && !effortChanged) continue;
            const targetModelId = nextModel ?? runtime.model;
            const caps = resolveModelCapabilities(targetModelId, selectedModel);
            const autoContextWindow = (nextModel ? caps.contextWindow : undefined) ?? config.contextWindow;
            const rawMaxOut = (nextModel ? caps.maxOutputTokens : undefined) ?? config.maxOutputTokens;
            const autoMaxOutputTokens = Math.min(rawMaxOut, Math.floor(autoContextWindow / 2));
            const finalEffort = effortChanged
              ? nextEffort
              : (nextModel && caps.defaultEffort ? caps.defaultEffort : config.effort);
            config = await loadConfig({
              ...config,
              ...(nextModel ? { model: nextModel, contextWindow: autoContextWindow, maxOutputTokens: autoMaxOutputTokens } : {}),
              ...(finalEffort ? { effort: finalEffort } : {}),
            });
            await saveConfig(config);
            runtime = await resolveRuntime(config, runtime.cwd);
            store.session.model = runtime.model;
            tui?.setTitle(`${runtime.model} · ${store.session.id.slice(0, 8)}`);
            tui?.setEffort(runtime.effort);
            syncTuiHeader();
            await rebuildHarness();
            display(`Active model: ${runtime.model} (ctx: ${Math.round(runtime.contextWindow / 1024)}k · out: ${Math.round(runtime.maxOutputTokens / 1000)}k · ${caps.supportsVision === false ? 'text-only' : 'vision'} · effort: ${runtime.effort ?? 'default'})\n`);
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
            syncTuiHeader();
            await rebuildHarness();
            display(`Reasoning effort set to: ${runtime.effort ?? 'default'}\n`);
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
          if (input === '/checkpoints' || input === '/revert list' || input === '/undo list' || input === '/revert pick' || input === '/undo pick') {
            const cps = store.session.checkpoints ?? [];
            if (!cps.length) {
              if (tui) tui.setNotice('↺ No previous turns to rewind in this session.');
              else display('No turn checkpoints recorded in this session yet.\n');
              continue;
            }
            if (tui) {
              const reversed = [...cps].reverse();
              const rows = reversed.slice(0, 15).map((cp, idx) => {
                const numStr = String(idx + 1).padStart(2, ' ');
                const fileDesc = cp.files.length ? `${cp.files.length} file(s): ${cp.files.map(f => f.path).join(', ')}` : 'no file changes';
                const promptPreview = cp.prompt.replace(/\s+/g, ' ').slice(0, 54);
                return `  ${numStr}. #${cp.id} [${cp.createdAt.slice(11, 19)}] "${promptPreview}" (${fileDesc})`;
              });
              const pickerBlock = `\n◇ Rewind to turn checkpoint · ${cps.length} available\n${rows.join('\n')}\n`;
              const choice = (await tui.askTransient(pickerBlock, `Rewind [1-${Math.min(15, reversed.length)}, or Enter to cancel]: `, undefined, { escHint: 'cancel' })).trim();
              const pickedNum = Number(choice);
              if (!choice || !Number.isInteger(pickedNum) || pickedNum < 1 || pickedNum > reversed.length) {
                continue;
              }
              input = `/revert ${pickedNum}`;
            } else {
              display(`\nTurn Checkpoints (${cps.length}):\n`);
              for (const cp of cps) {
                const fileDesc = cp.files.length ? `files: ${cp.files.map(f => f.path).join(', ')}` : 'no file changes';
                display(`  #${cp.id} [${cp.createdAt.slice(11, 19)}] "${cp.prompt.slice(0, 60)}" (${fileDesc})\n`);
              }
              continue;
            }
          }
          if (input === '/revert' || input.startsWith('/revert ') || input === '/undo' || input.startsWith('/undo ')) {
            const raw = input.replace(/^\/(?:revert|undo)\b/, '').trim();
            const parts = raw ? raw.split(/\s+/) : [];
            let scope: 'both' | 'code' | 'messages' = 'both';
            let steps = 1;
            let force = false;
            for (const p of parts) {
              const lp = p.toLowerCase();
              if (lp === '--force' || lp === '-f' || lp === 'force') force = true;
              else if (lp === 'code' || lp === 'files' || lp === 'file' || lp === 'source') scope = 'code';
              else if (lp === 'msg' || lp === 'messages' || lp === 'chat') scope = 'messages';
              else if (lp === 'both' || lp === 'all') scope = 'both';
              else {
                const n = Number(p);
                if (Number.isInteger(n) && n > 0) steps = n;
              }
            }
            try {
              const res = await harness.revert({ steps, scope, force });
              // Files edited after the agent's turn are never overwritten silently.
              const keptNote = res.conflictFiles.length
                ? `\n⚠ Kept ${res.conflictFiles.length} file(s) changed after that turn: ${res.conflictFiles.join(', ')} (use /revert ${steps} ${scope} --force to overwrite)\n`
                : '';
              const lostNote = res.unrecoverableFiles.length
                ? `\n⚠ Backup pruned (session size limit), not restored: ${res.unrecoverableFiles.join(', ')}\n`
                : '';
              if (keptNote || lostNote) display(keptNote + lostNote);
              if (tui) {
                tui.setTokens(store.session.tokens);
                syncTuiHeader(true);
                const changedFiles = [...res.restoredFiles, ...res.deletedFiles];
                if (scope === 'both' || scope === 'messages') {
                  const rewoundFromSnapshot = tui.rewindTurns(res.turnsReverted);
                  if (!rewoundFromSnapshot) {
                    tui.clear();
                    display(welcomeBanner(runtime.model, runtime.provider, runtime.baseUrl, runtime.effort, runtime.cwd, store.session.id, mode));
                    renderResumedHistory(false);
                  }
                  if (changedFiles.length > 0) {
                    display(`\n↺ Reverted ${res.turnsReverted} turn${res.turnsReverted > 1 ? 's' : ''} (files restored: ${changedFiles.join(', ')})\n`);
                  }
                  const restoredPrompt = res.revertedPrompts[0];
                  if (restoredPrompt) {
                    tui.setInput(restoredPrompt, true);
                  }
                  const fileNote = changedFiles.length > 0 ? ` · restored ${changedFiles.length} file(s)` : '';
                  tui.setNotice(
                    `↺ Rewound ${res.turnsReverted} turn${res.turnsReverted > 1 ? 's' : ''}${fileNote} — edit prompt below, Enter to resend, or Esc Esc to rewind further`,
                  );
                } else {
                  display(
                    `\n↺ Reverted file changes from ${res.turnsReverted} turn${res.turnsReverted > 1 ? 's' : ''}` +
                      (changedFiles.length ? ` (${changedFiles.join(', ')})\n` : ' (no files modified)\n'),
                  );
                }
              } else {
                display(
                  `\n↺ Reverted ${res.turnsReverted} turn(s) [scope: ${scope}] (${res.messagesRemoved} messages removed)\n` +
                  (res.restoredFiles.length ? `  Restored source files: ${res.restoredFiles.join(', ')}\n` : '') +
                  (res.deletedFiles.length ? `  Deleted newly created files: ${res.deletedFiles.join(', ')}\n` : '') +
                  (!res.restoredFiles.length && !res.deletedFiles.length && scope !== 'messages' ? `  No source files were modified in the reverted turn(s).\n` : '') +
                  (res.revertedPrompts.length ? `  Reverted prompt: "${res.revertedPrompts[0].slice(0, 80)}"\n` : '')
                );
              }
            } catch (error) {
              if (tui) {
                tui.setNotice(`↺ ${safeError(error, apiKey)}`);
              } else {
                display(`${safeError(error, apiKey)}\n`);
              }
            }
            continue;
          }
          if (input === '/new' || input.startsWith('/new ')) {
            const newTitle = input.slice(4).trim() || undefined;
            const sessionsRoot = join(runtime.dataDir, 'sessions');
            const newStore = await SessionStore.open(sessionsRoot, runtime);
            if (newTitle) {
              newStore.session.title = newTitle;
              await newStore.save();
            }
            const oldStore = store;
            store = newStore;
            await oldStore.discardIfEmpty();
            await oldStore.close();
            await rebuildHarness();
            tui?.clear();
            tui?.setTitle(store.session.title ? `${runtime.model} · ${store.session.title}` : `${runtime.model} · ${store.session.id.slice(0, 8)}`);
            tui?.setTokens(0);
            syncTuiHeader(true);
            tui?.event({ type: 'todo', todos: [] });
            display(welcomeBanner(runtime.model, runtime.provider, runtime.baseUrl, runtime.effort, runtime.cwd, store.session.id, mode));
            display(`Started new session: ${store.session.id}${store.session.title ? ` (${store.session.title})` : ''}\n`);
            continue;
          }
          if (input === '/resume' || input.startsWith('/resume ')) {
            const sessionsRoot = join(runtime.dataDir, 'sessions');
            const allList = await SessionStore.list(sessionsRoot);
            const resumable = (s: (typeof allList)[number]) => (s.messagesCount ?? 0) > 0 || Boolean(s.title);
            const projectSessions = allList.filter(resumable);
            const folderSessions = projectSessions.filter(s => sameWorkspace(s.cwd, runtime.cwd));
            let arg = input.slice(7).trim();
            let targetId: string | undefined;
            if (!arg) {
              if (!projectSessions.length) {
                display('No saved sessions yet. Sessions appear here after the first prompt.\n');
                continue;
              }
              if (tui) {
                // Two tabs so a developer who does not remember the folder can still find a session.
                const toItem = (s: (typeof allList)[number], showFolder: boolean) => ({
                  id: s.id,
                  label: `${s.id === store.session.id ? '(current) ' : ''}${s.title ? `${s.title}: ` : ''}${s.preview ?? '(no prompt yet)'}`,
                  meta: [showFolder ? folderLabel(s.cwd) : '', formatRelativeTime(s.updatedAt), `${s.messagesCount ?? 0} msgs`].filter(Boolean).join('   '),
                  detail: `${s.cwd}   ${s.id.slice(0, 8)}   ${s.model}`,
                  search: `${s.cwd} ${s.model} ${s.title ?? ''}`,
                });
                const picked = await tui.pick('Resume', [
                  { label: 'This folder', items: folderSessions.map(s => toItem(s, false)), empty: 'No sessions in this folder yet. Press → for all projects.' },
                  { label: 'All projects', items: projectSessions.map(s => toItem(s, true)) },
                ], { initialTab: folderSessions.length ? 0 : 1, action: 'resume' });
                if (!picked) {
                  tui.setNotice('Resume cancelled.');
                  continue;
                }
                targetId = picked.item.id;
              } else {
                // Readline mode: a numbered list; "a" switches between this folder and all projects.
                let showAll = folderSessions.length === 0;
                for (;;) {
                  const pool = showAll ? projectSessions : folderSessions;
                  const rows = pool.slice(0, 20).map((s, idx) =>
                    `  ${String(idx + 1).padStart(2)}  ${(s.title ?? s.preview ?? '(no prompt yet)').replace(/\s+/g, ' ').slice(0, 60)}   ${showAll ? `${folderLabel(s.cwd)}   ` : ''}${formatRelativeTime(s.updatedAt)}`);
                  display(`\n${showAll ? 'All projects' : 'This folder'} (${pool.length})\n${rows.join('\n') || '  (none)'}\n`);
                  const choice = (await question(`Resume [number, ID prefix, a = ${showAll ? 'this folder' : 'all projects'}, Enter = cancel]: `)).trim();
                  if (choice.toLowerCase() === 'a') { showAll = !showAll; continue; }
                  const n = Number(choice);
                  targetId = Number.isInteger(n) && n >= 1 && n <= pool.length ? pool[n - 1].id : choice || undefined;
                  break;
                }
                if (!targetId) {
                  display('Resume cancelled.\n');
                  continue;
                }
              }
              arg = targetId;
            }
            try {
              const candidates = folderSessions.length ? folderSessions : projectSessions;
              const num = Number(arg);
              const prefix = arg.toLowerCase();
              const targetMatch = Number.isInteger(num) && num >= 1 && num <= candidates.length && !/^[0-9a-f]{4,}$/i.test(arg)
                ? candidates[num - 1].id
                : arg === 'latest'
                  ? candidates.find(s => s.id !== store.session.id)?.id ?? candidates[0]?.id
                  : allList.find(s => s.id.toLowerCase() === prefix)?.id
                    ?? folderSessions.find(s => s.id.toLowerCase().startsWith(prefix))?.id
                    ?? allList.find(s => s.id.toLowerCase().startsWith(prefix))?.id
                    ?? arg;
              if (targetMatch === store.session.id) {
                display(`Already in session ${store.session.id.slice(0, 8)}.\n`);
                renderResumedHistory();
                continue;
              }
              // A session from another project brings its workspace with it.
              const target = allList.find(s => s.id === targetMatch);
              const previousRuntime = runtime;
              const switching = target !== undefined && !sameWorkspace(target.cwd, runtime.cwd);
              if (switching) {
                const { realpath } = await import('node:fs/promises');
                const folder = await realpath(target!.cwd).catch(() => undefined);
                if (!folder) {
                  display(`Cannot resume: the session's folder no longer exists (${target!.cwd}).\n`);
                  continue;
                }
                runtime = await resolveRuntime(config, folder);
              }
              let newStore;
              try {
                newStore = await SessionStore.open(sessionsRoot, runtime, targetMatch, store.session.id);
              } catch (error) {
                runtime = previousRuntime;
                throw error;
              }
              const oldStore = store;
              store = newStore;
              await oldStore.discardIfEmpty();
              await oldStore.close();
              await rebuildHarness(switching ? { reconnectMcp: true } : {});
              if (tui) {
                tui.clear();
                display(welcomeBanner(runtime.model, runtime.provider, runtime.baseUrl, runtime.effort, runtime.cwd, store.session.id, mode));
                if (switching) void tools.files.listWorkspaceFiles().then(files => tui?.setWorkspaceFiles(files)).catch(() => {});
              }
              tui?.setTitle(store.session.title ? `${runtime.model} · ${store.session.title}` : `${runtime.model} · ${store.session.id.slice(0, 8)}`);
              tui?.setTokens(store.session.tokens);
              syncTuiHeader(true);
              tui?.event({ type: 'todo', todos: store.session.todos ?? [] });
              renderResumedHistory();
              if (switching) display(`Workspace switched to ${runtime.cwd}\n`);
              if (!store.session.messages.length) {
                display(`Resumed session ${store.session.id.slice(0, 8)} (0 messages).\n`);
              }
            } catch (error) {
              display(`Failed to resume session: ${safeError(error, apiKey)}\n`);
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
          if (input === '/recap') {
            controller = new AbortController();
            tui?.setStatus('Running');
            try {
              const res = await harness.recap(controller.signal);
              display(`\n◇ Session recap\n${res.text.split('\n').map(line => `  ${line}`).join('\n')}\n`);
            } catch (error) {
              display(`Recap failed: ${safeError(error, apiKey)}\n`);
            } finally {
              controller = undefined;
              tui?.setStatus('Ready');
            }
            continue;
          }
          if (input === '/cd' || input.startsWith('/cd ')) {
            const targetDir = input.slice(3).trim();
            if (!targetDir) {
              display(`Current workspace directory: ${runtime.cwd}\nUsage: /cd <directory>\n`);
              continue;
            }
            const { stat, realpath } = await import('node:fs/promises');
            try {
              const candidate = resolve(runtime.cwd, targetDir);
              const st = await stat(candidate);
              if (!st.isDirectory()) {
                display(`Not a directory: ${candidate}\n`);
                continue;
              }
              const real = await realpath(candidate).catch(() => candidate);
              runtime = { ...runtime, cwd: real };
              store.session.cwd = real;
              await store.save();
              await rebuildHarness({ reconnectMcp: true });
              if (tui) {
                void tools.files.listWorkspaceFiles().then(files => tui?.setWorkspaceFiles(files)).catch(() => {});
                syncTuiHeader();
                tui.setNotice(`Changed directory to: ${real}`);
              }
              display(`Changed workspace directory to: ${real}\n`);
            } catch (error) {
              display(`Failed to change directory: ${safeError(error, apiKey)}\n`);
            }
            continue;
          }
          if (input === '/delete' || input.startsWith('/delete ')) {
            const target = input.slice(7).trim();
            const sessionsRoot = join(runtime.dataDir, 'sessions');
            if (!target || target.toLowerCase() === 'current') {
              const oldId = store.session.id;
              const oldStore = store;
              store = await SessionStore.open(sessionsRoot, runtime);
              await oldStore.close();
              await SessionStore.deleteSession(sessionsRoot, oldId);
              await rebuildHarness();
              tui?.clear();
              tui?.setTitle(`${runtime.model} · ${store.session.id.slice(0, 8)}`);
              tui?.setTokens(0);
              syncTuiHeader(true);
              tui?.event({ type: 'todo', todos: [] });
              display(welcomeBanner(runtime.model, runtime.provider, runtime.baseUrl, runtime.effort, runtime.cwd, store.session.id, mode));
              display(`Deleted session ${oldId.slice(0, 8)} and started fresh session ${store.session.id.slice(0, 8)}.\n`);
              continue;
            }
            try {
              if (store.session.id.toLowerCase().startsWith(target.toLowerCase())) {
                display('To delete the currently active session, run: /delete current\n');
                continue;
              }
              const deletedId = await SessionStore.deleteSession(sessionsRoot, target);
              display(`Deleted saved session: ${deletedId}\n`);
            } catch (error) {
              display(`Failed to delete session: ${safeError(error, apiKey)}\n`);
            }
            continue;
          }
          if (input.startsWith('/presets ') || input.startsWith('/preset ')) {
            const { loadPresets } = await import('./context.js');
            const rest = input.replace(/^\/presets?\s+/, '').trim();
            const name = rest.replace(/^run\s+/i, '').trim();
            const presets = await loadPresets();
            const matched = presets[name];
            if (!matched) {
              display(`Preset "${name}" not found. Use "/presets list" or "/presets save <name> <prompt>".\n`);
              continue;
            }
            display(`Running preset "${matched.name}":\n`);
            await run(matched.prompt);
            continue;
          }
          if (input === '/review' || input.startsWith('/review ')) {
            const extra = input.slice(7).trim();
            const branchMatch = /^branch\s+(\S+)(?:\s+([\s\S]+))?$/i.exec(extra);
            const commitMatch = /^commit\s+(\S+)(?:\s+([\s\S]+))?$/i.exec(extra);
            let reviewPrompt: string;
            if (branchMatch) {
              const baseBranch = branchMatch[1];
              const notes = branchMatch[2]?.trim();
              reviewPrompt =
                `Review the changes on the current branch compared to "${baseBranch}" (run \`git diff ${baseBranch}...HEAD\` and \`git log --oneline ${baseBranch}..HEAD\`). ` +
                `Focus on correctness, bugs, regressions, security issues, edge cases, and missing tests. Report findings ordered by severity with exact file and line references.` +
                (notes ? `\nAdditional review focus: ${notes}` : '');
            } else if (commitMatch) {
              const sha = commitMatch[1];
              const notes = commitMatch[2]?.trim();
              reviewPrompt =
                `Review commit "${sha}" in this repository (run \`git show ${sha}\`). ` +
                `Focus on correctness, bugs, regressions, security issues, edge cases, and missing tests. Report findings ordered by severity with exact file and line references.` +
                (notes ? `\nAdditional review focus: ${notes}` : '');
            } else {
              reviewPrompt =
                `Review the current changes in this repository (run git status and git diff or inspect modified files). ` +
                `Focus on correctness, bugs, security issues, edge cases, and missing tests. Report findings ordered by severity with exact file and line references.` +
                (extra ? `\nAdditional review focus: ${extra}` : '');
            }
            await run(reviewPrompt);
            continue;
          }
          if (input === '/init') {
            await run(
              'Inspect this workspace (package manifests, build/test scripts, directory structure, existing docs) and create or update AGENTS.md with concise, accurate commands for building, linting, typechecking, testing, and key architectural invariants.'
            );
            continue;
          }
          if (input.startsWith('/mcp ')) {
            const subArgs = input.slice(5).trim().split(/\s+/);
            const sub = (subArgs[0] ?? 'list').toLowerCase();
            const { loadMcpConfig, saveMcpConfig, importCodexMcpServers } = await import('./mcp.js');
            if (sub === 'reload' || sub === 'reconnect') {
              await rebuildHarness({ reconnectMcp: true });
              display(`Reloaded MCP servers (${manager?.tools.length ?? 0} MCP tools active).\n`);
              continue;
            }
            if (sub === 'import-codex' || sub === 'sync-codex') {
              try {
                const res = await importCodexMcpServers(undefined, subArgs.slice(1).length ? subArgs.slice(1) : undefined);
                if (!res.imported.length) {
                  display(`No matching MCP servers found in ~/.codex/config.toml (target: ${res.path}).\n`);
                } else {
                  display(`Imported ${res.imported.length} MCP server(s) into ${res.path}: ${res.imported.join(', ')}\n`);
                  await rebuildHarness({ reconnectMcp: true });
                  display(`Active MCP tools: ${manager?.tools.length ?? 0}\n`);
                }
              } catch (error) {
                display(`MCP import failed: ${safeError(error, apiKey)}\n`);
              }
              continue;
            }
            if (sub === 'remove' || sub === 'rm') {
              const target = subArgs[1];
              if (!target) {
                display('Usage: /mcp remove <name>\n');
                continue;
              }
              const cfg = await loadMcpConfig();
              if (!cfg.servers[target]) {
                display(`MCP server "${target}" not found.\n`);
                continue;
              }
              delete cfg.servers[target];
              await saveMcpConfig(cfg);
              await rebuildHarness({ reconnectMcp: true });
              display(`Removed MCP server "${target}" (${manager?.tools.length ?? 0} MCP tools active).\n`);
              continue;
            }
            if (sub === 'check') {
              await rebuildHarness({ reconnectMcp: true });
              for (const st of manager?.statuses ?? []) {
                display(`  ${st.name}: ${st.status}${st.status === 'ok' ? ` (${st.toolCount} tools)` : st.error ? ` — ${st.error}` : ''}\n`);
              }
              continue;
            }
            display('Usage: /mcp [list | check | reload | import-codex [names...] | remove <name>]\n');
            continue;
          }
          if (input === '/subagents' || input === '/agents' || input === '/agent' || input.startsWith('/subagents ') || input.startsWith('/agents ') || input.startsWith('/agent ')) {
            const rest = input.replace(/^\/(?:subagents|agents|agent)\s*/, '').trim();
            if (!rest || rest === 'list') {
              const list = await (subagents?.listSubagentSummaries() ?? Promise.resolve([]));
              if (!list.length) {
                display('  No subagents spawned in this session yet.\n  Tip: Use "/agent <task>" or ask the agent to spawn subagents (spawn_agent / delegate_task).\n');
              } else {
                const lines = list.map((s) =>
                  `  ${s.nickname} (${s.id})  [${s.role}]  ${s.status}  (${s.messagesCount} msgs)  ${s.updatedAt}\n    Task: ${s.task}${s.summary ? `\n    Summary: ${s.summary}` : ''}`,
                );
                display(`Subagents (${list.length}):\n${lines.join('\n')}\n`);
              }
              continue;
            }
            if (rest.startsWith('show ')) {
              const target = rest.slice(5).trim();
              const transcript = await subagents?.readSubagentTranscript(target);
              display((transcript ?? `Subagent "${target}" not found.`) + '\n');
              continue;
            }
            await run(`Use a subagent (spawn_agent + wait_agent, or delegate_task / delegate_edit as appropriate) to execute the following task and report the result:\n\n${rest}`);
            continue;
          }
          await run(input);
        }
      }

    } finally {
      process.removeListener('SIGINT', stop); process.removeListener('SIGTERM', terminate); process.removeListener('message', onIpcSignal);
      try { tui?.close(); rl?.close(); await subagents?.closeAll(); await manager?.close(); }
      finally {
        await store.close();
        if (process.connected) {
          try { process.disconnect(); } catch { /* ignore */ }
        }
      }
    }
  } catch (error) {
    const message = cleanTerminal(safeError(error, apiKey));
    if (jsonOutput) stdout.write(JSON.stringify({ type: 'error', message }) + '\n');
    else stderr.write(`goon: ${message}\n`);
    if (process.exitCode !== 130 && process.exitCode !== 143) process.exitCode = 1;
  }
}

