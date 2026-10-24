/**
 * Execution policy (v0.5, P12): network default-deny and shared process limits.
 *
 * The policy is resolved once from config + CLI flags and attached to the tool registry, the
 * workflow runner and the MCP manager. It is a *gate*, not an OS sandbox: allowed operations
 * still run with the user's privileges.
 *
 * Network classification is deliberately conservative and fail-closed:
 * - `deny` blocks every operation that looks like it can open a socket.
 * - `allowlist` allows only exact host matches; when a host cannot be determined (a git remote
 *   name, a package-manager install, `npx`, ...) the operation is denied.
 * - Hosts are compared exactly (no suffix or wildcard matches) after lowercasing and stripping
 *   a trailing dot.
 */
import type { Config } from './config.js';
import { baseName, peelWrappers, scanShell, tokenize } from './risk.js';
import type { McpServerConfig } from './mcp.js';
import {
  DEFAULT_COMMAND_TIMEOUT_MS,
  DEFAULT_HARD_OUTPUT_LIMIT_BYTES,
  DEFAULT_OUTPUT_LIMIT_BYTES,
  MAX_COMMAND_TIMEOUT_MS,
} from './process.js';

/** MCP results are text; 32 KiB matches the pre-policy character bound for ASCII output. */
export const DEFAULT_MCP_MAX_OUTPUT_BYTES = 32 * 1024;

export interface ExecutionLimits {
  defaultToolTimeoutMs: number;
  maxToolTimeoutMs: number;
  maxOutputBytes: number;
  hardOutputBytes: number;
  mcpMaxOutputBytes: number;
}

export interface ExecutionPolicy {
  network: { mode: 'deny' | 'allow' | 'allowlist'; hosts: ReadonlySet<string> };
  limits: ExecutionLimits;
  allowNetForServer(serverConfig: Pick<McpServerConfig, 'allowNet'> | undefined): boolean;
}

export interface NetworkAssessment {
  /** True when the input contains a segment that may open a network connection. */
  network: boolean;
  /** Normalized hosts discovered in the input (may be empty). */
  hosts: string[];
  /** True when at least one network segment has no determinable host (fail closed). */
  unknown: boolean;
}

export const NO_NETWORK: NetworkAssessment = Object.freeze({ network: false, hosts: [], unknown: false });

/** Client binaries whose primary purpose is opening a socket. `ssh-keygen` stays local. */
const NETWORK_CLIENTS = new Set([
  'curl', 'wget', 'ssh', 'scp', 'sftp', 'rsync', 'nc', 'ncat', 'netcat', 'ftp', 'telnet',
  'socat', 'tnftp', 'lftp',
]);

/** Package managers and the subcommands that always reach the network. */
const PACKAGE_MANAGERS = new Set([
  'npm', 'pnpm', 'yarn', 'bun', 'pip', 'pip3', 'uv', 'poetry', 'cargo', 'go', 'gem',
  'composer', 'apt', 'apt-get', 'pkg', 'brew', 'dnf', 'pacman', 'apk', 'yum',
]);
const PACKAGE_NETWORK_SUBCOMMANDS = new Set([
  'install', 'i', 'add', 'ci', 'update', 'upgrade', 'up', 'get', 'remove', 'rm', 'uninstall',
  'un', 'sync', 'publish', 'audit', 'view', 'info', 'outdated', 'fetch', 'download', 'exec',
  'x', 'dlx',
]);
const NETWORK_PACKAGE_RUNNERS = new Set(['npx', 'pnpx', 'bunx']);

const NETWORK_GIT_SUBCOMMANDS = new Set([
  'push', 'pull', 'fetch', 'clone', 'ls-remote', 'submodule', 'request-pull', 'archive',
]);

