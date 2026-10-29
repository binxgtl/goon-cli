/**
 * Risk classification for tools and shell commands (v0.4, P04).
 *
 * This module is intentionally dependency-light: it only type-imports the MCP config so the tool
 * registry can import it without a runtime cycle. It never executes anything and never mutates
 * configuration; it is a gate used before the approval callback runs.
 *
 * Invariants:
 * - `critical` is denied before any approval callback is consulted.
 * - `high` is never auto-approved, never remembered by the session tool allowlist and never added
 *   to the exact/prefix bash allowlists.
 * - Classification is heuristic. Approval is not an OS sandbox; see docs/PLAN-yolo-port.md.
 */
import type { McpServerConfig } from './mcp.js';

export type RiskClass = 'low' | 'medium' | 'high' | 'critical';

const RISK_LEVEL: Record<RiskClass, number> = { low: 0, medium: 1, high: 2, critical: 3 };

export function riskLevel(risk: RiskClass): number {
  return RISK_LEVEL[risk] ?? 0;
}

function maxRisk(a: RiskClass, b: RiskClass): RiskClass {
  return riskLevel(a) >= riskLevel(b) ? a : b;
}

/** Wrappers that only change environment/privilege/process plumbing before the real command. */
const WRAPPERS = new Set([
  'sudo', 'doas', 'env', 'time', 'nice', 'nohup', 'stdbuf', 'timeout', 'command', 'builtin',
  'xargs', 'setsid', 'exec',
]);
/** Wrappers that run a program chosen by their arguments: never below high. */
const HIGH_FLOOR_WRAPPERS = new Set(['sudo', 'doas', 'xargs']);
const ENV_ASSIGNMENT_RE = /^[A-Za-z_][A-Za-z0-9_]*=/;

/**
 * Programs that run another program given somewhere in their arguments (`watch rm -rf /`,
 * `flock /tmp/l rm -rf /`, `strace -f rm -rf /`). Their option grammars differ too much to peel
 * reliably, so the classifier inspects every argument suffix (and every whitespace-containing
 * argument as a nested command line) and never rates them below high.
 */
const GENERIC_RUNNERS = new Set([
  'watch', 'ionice', 'strace', 'ltrace', 'chroot', 'flock', 'unbuffer', 'chrt', 'taskset',
  'numactl', 'nsenter', 'unshare', 'firejail', 'proot', 'fakeroot', 'fakechroot', 'caffeinate',
  'systemd-run', 'runuser', 'su', 'tsu', 'script', 'parallel', 'entr', 'cpulimit', 'faketime',
  'torsocks', 'proxychains', 'proxychains4', 'catchsegv', 'valgrind', 'gdb', 'expect',
]);
/** True for programs that run another command named anywhere in their arguments. */
export function isGenericRunner(cmd: string): boolean {
  return GENERIC_RUNNERS.has(cmd) || cmd === 'trap';
}
/** Multi-call binaries whose first argument is the real applet (`busybox wget`, `toybox rm`). */
const MULTICALL_BINARIES = new Set(['busybox', 'toybox']);
/** Interpreters whose inline-code flags execute arbitrary code. */
const INLINE_CODE_FLAGS: Record<string, readonly string[]> = {
  perl: ['-e', '-E'], ruby: ['-e'], php: ['-r'], lua: ['-e'], luajit: ['-e'], osascript: ['-e'],
  rscript: ['-e'], tclsh: ['-c'], deno: ['eval'], bun: ['-e', '--eval'],
};

const SHELL_INTERPRETERS = new Set([
  'bash', 'sh', 'zsh', 'fish', 'dash', 'ksh', 'mksh', 'tcsh', 'csh', 'pwsh', 'powershell',
  'powershell.exe', 'cmd', 'cmd.exe',
]);

const NETWORK_COMMANDS = new Set([
  'curl', 'wget', 'ssh', 'scp', 'sftp', 'rsync', 'nc', 'ncat', 'netcat', 'ftp', 'telnet',
  'socat', 'ssh-keygen', 'tnftp', 'lftp',
]);