/** Flags whose next token is a value, never a host/URL. Used only for host extraction. */
const CURL_VALUE_FLAGS = new Set([
  '-o', '--output', '-H', '--header', '-d', '--data', '--data-raw', '--data-binary',
  '--data-urlencode', '-X', '--request', '-u', '--user', '-A', '--user-agent', '-e', '--referer',
  '-b', '--cookie', '-c', '--cookie-jar', '-F', '--form', '-T', '--upload-file', '-x', '--proxy',
  '-m', '--max-time', '--connect-timeout', '-r', '--range', '-w', '--write-out', '-K', '--config',
  '-E', '--cert', '--key', '--cacert', '--capath', '--resolve', '--connect-to', '--retry',
  '--retry-delay', '--retry-max-time', '--url', '--unix-socket', '--interface', '--limit-rate',
  '--max-filesize', '--speed-limit', '--speed-time', '--max-redirs', '--local-port', '--dns-servers',
  '--expect100-timeout', '--happy-eyeballs-timeout-ms', '--keepalive-time',
]);
const WGET_VALUE_FLAGS = new Set([
  '-O', '--output-document', '-o', '--output-file', '-P', '--directory-prefix', '-U', '--user-agent',
  '-T', '--timeout', '-t', '--tries', '-w', '--wait', '-e', '--execute', '-a', '--append-output',
  '-i', '--input-file', '-B', '--base', '-D', '--domains', '-H', '--header', '-Y', '--proxy',
  '-Q', '--quota', '-l', '--level', '-A', '--accept', '-R', '--reject', '--post-data',
  '--post-file', '--method', '--body-data', '--body-file',
]);
const SSH_VALUE_FLAGS = new Set([
  '-p', '-i', '-l', '-o', '-F', '-J', '-L', '-R', '-D', '-W', '-b', '-c', '-E', '-I', '-m', '-O',
  '-Q', '-S', '-w', '-e', '-B',
]);
const COPY_VALUE_FLAGS = new Set(['-e', '--rsh', '-i', '--identity-file', '-P', '-p', '-o', '-F', '-S', '-l']);
const NETCAT_VALUE_FLAGS = new Set(['-p', '-s', '-w', '-i', '-x', '-X', '-c', '-q', '-T', '-O', '-m', '-I', '-e']);
const FTP_VALUE_FLAGS = new Set(['-p', '-P', '-u', '-w', '-o', '-N', '-t', '-r', '-g']);
const GIT_VALUE_FLAGS = new Set(['-C', '-c', '--git-dir', '--work-tree', '--namespace', '--super-prefix']);
/** Flags after the git subcommand that take a value and are not remote URLs. */
const GIT_SUB_VALUE_FLAGS = new Set([
  '--depth', '--branch', '-b', '--origin', '-o', '--config', '--reference', '--separate-git-dir',
  '--jobs', '-j', '--filter', '--shallow-since', '--shallow-exclude', '--repo', '--server-option',
]);
const PACKAGE_VALUE_FLAGS = new Set([
  '--prefix', '--registry', '--cache', '--userconfig', '--globalconfig', '--cwd', '--dir',
  '--workspace', '-w', '--filter', '--loglevel', '--target', '--index-url', '-i', '--extra-index-url',
  '--python', '--root', '--manifest-path', '--features', '-F', '--package', '-p', '--toolchain',
  '--config', '--repository', '--source', '--build-dir',
]);

/**
 * Options that redirect a connection to a different host than the visible URL operand. In
 * allowlist mode these are treated as an unknown target and denied (fail closed): allowing
 * `example.com` must not silently allow a proxy/`--resolve`/`ProxyCommand` to `evil.test`.
 */
const CURL_REDIRECT_FLAGS = new Set([
  '-x', '--proxy', '--preproxy', '--socks4', '--socks4a', '--socks5', '--socks5-hostname',
  '--resolve', '--connect-to', '--unix-socket', '--abstract-unix-socket', '--doh-url',
  '-K', '--config',
]);
const WGET_REDIRECT_FLAGS = new Set(['-e', '--execute']);
const SSH_REDIRECT_FLAGS = new Set(['-o', '-J', '-L', '-R', '-D', '-W', '-F', '-w']);
const COPY_REDIRECT_FLAGS = new Set(['-o', '-F', '-J', '-S']);
const RSYNC_REDIRECT_FLAGS = new Set(['-e', '--rsh', '--rsync-path']);
const NETCAT_REDIRECT_FLAGS = new Set(['-x', '--proxy', '-e', '--exec', '-c', '--sh-exec']);