const DESTRUCTIVE_COMMANDS = new Set([
  'dd', 'shred', 'wipefs', 'fdisk', 'parted', 'mkswap',
]);

const PROCESS_CONTROL_COMMANDS = new Set(['kill', 'pkill', 'killall', 'taskkill']);
const PRIVILEGED_COMMANDS = new Set([
  'docker', 'podman', 'kubectl', 'systemctl', 'service', 'launchctl', 'mount', 'umount',
]);

const FILE_WRITE_COMMANDS = new Set([
  'cp', 'mv', 'ln', 'chmod', 'chown', 'chgrp', 'truncate', 'install', 'mkdir', 'touch',
  'rmdir', 'unlink', 'tee', 'patch', 'unzip', 'zip', 'gzip', 'gunzip', 'tar', 'cpio', 'rsync',
]);

/** Read-only text/process inspection. Any extra flags are still inspected by the caller. */
const SAFE_READ_COMMANDS = new Set([
  'cat', 'ls', 'grep', 'rg', 'head', 'tail', 'wc', 'sort', 'uniq', 'cut', 'tr', 'column',
  'diff', 'cmp', 'comm', 'pwd', 'echo', 'printf', 'which', 'type', 'file', 'stat', 'du', 'df',
  'tree', 'date', 'basename', 'dirname', 'realpath', 'readlink', 'test', 'true', 'false',
  'printenv', 'id', 'whoami', 'uname', 'hostname', 'ps', 'top', 'free', 'nproc', 'sleep',
  'jq', 'yq', 'nl', 'od', 'xxd', 'md5sum', 'sha1sum', 'sha256sum', 'cd', 'export', 'set',
]);

/** Local build/typecheck/test entry points that are safe to auto-approve in auto mode. */
const SAFE_SCRIPT_NAMES = new Set([
  'test', 'tests', 'build', 'lint', 'typecheck', 'check', 'test:unit', 'test:ci', 'ci',
]);

const PACKAGE_MANAGERS = new Set([
  'npm', 'pnpm', 'yarn', 'bun', 'pip', 'pip3', 'uv', 'poetry', 'cargo', 'go', 'gem',
  'composer', 'apt', 'apt-get', 'pkg', 'brew', 'dnf', 'pacman', 'apk', 'yum', 'npx', 'pnpx',
  'bunx',
]);
const INSTALL_SUBCOMMANDS = new Set([
  'install', 'i', 'add', 'ci', 'update', 'upgrade', 'up', 'get', 'remove', 'rm', 'uninstall',
  'un', 'sync', 'publish', 'audit', 'view', 'info', 'outdated', 'fetch', 'download', 'exec',
  'x', 'dlx',
]);
const SAFE_GIT_SUBCOMMANDS = new Set([
  'status', 'diff', 'log', 'show', 'blame', 'shortlog', 'describe', 'rev-parse', 'ls-files',
  'ls-tree', 'grep', 'whatchanged', 'reflog', 'cat-file', 'hash-object', 'version', 'help',
]);
const NETWORK_GIT_SUBCOMMANDS = new Set([
  'push', 'pull', 'fetch', 'clone', 'ls-remote', 'submodule', 'request-pull', 'archive',
]);
const WRITE_GIT_SUBCOMMANDS = new Set([
  'add', 'commit', 'mv', 'rm', 'restore', 'reset', 'checkout', 'switch', 'stash', 'merge',
  'rebase', 'cherry-pick', 'revert', 'tag', 'apply', 'am', 'clean', 'config', 'init', 'gc',
  'prune', 'notes', 'worktree',
]);
const DANGEROUS_RM_TARGETS = new Set([
  '/', '/*', '~', '~/', '~/*', '$HOME', '${HOME}', '$HOME/*', '${HOME}/*', '/home', '/home/',
  '/home/*', '/root', '/root/', '/root/*', '/Users', '/Users/', '/Users/*', '..', '../', '../..',
  '../../..', '.', './',
]);