/** True when any token is exactly one of `flags` (inline `--flag=value` included). */
function hasFlag(tokens: string[], flags: ReadonlySet<string>): boolean {
  return tokens.some(token => {
    if (!isFlag(token)) return false;
    const eq = token.indexOf('=');
    return flags.has(eq > 0 ? token.slice(0, eq) : token);
  });
}

/** True for attached short flags such as `-xhttp://proxy` or `-oProxyCommand=...`. */
function hasAttachedShortFlag(tokens: string[], short: string): boolean {
  return tokens.some(token => token.length > short.length && token.startsWith(short));
}

function gitHasRedirect(rest: string[]): boolean {
  for (const token of rest) {
    if (!isFlag(token)) continue;
    // `-c` is a config override (proxies, sshCommand); `-C` is only a working directory.
    // Both global options and per-subcommand options (`git clone -c key=value`) count.
    if (token === '-c' || token.startsWith('-c')) return true;
    const lower = token.toLowerCase();
    if (lower.startsWith('--config') || lower.startsWith('--exec-path')
      || lower.startsWith('--upload-pack') || lower.startsWith('--receive-pack')) return true;
  }
  return false;
}

/** Lowercase a host and strip a port; IPv6 brackets stay; wildcards are never accepted. */
export function normalizeNetworkHost(raw: string): string | undefined {
  let host = raw.trim().toLowerCase();
  if (!host) return undefined;
  if (host.includes('://')) {
    try { host = new URL(host).hostname; } catch { return undefined; }
  }
  if (host.startsWith('[')) {
    const end = host.indexOf(']');
    if (end > 0) host = host.slice(0, end + 1);
  } else if ((host.match(/:/g) ?? []).length >= 2) {
    // Bare IPv6 literal; normalize to the bracketed form returned by URL.hostname.
    host = `[${host}]`;
  } else {
    const colon = host.lastIndexOf(':');
    if (colon > 0 && /^\d+$/.test(host.slice(colon + 1))) host = host.slice(0, colon);
  }
  host = host.replace(/\.+$/, '');
  if (!host || host.includes('*')) return undefined;
  return host;
}

/**
 * Parse `--allow-net-hosts` / config `allowNetHosts`: comma-separated strings, arrays, or a mix.
 * Entries may be hosts or full URLs. Invalid/wildcard entries are dropped (fail closed).
 */
export function parseNetworkHostList(raw: unknown): string[] {
  const hosts: string[] = [];
  const add = (value: unknown): void => {
    if (typeof value === 'string') {
      for (const part of value.split(',')) {
        const host = normalizeNetworkHost(part);
        if (host) hosts.push(host);
      }
      return;
    }
    if (Array.isArray(value)) for (const item of value) add(item);
  };
  add(raw);
  return [...new Set(hosts)];
}

export function resolveExecutionPolicy(config: Config, flags: Record<string, unknown> = {}): ExecutionPolicy {
  const flagAllowNet = flags['allow-net'] === true || flags.allowNet === true;
  const flagHosts = parseNetworkHostList(flags['allow-net-hosts'] ?? flags.allowNetHosts);
  const configHosts = parseNetworkHostList((config as Pick<Config, 'allowNetHosts'>).allowNetHosts);
  const hosts = new Set([...configHosts, ...flagHosts]);
  const allowAll = flagAllowNet || (config as Pick<Config, 'allowNet'>).allowNet === true;
  const mode: ExecutionPolicy['network']['mode'] = allowAll ? 'allow' : hosts.size ? 'allowlist' : 'deny';
  const limits: ExecutionLimits = Object.freeze({
    defaultToolTimeoutMs: DEFAULT_COMMAND_TIMEOUT_MS,
    maxToolTimeoutMs: MAX_COMMAND_TIMEOUT_MS,
    maxOutputBytes: DEFAULT_OUTPUT_LIMIT_BYTES,
    hardOutputBytes: DEFAULT_HARD_OUTPUT_LIMIT_BYTES,
    mcpMaxOutputBytes: DEFAULT_MCP_MAX_OUTPUT_BYTES,
  });
  return Object.freeze({
    network: Object.freeze({ mode, hosts }),
    limits,
    allowNetForServer: (serverConfig: Pick<McpServerConfig, 'allowNet'> | undefined) =>
      allowAll || serverConfig?.allowNet === true,
  });
}