export interface ShellScan {
  segments: string[];
  /** True when the command contains code execution the parser cannot classify safely. */
  criticalEscape: boolean;
  /** True when any unquoted redirection is present. */
  redirection: boolean;
  /** True when the command contains an unquoted pipe inside a quoted program (awk etc.). */
  pipeInProgram: boolean;
}

/**
 * Quote-aware scan: split compound commands into segments while ignoring separators inside
 * single/double quotes. `$(...)`, backticks and process substitution are treated as critical
 * unless they are inside single quotes (literal text).
 */
export function scanShell(command: string): ShellScan {
  const segments: string[] = [];
  let current = '';
  let single = false;
  let double = false;
  let criticalEscape = false;
  let redirection = false;
  const push = () => {
    const trimmed = current.trim();
    if (trimmed) segments.push(trimmed);
    current = '';
  };
  for (let i = 0; i < command.length; i++) {
    const ch = command[i];
    if (single) {
      current += ch;
      if (ch === "'") single = false;
      continue;
    }
    if (ch === '\\') {
      const next = command[i + 1];
      // `\<newline>` is a line continuation: drop both so the newline neither splits the
      // segment nor hides inside a token (`rm \<newline>-rf /` must classify like `rm -rf /`).
      if (next === '\n') { i++; continue; }
      if (next === '\r' && command[i + 2] === '\n') { i += 2; continue; }
      current += ch;
      if (next !== undefined) current += command[++i];
      continue;
    }
    if (double) {
      current += ch;
      if (ch === '"') { double = false; continue; }
      if (ch === '`') criticalEscape = true;
      if (ch === '$' && command[i + 1] === '(' && command[i + 2] !== '(') criticalEscape = true;
      continue;
    }
    if (ch === "'") { single = true; current += ch; continue; }
    if (ch === '"') { double = true; current += ch; continue; }
    if (ch === '`') { criticalEscape = true; current += ch; continue; }
    if (ch === '$' && command[i + 1] === '(' && command[i + 2] === '(') {
      // Arithmetic expansion: parentheses are part of the expression, not command separators.
      let depth = 0;
      current += ch;
      i++;
      while (i < command.length) {
        const inner = command[i];
        current += inner;
        if (inner === '(') depth++;
        else if (inner === ')') {
          depth--;
          if (depth === 0 && command[i + 1] === ')') { current += command[++i]; break; }
        }
        i++;
      }
      continue;
    }
    if (ch === '$' && command[i + 1] === '(') { criticalEscape = true; current += ch; continue; }
    if ((ch === '<' || ch === '>') && command[i + 1] === '(') { criticalEscape = true; current += ch; continue; }
    if (ch === '>' || ch === '<') { redirection = true; current += ch; continue; }
    if (ch === ';' || ch === '\n' || ch === '\r' || ch === '(' || ch === ')' || ch === '{' || ch === '}') {
      push();
      continue;
    }
    if (ch === '&' || ch === '|') {
      push();
      if (command[i + 1] === ch) i++;
      continue;
    }
    current += ch;
  }
  push();
  return { segments, criticalEscape, redirection, pipeInProgram: false };
}

/** Hex digit value 0-15, or -1 when the character is not a hex digit. */
function hexDigit(ch: string): number {
  const code = ch.charCodeAt(0);
  if (code >= 48 && code <= 57) return code - 48;
  if (code >= 97 && code <= 102) return code - 87;
  if (code >= 65 && code <= 70) return code - 55;
  return -1;
}

/** Octal digit value 0-7, or -1 when the character is not an octal digit. */
function octalDigit(ch: string): number {
  const code = ch.charCodeAt(0);
  return code >= 48 && code <= 55 ? code - 48 : -1;
}

/**
 * Bash ANSI-C quoting (`$'...'`) decodes escapes, so a destructive flag can hide as
 * `$'\x2drf'`. Tokenizing without decoding would let `rm $'\x2drf' /` classify as a
 * plain medium rm. `decodeAnsiEscape` mirrors the bash rules for the sequences that
 * can produce flag characters: `\xHH`, `\uHHHH`, octal `\NNN`, and the simple table;
 * unknown escapes drop the backslash, like bash.
 * Returns the decoded char and how many source characters it consumed after the backslash.
 */