function isFlag(token: string): boolean {
  return token.startsWith('-') && token !== '-';
}

/** Positional tokens, skipping flags and the values of known value-taking flags. */
function positionals(tokens: string[], valueFlags: ReadonlySet<string>): string[] {
  const out: string[] = [];
  for (let i = 0; i < tokens.length; i++) {
    const token = tokens[i];
    if (token === '--') { out.push(...tokens.slice(i + 1)); break; }
    if (!isFlag(token)) { out.push(token); continue; }
    if (valueFlags.has(token) && i + 1 < tokens.length) i++;
  }
  return out;
}

/** Inline `--flag=value` values (curl --url=, pip --index-url=, ...). */
function inlineValues(tokens: string[], names: ReadonlySet<string>): string[] {
  const out: string[] = [];
  for (const token of tokens) {
    const eq = token.indexOf('=');
    if (eq > 0 && names.has(token.slice(0, eq))) out.push(token.slice(eq + 1));
  }
  return out;
}

function trimPath(value: string): string {
  const slash = value.indexOf('/');
  const backslash = value.indexOf('\\');
  const cut = slash < 0 ? backslash : backslash < 0 ? slash : Math.min(slash, backslash);
  return cut > 0 ? value.slice(0, cut) : value;
}

/** Extract a host from a URL, scp/rsync remote (`user@host:path`), or a bare host[:port]. */
function hostFromToken(token: string, allowBareHost = true): string | undefined {
  const value = token.trim();
  if (!value || isFlag(value)) return undefined;
  // Windows drive path (C:\dir), not a remote.
  if (value.length >= 3 && value[1] === ':' && (value[2] === '\\' || value[2] === '/')) return undefined;
  if (value.includes('://')) {
    try { return normalizeNetworkHost(new URL(value).hostname); } catch { return undefined; }
  }
  const at = value.lastIndexOf('@');
  const remotePart = at >= 0 ? value.slice(at + 1) : value;
  let host: string | undefined;
  if (remotePart.startsWith('[')) {
    const end = remotePart.indexOf(']');
    if (end > 0) host = normalizeNetworkHost(remotePart.slice(0, end + 1));
  } else {
    const colon = remotePart.indexOf(':');
    if (colon > 0) host = normalizeNetworkHost(remotePart.slice(0, colon));
  }
  // A single letter is a drive/relative name, never a remote host.
  if (host && host.length > 1) return host;
  if (!allowBareHost) return undefined;
  return normalizeNetworkHost(trimPath(at >= 0 ? remotePart : value));
}

function addHost(hosts: string[], host: string | undefined): void {
  if (host && !hosts.includes(host)) hosts.push(host);
}

/** The git subcommand, skipping global flags and their values. */
function gitSubcommand(rest: string[]): { sub?: string; args: string[] } {
  let index = 0;
  while (index < rest.length) {
    const token = rest[index];
    if (token === '--') {
      const sub = rest[index + 1];
      return { sub: sub?.toLowerCase(), args: rest.slice(index + 2) };
    }
    if (isFlag(token)) {
      if (GIT_VALUE_FLAGS.has(token) && !token.includes('=')) index++;
      index++;
      continue;
    }
    return { sub: token.toLowerCase(), args: rest.slice(index + 1) };
  }
  return { args: [] };
}

function packageSubcommand(rest: string[]): string | undefined {
  for (let i = 0; i < rest.length; i++) {
    const token = rest[i];
    if (token === '--') return rest[i + 1]?.toLowerCase();
    if (isFlag(token)) {
      if (PACKAGE_VALUE_FLAGS.has(token) && i + 1 < rest.length) i++;
      continue;
    }
    return token.toLowerCase();
  }
  return undefined;
}

function analyzeSegment(tokens: string[]): NetworkAssessment {
  const peeled = peelWrappers(tokens);
  if (!peeled.tokens.length) return NO_NETWORK;
  const cmd = baseName(peeled.tokens[0]);
  const rest = peeled.tokens.slice(1);

  // BusyBox/ToyBox dispatch applets (`busybox wget ...`); inspect the applet command line.
  if ((cmd === 'busybox' || cmd === 'toybox') && rest.length) return analyzeSegment(rest);

  if (NETWORK_PACKAGE_RUNNERS.has(cmd)) {
    // The package name is not a host, and the runner may contact a registry plus script hosts.
    return { network: true, hosts: [], unknown: true };
  }
  if (PACKAGE_MANAGERS.has(cmd)) {
    const sub = packageSubcommand(rest);
    if (!sub || !PACKAGE_NETWORK_SUBCOMMANDS.has(sub)) return NO_NETWORK;
    // Registries and transitive download hosts are not knowable from the arguments.
    return { network: true, hosts: [], unknown: true };
  }
  if (cmd === 'git') {
    const { sub, args } = gitSubcommand(rest);
    if (!sub || !NETWORK_GIT_SUBCOMMANDS.has(sub)) return NO_NETWORK;
    const hosts: string[] = [];
    for (const token of [...positionals(args, GIT_SUB_VALUE_FLAGS), ...inlineValues(args, new Set(['--remote', '--upload-pack', '--receive-pack']))]) {
      addHost(hosts, hostFromToken(token, false));
    }
    // `git push origin main` names a configured remote; its URL/host is not knowable here.
    return { network: true, hosts, unknown: hosts.length === 0 || gitHasRedirect(rest) };
  }
  if (NETWORK_CLIENTS.has(cmd)) {
    const hosts: string[] = [];
    if (cmd === 'curl' || cmd === 'wget') {
      const valueFlags = cmd === 'curl' ? CURL_VALUE_FLAGS : WGET_VALUE_FLAGS;
      const redirectFlags = cmd === 'curl' ? CURL_REDIRECT_FLAGS : WGET_REDIRECT_FLAGS;
      const args = [...positionals(rest, valueFlags), ...inlineValues(rest, new Set(['--url']))];
      for (const token of args) addHost(hosts, hostFromToken(token));
      // A client invocation without a host is still a network client (`curl --version`); it is
      // blocked in deny/allowlist mode rather than guessed to be safe.
      const redirect = hasFlag(rest, redirectFlags)
        || (cmd === 'curl' && hasAttachedShortFlag(rest, '-x'))
        || (cmd === 'wget' && hasAttachedShortFlag(rest, '-e'));
      return { network: true, hosts, unknown: hosts.length === 0 || redirect };
    }
    if (cmd === 'ssh') {
      const args = positionals(rest, SSH_VALUE_FLAGS);
      addHost(hosts, hostFromToken(args[0] ?? '', true));
      const redirect = hasFlag(rest, SSH_REDIRECT_FLAGS) || hasAttachedShortFlag(rest, '-o');
      return { network: true, hosts, unknown: hosts.length === 0 || redirect };
    }
    if (cmd === 'scp' || cmd === 'sftp' || cmd === 'rsync') {
      for (const token of positionals(rest, COPY_VALUE_FLAGS)) addHost(hosts, hostFromToken(token, cmd === 'sftp'));
      const redirect = cmd === 'rsync'
        ? hasFlag(rest, RSYNC_REDIRECT_FLAGS) || hasAttachedShortFlag(rest, '-e')
        : hasFlag(rest, COPY_REDIRECT_FLAGS) || hasAttachedShortFlag(rest, '-o');
      // Local-only copies stay local; a transport override (proxy/custom ssh) makes the real
      // destination unknowable.
      if (!hosts.length && !redirect) return NO_NETWORK;
      return { network: true, hosts, unknown: redirect };
    }
    if (cmd === 'socat') {
      let redirect = false;
      for (const token of rest) {
        if (isFlag(token)) continue;
        const lower = token.toLowerCase();
        if (lower.startsWith('exec:') || lower.startsWith('system:') || lower.startsWith('program:')) {
          redirect = true;
          continue;
        }
        const upper = token.toUpperCase();
        const address = upper.match(/^(?:TCP|TCP4|TCP6|UDP|UDP4|UDP6|OPENSSL|SSL|SOCKS4|SOCKS4A|SOCKS5)(?:4|5|6)?:([^:]+):/);
        addHost(hosts, normalizeNetworkHost(address?.[1] ?? ''));
      }
      return { network: true, hosts, unknown: hosts.length === 0 || redirect };
    }
    // ftp/telnet/nc: first positional is the host. A bare `nc -l` is a listening socket.
    const args = positionals(rest, cmd === 'nc' || cmd === 'ncat' || cmd === 'netcat' ? NETCAT_VALUE_FLAGS : FTP_VALUE_FLAGS);
    addHost(hosts, hostFromToken(args[0] ?? '', true));
    const redirect = (cmd === 'nc' || cmd === 'ncat' || cmd === 'netcat')
      && (hasFlag(rest, NETCAT_REDIRECT_FLAGS) || hasAttachedShortFlag(rest, '-x'));
    return { network: true, hosts, unknown: hosts.length === 0 || redirect };
  }
  return NO_NETWORK;
}