function decodeAnsiEscape(segment: string, afterBackslash: number): { char: string; consumed: number } {
  const lead = segment[afterBackslash];
  if (lead === undefined || lead === "'") return { char: '\\', consumed: 0 };
  if (lead === 'x' || lead === 'u') {
    const maxDigits = lead === 'x' ? 2 : 4;
    let value = 0;
    let digits = 0;
    let index = afterBackslash + 1;
    while (digits < maxDigits && index < segment.length) {
      const digit = hexDigit(segment[index]!);
      if (digit < 0) break;
      value = value * 16 + digit;
      digits++;
      index++;
    }
    if (digits > 0 && value <= 0x10ffff) return { char: String.fromCodePoint(value), consumed: index - afterBackslash - 1 };
    // `\x`/`\u` without usable digits keeps the letter itself.
    return { char: lead, consumed: 1 };
  }
  if (octalDigit(lead) >= 0) {
    let value = 0;
    let digits = 0;
    let index = afterBackslash;
    while (digits < 3 && index < segment.length) {
      const digit = octalDigit(segment[index]!);
      if (digit < 0) break;
      value = value * 8 + digit;
      digits++;
      index++;
    }
    if (value <= 0x10ffff) return { char: String.fromCodePoint(value), consumed: index - afterBackslash };
    return { char: lead, consumed: 1 };
  }
  const simple: Record<string, string> = { n: '\n', r: '\r', t: '\t', a: '\x07', b: '\b', e: '\x1b', E: '\x1b', f: '\f', v: '\v', '\\': '\\', "'": "'", '"': '"', '?': '?' };
  const mapped = simple[lead];
  return { char: mapped !== undefined ? mapped : lead, consumed: 1 };
}

/** Quote-aware word split. Quotes are removed; backslash escapes are kept as literal chars. */
export function tokenize(segment: string): string[] {
  const tokens: string[] = [];
  let current = '';
  let single = false;
  let double = false;
  let ansiC = false;
  const push = () => {
    if (current) tokens.push(current);
    current = '';
  };
  for (let i = 0; i < segment.length; i++) {
    const ch = segment[i];
    if (ansiC) {
      if (ch === "'") { ansiC = false; continue; }
      if (ch === '\\') {
        const decoded = decodeAnsiEscape(segment, i + 1);
        current += decoded.char;
        i += decoded.consumed;
        continue;
      }
      current += ch;
      continue;
    }
    if (single) {
      if (ch === "'") single = false;
      else current += ch;
      continue;
    }
    if (double) {
      if (ch === '"') double = false;
      else if (ch === '\\' && i + 1 < segment.length) current += segment[++i];
      else current += ch;
      continue;
    }
    if (ch === '$' && segment[i + 1] === "'") { ansiC = true; i++; continue; }
    if (ch === "'") { single = true; continue; }
    if (ch === '"') { double = true; continue; }
    if (ch === '\\' && i + 1 < segment.length) { current += segment[++i]; continue; }
    if (/\s/.test(ch)) { push(); continue; }
    current += ch;
  }
  push();
  return tokens;
}

export function baseName(token: string): string {
  const base = (token.replace(/\\/g, '/').split('/').pop() ?? token).toLowerCase();
  // Windows resolves `curl.exe`/`npm.cmd` to the same command as `curl`/`npm`; classify the
  // underlying name so critical/network rules cannot be bypassed by an executable suffix.
  return base.replace(/\.(?:exe|cmd|bat|com)$/, '');
}

function stripEnvAssignments(tokens: string[]): string[] {
  let index = 0;
  while (index < tokens.length && ENV_ASSIGNMENT_RE.test(tokens[index])) index++;
  return tokens.slice(index);
}

function skipFlags(tokens: string[], valueFlags: ReadonlySet<string> = new Set()): string[] {
  let index = 0;
  while (index < tokens.length) {
    const token = tokens[index];
    if (token === '--') return tokens.slice(index + 1);
    if (!token.startsWith('-') || token === '-') return tokens.slice(index);
    index++;
    if (valueFlags.has(token) && index < tokens.length) index++;
  }
  return [];
}

/** Peels environment assignments and command wrappers, returning the inner argv and a risk floor. */
export function peelWrappers(input: string[]): { tokens: string[]; floor: RiskClass } {
  let tokens = input.slice();
  let floor: RiskClass = 'low';
  for (let guard = 0; guard < 20; guard++) {
    tokens = stripEnvAssignments(tokens);
    if (!tokens.length) return { tokens, floor: maxRisk(floor, 'medium') };
    const wrapper = baseName(tokens[0]);
    if (!WRAPPERS.has(wrapper)) return { tokens, floor };
    if (HIGH_FLOOR_WRAPPERS.has(wrapper)) floor = maxRisk(floor, 'high');
    const rest = tokens.slice(1);
    if (wrapper === 'env') {
      // `env -S 'cmd args'` re-splits a string; treat the string as a new command.
      const splitIndex = rest.findIndex(t => t === '-S' || t === '--split-string' || t.startsWith('--split-string='));
      if (splitIndex >= 0) {
        const raw = rest[splitIndex].includes('=') ? rest[splitIndex].slice(rest[splitIndex].indexOf('=') + 1) : rest[splitIndex + 1];
        if (!raw) return { tokens: [], floor: maxRisk(floor, 'high') };
        tokens = tokenize(raw);
        floor = maxRisk(floor, 'high');
        continue;
      }
      tokens = skipFlags(rest, new Set(['-u', '--unset', '-C', '--chdir']));
      continue;
    }
    if (wrapper === 'sudo' || wrapper === 'doas') {
      tokens = skipFlags(rest, new Set(['-u', '-g', '-p', '-c', '-h', '-r', '-t', '-U', '--user', '--group']));
      continue;
    }
    if (wrapper === 'timeout') {
      const afterFlags = skipFlags(rest, new Set(['-s', '--signal', '-k', '--kill-after']));
      // The first remaining token is the duration; the command follows it.
      tokens = afterFlags.slice(1);
      continue;
    }
    if (wrapper === 'nice') {
      tokens = skipFlags(rest, new Set(['-n', '--adjustment']));
      continue;
    }
    if (wrapper === 'stdbuf') {
      tokens = skipFlags(rest);
      continue;
    }
    if (wrapper === 'xargs') {
      tokens = skipFlags(rest, new Set(['-n', '-I', '-d', '-E', '-L', '-P', '-s', '-a']));
      continue;
    }
    if (wrapper === 'time') {
      tokens = skipFlags(rest, new Set(['-f', '--format', '-o', '--output']));
      continue;
    }
    if (wrapper === 'command' || wrapper === 'builtin') {
      tokens = rest.filter((t, i) => i > 0 || !t.startsWith('-'));
      continue;
    }
    // nohup, setsid, exec
    tokens = skipFlags(rest);
  }
  return { tokens: [], floor: maxRisk(floor, 'high') };
}

function hasFlag(tokens: string[], short: string, long: string): boolean {
  return tokens.some(token => {
    if (token === long) return true;
    if (!token.startsWith('-') || token.startsWith('--')) return false;
    return token.slice(1).includes(short);
  });
}