/**
 * Classify the network behavior of a whole input line. Wrappers (`sudo`, `env`, `time`,
 * `timeout`, `xargs`, ...) are peeled exactly like the risk classifier, and every compound
 * segment is inspected.
 */
export function assessCommandNetwork(input: string): NetworkAssessment {
  if (typeof input !== 'string' || !input.trim()) return NO_NETWORK;
  const scan = scanShell(input);
  const hosts: string[] = [];
  let network = false;
  let unknown = false;
  for (const segment of scan.segments) {
    const tokens = tokenize(segment);
    if (!tokens.length) continue;
    const result = analyzeSegment(tokens);
    if (!result.network) continue;
    network = true;
    unknown ||= result.unknown;
    for (const host of result.hosts) addHost(hosts, host);
  }
  return network ? { network, hosts, unknown } : NO_NETWORK;
}

/** First determinable host in the input line, if any. */
export function extractNetworkHost(input: string): string | undefined {
  return assessCommandNetwork(input).hosts[0];
}

/** True when the execution policy permits this input line. Non-network operations always pass. */
export function isNetworkAllowedForCommand(policy: ExecutionPolicy, input: string): boolean {
  const assessment = assessCommandNetwork(input);
  if (!assessment.network) return true;
  if (policy.network.mode === 'allow') return true;
  if (policy.network.mode === 'deny') return false;
  return !assessment.unknown
    && assessment.hosts.length > 0
    && assessment.hosts.every(host => policy.network.hosts.has(host));
}

/** User-facing denial text. Does not echo the input (it may contain credentials). */
export function networkDeniedMessage(input: string): string {
  const assessment = assessCommandNetwork(input);
  if (assessment.unknown) {
    return 'Permission denied: network access is disabled and the target host cannot be verified. '
      + 'Use --allow-net to permit all network access.';
  }
  if (assessment.hosts.length) {
    return 'Permission denied: network access is blocked by the execution policy for this host. '
      + 'Use --allow-net or --allow-net-hosts=HOST to permit it.';
  }
  return 'Permission denied: network access is blocked by the execution policy. '
    + 'Use --allow-net or --allow-net-hosts=HOST to permit it.';
}