function isDangerousPath(target: string): boolean {
  const clean = target.trim().replace(/^["']|["']$/g, '');
  if (DANGEROUS_RM_TARGETS.has(clean)) return true;
  // `//`, `/./`, `//*` and friends still point at the filesystem root.
  const normalized = clean.replace(/\/+/g, '/').replace(/\/\.(?=\/|$)/g, '');
  return normalized === '/' || normalized === '/*';
}

function classifyRm(rest: string[]): RiskClass {
  const flags = rest.filter(t => t.startsWith('-'));
  const recursive = flags.some(t => t === '--recursive' || (!t.startsWith('--') && /[rR]/.test(t)));
  const force = flags.some(t => t === '--force' || (!t.startsWith('--') && t.slice(1).includes('f')));
  const noPreserveRoot = flags.some(t => t === '--no-preserve-root');
  const targets = rest.filter(t => !t.startsWith('-'));
  if (noPreserveRoot || (targets.some(isDangerousPath) && (recursive || force))) return 'critical';
  if (targets.some(t => t === '..' || t.startsWith('../')) && (recursive || force)) return 'critical';
  if (recursive || force) return 'high';
  return 'medium';
}

function classifyGit(rest: string[]): RiskClass {
  let index = 0;
  let floor: RiskClass = 'low';
  while (index < rest.length) {
    const token = rest[index];
    if (token === '-C' || token === '--git-dir' || token === '--work-tree' || token === '-c') {
      floor = maxRisk(floor, token === '-c' ? 'high' : 'low');
      index += 2;
      continue;
    }
    if (token === '--exec-path' || token === '--upload-pack' || token === '--receive-pack') {
      return 'high';
    }
    if (token.startsWith('-')) { index++; continue; }
    break;
  }
  const sub = (rest[index] ?? '').toLowerCase();
  const subArgs = rest.slice(index + 1);
  if (!sub) return maxRisk(floor, 'high');
  if (NETWORK_GIT_SUBCOMMANDS.has(sub)) return 'high';
  if (sub === 'reset' && hasFlag(subArgs, 'h', '--hard')) return 'high';
  if (sub === 'clean' && subArgs.some(t => t.startsWith('-') && /[fdx]/.test(t))) return 'high';
  if (WRITE_GIT_SUBCOMMANDS.has(sub)) return maxRisk(floor, 'medium');
  if (SAFE_GIT_SUBCOMMANDS.has(sub)) return maxRisk(floor, 'low');
  return maxRisk(floor, 'medium');
}

function classifyPackageManager(cmd: string, rest: string[]): RiskClass {
  if (cmd === 'npx' || cmd === 'pnpx' || cmd === 'bunx') return 'high';
  const sub = (rest[0] ?? '').toLowerCase();
  if (!sub) return 'medium';
  if (INSTALL_SUBCOMMANDS.has(sub)) return 'high';
  if (['ls', 'list', 'help', 'version', 'why', 'root', 'config'].includes(sub)) return 'low';
  if (sub === 'run' || sub === 'run-script' || sub === 'workspace') {
    const script = (rest[1] ?? '').toLowerCase();
    if (!script) return 'medium';
    return SAFE_SCRIPT_NAMES.has(script) ? 'low' : 'medium';
  }
  if (['test', 't', 'exec'].includes(sub)) return sub === 'exec' ? 'high' : 'low';
  if (cmd === 'pip' || cmd === 'pip3' || cmd === 'uv' || cmd === 'poetry' || cmd === 'gem' || cmd === 'composer') {
    if (['list', 'show', 'freeze', 'check', '--version', 'version'].includes(sub)) return 'low';
  }
  return 'medium';
}

function classifyAwk(rest: string[]): RiskClass {
  const program = rest.join(' ');
  if (/system\s*\(/.test(program)) return 'critical';
  if (/\|\s*(?:getline|["'])/.test(program) || /print\s*\|/.test(program)) return 'high';
  return 'low';
}

function classifySed(rest: string[]): RiskClass {
  const inPlace = hasFlag(rest, 'i', '--in-place');
  const scripts = rest.filter(t => !t.startsWith('-'));
  // GNU sed `e` runs a shell command: bare (`e cmd`), after an address (`1e id`, `$e`, `/x/e`)
  // or as an `s///e` flag. `w`/`W` write arbitrary files.
  const executes = scripts.some(s => /(?:^|[;\s\d$/!},])e(?:[\s;]|$)/.test(s)
    || /\/[gpIiMm0-9w]*e[gpIiMm0-9]*(?:[;\s]|$)/.test(s));
  if (executes) return 'high';
  const writes = scripts.some(s => /(?:^|[;\s\d$/!},])[wW]\s+\S/.test(s) || /\/[gpIiMm0-9e]*w\s+\S/.test(s));
  return inPlace || writes ? 'medium' : 'low';
}

function classifyBase(cmd: string, rest: string[], depth = 0): RiskClass {
  if (['eval', 'source', '.'].includes(cmd)) return 'critical';
  if (MULTICALL_BINARIES.has(cmd)) {
    if (!rest.length || depth > 4) return 'medium';
    const peeled = peelWrappers(rest);
    if (!peeled.tokens.length) return maxRisk('medium', peeled.floor);
    return maxRisk(peeled.floor, classifyBase(baseName(peeled.tokens[0]), peeled.tokens.slice(1), depth + 1));
  }
  if (GENERIC_RUNNERS.has(cmd)) return classifyGenericRunner(rest, depth);
  if (cmd === 'trap') {
    // `trap 'cmd' SIGNAL` runs the handler later in this shell.
    const handler = rest.find(t => !t.startsWith('-'));
    return handler && depth <= 4 ? maxRisk('medium', classifyBashRiskAt(handler, depth + 1)) : 'medium';
  }
  const inlineFlags = INLINE_CODE_FLAGS[cmd];
  if (inlineFlags && rest.some(t => inlineFlags.includes(t))) return 'high';
  if (SHELL_INTERPRETERS.has(cmd)) {
    return rest.some(t => t === '-c' || t === '/c' || t === '-Command' || t === '-command') ? 'critical' : 'high';
  }
  if (cmd === 'rm') return classifyRm(rest);
  if (DESTRUCTIVE_COMMANDS.has(cmd) || cmd.startsWith('mkfs')) return 'critical';
  if (NETWORK_COMMANDS.has(cmd)) return 'high';
  if (PROCESS_CONTROL_COMMANDS.has(cmd)) return 'high';
  if (PRIVILEGED_COMMANDS.has(cmd)) return 'high';
  if (PACKAGE_MANAGERS.has(cmd)) return classifyPackageManager(cmd, rest);

  if (cmd === 'git') return classifyGit(rest);
  if (cmd === 'awk' || cmd === 'gawk' || cmd === 'mawk') return classifyAwk(rest);
  if (cmd === 'sed') return classifySed(rest);
  if (cmd === 'find') {
    const dangerous = rest.some(t => ['-exec', '-execdir', '-ok', '-okdir', '-delete'].includes(t));
    return dangerous ? 'high' : 'low';
  }
  if (cmd === 'node' || cmd === 'nodejs') {
    if (rest.includes('--test') || rest.some(t => t.startsWith('--test='))) return 'low';
    if (rest.some(t => ['-e', '--eval', '-p', '--print'].includes(t))) return 'high';
    return 'medium';
  }
  if (cmd === 'python' || cmd === 'python3' || cmd === 'pypy' || cmd === 'pypy3') {
    if (rest.includes('-c')) return 'high';
    if (rest.includes('-m')) {
      const module = (rest[rest.indexOf('-m') + 1] ?? '').toLowerCase();
      if (module === 'pytest' || module === 'unittest' || module === 'mypy' || module === 'ruff') return 'low';
      if (module === 'pip') return 'high';
      return 'medium';
    }
    return 'medium';
  }
  if (cmd === 'pytest' || cmd === 'py.test' || cmd === 'mypy' || cmd === 'ruff' || cmd === 'oxlint' || cmd === 'eslint') return 'low';
  if (cmd === 'tsc') return 'low';
  if (cmd === 'go') {
    const sub = (rest[0] ?? '').toLowerCase();
    if (['test', 'build', 'vet', 'fmt', 'mod'].includes(sub)) {
      if (sub === 'mod' && /^(?:download|tidy)$/i.test(rest[1] ?? '')) return 'high';
      return 'low';
    }
    if (['get', 'install'].includes(sub)) return 'high';
    return 'medium';
  }
  if (cmd === 'cargo') {
    const sub = (rest[0] ?? '').toLowerCase();
    if (['test', 'build', 'check', 'clippy', 'bench', 'doc'].includes(sub)) return 'low';
    if (['install', 'add', 'update', 'fetch', 'publish', 'login'].includes(sub)) return 'high';
    if (sub === 'fmt') return rest.some(t => t === '--check' || t === '--check-only') ? 'low' : 'medium';
    return 'medium';
  }
  if (cmd === 'make') {
    const target = rest.find(t => !t.startsWith('-'))?.toLowerCase();
    return target && SAFE_SCRIPT_NAMES.has(target) ? 'low' : 'medium';
  }
  if (FILE_WRITE_COMMANDS.has(cmd)) return 'medium';
  if (SAFE_READ_COMMANDS.has(cmd)) return 'low';
  return 'medium';
}

function classifyGenericRunner(rest: string[], depth: number): RiskClass {
  let risk: RiskClass = 'high';
  if (depth > 4) return risk;
  for (let i = 0; i < rest.length; i++) {
    const token = rest[i];
    // A quoted argument such as `watch "rm -rf /"` or `su -c 'rm -rf /'` is a command line.
    if (/\s/.test(token)) risk = maxRisk(risk, classifyBashRiskAt(token, depth + 1));
    if (token.startsWith('-')) continue;
    const peeled = peelWrappers(rest.slice(i));
    if (!peeled.tokens.length) continue;
    risk = maxRisk(risk, maxRisk(peeled.floor, classifyBase(baseName(peeled.tokens[0]), peeled.tokens.slice(1), depth + 1)));
    if (risk === 'critical') return risk;
  }
  return risk;
}

/**
 * Classify a shell command line. Compound commands take the maximum risk of their segments;
 * redirection raises the floor to medium; unquoted substitutions are critical.
 */
export function classifyBashRisk(command: string): RiskClass {
  return classifyBashRiskAt(command, 0);
}

function classifyBashRiskAt(command: string, depth: number): RiskClass {
  if (typeof command !== 'string' || !command.trim()) return 'medium';
  const scan = scanShell(command);
  if (scan.criticalEscape) return 'critical';
  let risk: RiskClass = scan.redirection ? 'medium' : 'low';
  for (const segment of scan.segments) {
    const tokens = tokenize(segment);
    if (!tokens.length) continue;
    const peeled = peelWrappers(tokens);
    if (!peeled.tokens.length) {
      risk = maxRisk(risk, peeled.floor);
      continue;
    }
    const cmd = baseName(peeled.tokens[0]);
    const innerRest = peeled.tokens.slice(1);
    risk = maxRisk(risk, maxRisk(peeled.floor, classifyBase(cmd, innerRest, depth)));
  }
  return risk;
}

export interface McpRiskAssessment {
  risk: RiskClass;
  net: boolean;
  reason: string;
}

/**
 * Conservative MCP tool risk (P13). Server annotations are untrusted and may only escalate:
 * `readOnlyHint` still gets the medium floor because MCP servers are outside the workspace trust
 * boundary. `trust` never raises risk and never makes a tool read-only; it only auto-approves
 * medium/low risk tools in the registry. The user's explicit `readOnlyTools` allowlist is the
 * only thing that lowers a tool to low. `openWorldHint` (or a server-level `net` flag) marks the
 * tool as network access, which the execution policy gates before approval.
 */
export function classifyMcpRisk(
  server: string,
  tool: string,
  remote: unknown,
  config: McpServerConfig,
): McpRiskAssessment {
  const annotations = remote && typeof remote === 'object' ? remote as Record<string, unknown> : {};
  const reasons: string[] = [];
  let risk: RiskClass = 'medium';
  if (annotations.readOnlyHint === true) reasons.push('readOnlyHint (medium floor)');
  if (annotations.destructiveHint === true) {
    risk = maxRisk(risk, 'high');
    reasons.push('destructiveHint');
  }
  if (annotations.openWorldHint === true) {
    risk = maxRisk(risk, 'high');
    reasons.push('openWorldHint');
  }
  if (config.readOnlyTools.includes(tool)) {
    risk = 'low';
    reasons.push('user readOnlyTools allowlist');
  }
  const net = config.net === true || annotations.openWorldHint === true;
  if (config.net === true) reasons.push('server net flag');
  if (config.trust) reasons.push('trusted server (auto-approves medium/low)');
  if (!reasons.length) reasons.push(`default risk for MCP tool ${server}/${tool}`);
  return { risk, net, reason: reasons.join('; ') };
}
