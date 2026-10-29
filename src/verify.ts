/**
 * Verification engine (v0.4). Pure planning + execution logic: it never emits events and never
 * performs network access itself. Callers are responsible for approval and event emission.
 *
 * Safety invariants:
 * - Commands are resolved locally (package scripts or `node_modules` binaries run through
 *   `process.execPath`); bare `npx` is never used.
 * - `runVerification` is fail-closed: a plan with commands must be explicitly approved.
 * - A timeout, a missing application module and a missing test fixture are failures, not skips.
 * - Only a missing tool binary/script may skip a stage.
 */
import { existsSync, readFileSync } from 'node:fs';
import { readFile } from 'node:fs/promises';
import { dirname, isAbsolute, join, resolve } from 'node:path';
import { runCommand } from './process.js';
import { validateSyntax } from './syntax.js';
import { classifyBashRisk, riskLevel, type RiskClass } from './risk.js';
import { isNetworkAllowedForCommand, type ExecutionPolicy } from './policy.js';

export type VerifySeverity = 'pass' | 'fail' | 'warn' | 'skip';
export type VerifyStage = 'syntax' | 'format' | 'lint' | 'typecheck' | 'build' | 'test';
export type VerifyProjectKind = 'node' | 'python' | 'rust' | 'go' | 'unknown';

export interface VerifyIssue { stage: VerifyStage; path: string; line?: number; column?: number; message: string; rule?: string }
export interface VerifyStageResult {
  stage: VerifyStage;
  severity: VerifySeverity;
  durationMs: number;
  issues: VerifyIssue[];
  output?: string;
  reason?: string;
}
export interface VerifyVerdict {
  pass: boolean;
  severity: VerifySeverity;
  stages: VerifyStageResult[];
  errors: VerifyIssue[];
  warnings: VerifyIssue[];
  reason: string;
}
export interface VerifyPolicy { requireTests: boolean; requireLint: boolean; requireTypecheck: boolean; allowWarnings: boolean }
export interface VerifyConfig { enabled: boolean; commands: string[]; timeoutMs: number; maxRepairRounds: number; policy: VerifyPolicy }
export interface VerifyCommand { stage: VerifyStage; command: string; args: string[]; display: string }
export interface VerifyProject { kind: VerifyProjectKind; root: string; files: string[] }
export interface VerifyPlan { stages: VerifyStage[]; commands: VerifyCommand[]; projects: VerifyProject[]; approved?: boolean }

export interface VerifyRunnerResult { code: number; output: string; timedOut?: boolean; notFound?: boolean }
export interface VerifyRunner {
  run(
    command: string,
    args: string[],
    opts: { cwd: string; timeoutMs: number; signal: AbortSignal; env?: NodeJS.ProcessEnv },
  ): Promise<VerifyRunnerResult>;
}

export const DEFAULT_VERIFY_POLICY: VerifyPolicy = {
  requireTests: false,
  requireLint: false,
  requireTypecheck: false,
  allowWarnings: true,
};
export const DEFAULT_VERIFY_CONFIG: VerifyConfig = {
  enabled: true,
  commands: [],
  timeoutMs: 120_000,
  maxRepairRounds: 2,
  policy: DEFAULT_VERIFY_POLICY,
};

const STAGE_ORDER: VerifyStage[] = ['syntax', 'format', 'lint', 'typecheck', 'build', 'test'];
const NODE_EXTENSIONS = new Set(['.ts', '.tsx', '.mts', '.cts', '.js', '.jsx', '.mjs', '.cjs']);
const PYTHON_EXTENSIONS = new Set(['.py', '.pyi']);
const RUST_EXTENSIONS = new Set(['.rs']);
const GO_EXTENSIONS = new Set(['.go']);
const SKIP_SEGMENTS = new Set(['node_modules', '.git', 'dist']);
const MAX_ISSUE_CHARS = 8_000;
const MAX_ISSUES = 20;
const MAX_TOTAL_ISSUE_CHARS = 16 * 1024;
const MISSING_TOOL_RE = /(?:command not found|is not recognized as an internal or external command|npx canceled due to missing packages|:\s*\d+:\s*[^\n]+:\s*not found)/i;

export class VerificationFailedError extends Error {
  constructor(readonly verdict: VerifyVerdict) {
    super(verdict.reason || 'Verification failed.');
    this.name = 'VerificationFailedError';
  }
}
export class VerificationApprovalError extends Error {
  constructor(message = 'Verification commands require approval before running.') {
    super(message);
    this.name = 'VerificationApprovalError';
  }
}

export function clampVerifyTimeout(timeoutMs: number): number {
  if (!Number.isFinite(timeoutMs)) return 120_000;
  return Math.min(600_000, Math.max(1_000, Math.trunc(timeoutMs)));
}

function extensionOf(path: string): string {
  const base = path.replace(/\\/g, '/').split('/').pop() ?? '';
  const dot = base.lastIndexOf('.');
  return dot > 0 ? base.slice(dot).toLowerCase() : '';
}

function normalizeRelative(path: string): string | undefined {
  const posix = path.replace(/\\/g, '/');
  if (!posix || posix === '.') return undefined;
  if (posix.startsWith('/') || /^[A-Za-z]:\//.test(posix) || isAbsolute(path)) return undefined;
  const normalized = posix.replace(/^(?:\.\/)+/, '');
  if (!normalized || normalized === '..' || normalized.startsWith('../')) return undefined;
  return normalized;
}

function normalizedList(files: string[]): string[] {
  const out = new Set<string>();
  for (const file of files) {
    const rel = normalizeRelative(file);
    if (rel) out.add(rel);
  }
  return [...out];
}

const KIND_EXTENSIONS: Record<Exclude<VerifyProjectKind, 'unknown'>, Set<string>> = {
  node: NODE_EXTENSIONS,
  python: PYTHON_EXTENSIONS,
  rust: RUST_EXTENSIONS,
  go: GO_EXTENSIONS,
};

/**
 * Detects project kinds from manifests in `cwd` only (never climbing to a parent, so a parent
 * monorepo manifest cannot make verification run commands for an unrelated subdirectory).
 * Returns a single `unknown` project when no manifest exists.
 */
export function detectProjects(cwd: string, changedFiles: string[]): VerifyProject[] {
  const root = resolve(cwd);
  const kinds: Array<Exclude<VerifyProjectKind, 'unknown'>> = [];
  if (
    existsSync(join(root, 'package.json')) ||
    existsSync(join(root, 'tsconfig.json')) ||
    existsSync(join(root, 'jsconfig.json'))
  ) kinds.push('node');
  if (
    existsSync(join(root, 'pyproject.toml')) ||
    existsSync(join(root, 'setup.cfg')) ||
    existsSync(join(root, 'requirements.txt'))
  ) kinds.push('python');
  if (existsSync(join(root, 'Cargo.toml'))) kinds.push('rust');
  if (existsSync(join(root, 'go.mod'))) kinds.push('go');
  const files = normalizedList(changedFiles);
  if (!kinds.length) return [{ kind: 'unknown', root, files }];
  return kinds.map(kind => ({
    kind,
    root,
    files: files.filter(file => KIND_EXTENSIONS[kind].has(extensionOf(file))),
  }));
}

/** Existing workspace-relative files belonging to the project, deduped and sorted. */
export function scopeFiles(changedFiles: string[], project: VerifyProject): string[] {
  const out = new Set<string>();
  for (const file of changedFiles) {
    const rel = normalizeRelative(file);
    if (!rel) continue;
    if (rel.split('/').some(segment => SKIP_SEGMENTS.has(segment))) continue;
    if (!existsSync(join(project.root, rel))) continue;
    out.add(rel);
  }
  return [...out].sort();
}

export interface LocalTool { command: string; args: string[] }

/**
 * Resolves a tool that ships in `node_modules`, running it with the current Node binary so a
 * spawn never depends on PATH or on `.cmd` shims (which fail with ENOENT on win32).
 */
export function resolveLocalTool(root: string, tool: 'tsc' | 'eslint' | 'prettier' | 'oxlint'): LocalTool | undefined {
  const candidates: Record<typeof tool, string[]> = {
    tsc: ['node_modules/typescript/bin/tsc'],
    eslint: ['node_modules/eslint/bin/eslint.js'],
    prettier: ['node_modules/prettier/bin/prettier.cjs', 'node_modules/prettier/bin-prettier.js'],
    oxlint: ['node_modules/oxlint/bin/oxlint'],
  };
  for (const relative of candidates[tool]) {
    const file = join(root, relative);
    if (existsSync(file)) return { command: process.execPath, args: [file] };
  }
  return undefined;
}

function npmInvocation(root: string, args: string[]): LocalTool {
  const execpath = process.env.npm_execpath;
  if (execpath && existsSync(execpath)) return { command: process.execPath, args: [execpath, ...args] };
  const localCli = join(root, 'node_modules/npm/bin/npm-cli.js');
  if (existsSync(localCli)) return { command: process.execPath, args: [localCli, ...args] };
  if (process.platform === 'win32') {
    return { command: process.env.ComSpec || 'cmd.exe', args: ['/d', '/s', '/c', 'npm', ...args] };
  }
  const adjacent = join(dirname(process.execPath), 'npm');
  if (existsSync(adjacent)) return { command: adjacent, args };
  return { command: 'npm', args };
}

interface PackageJson { scripts?: Record<string, unknown> }

function readPackageJson(root: string): PackageJson | undefined {
  try {
    const parsed: unknown = JSON.parse(readFileSync(join(root, 'package.json'), 'utf8'));
    if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) return undefined;
    return parsed as PackageJson;
  } catch {
    return undefined;
  }
}

function packageScript(root: string, name: string): string | undefined {
  const scripts = readPackageJson(root)?.scripts;
  const value = scripts?.[name];
  return typeof value === 'string' && value.trim() ? value.trim() : undefined;
}

function npmCommand(root: string, stage: VerifyStage, script: string): VerifyCommand {
  const invocation = npmInvocation(root, ['run', script]);
  return { stage, command: invocation.command, args: invocation.args, display: `npm run ${script}` };
}

function localCommand(tool: LocalTool, logicalName: string, args: string[], stage: VerifyStage): VerifyCommand {
  return { stage, command: tool.command, args: [...tool.args, ...args], display: [logicalName, ...args].join(' ') };
}

function bareCommand(command: string, args: string[], stage: VerifyStage): VerifyCommand {
  return { stage, command, args, display: [command, ...args].join(' ') };
}

function nodeCommands(root: string, files: string[], policy: VerifyPolicy): VerifyCommand[] {
  const out: VerifyCommand[] = [];
  if (policy.requireLint) {
    if (packageScript(root, 'format:check')) out.push(npmCommand(root, 'format', 'format:check'));
    else {
      const prettier = resolveLocalTool(root, 'prettier');
      if (prettier && files.length) out.push(localCommand(prettier, 'prettier', ['--check', ...files], 'format'));
    }
    if (packageScript(root, 'lint')) out.push(npmCommand(root, 'lint', 'lint'));
    else {
      const eslint = resolveLocalTool(root, 'eslint');
      if (eslint && files.length) out.push(localCommand(eslint, 'eslint', files, 'lint'));
      else {
        const oxlint = resolveLocalTool(root, 'oxlint');
        if (oxlint && files.length) out.push(localCommand(oxlint, 'oxlint', files, 'lint'));
      }
    }
  }
  if (policy.requireTypecheck) {
    const tsconfig = existsSync(join(root, 'tsconfig.json'))
      ? 'tsconfig.json'
      : existsSync(join(root, 'jsconfig.json')) ? 'jsconfig.json' : undefined;
    if (packageScript(root, 'typecheck')) out.push(npmCommand(root, 'typecheck', 'typecheck'));
    else if (tsconfig) {
      const tsc = resolveLocalTool(root, 'tsc');
      if (tsc) out.push(localCommand(tsc, 'tsc', ['--noEmit', '-p', tsconfig], 'typecheck'));
    }
    if (packageScript(root, 'build')) out.push(npmCommand(root, 'build', 'build'));
  }
  if (policy.requireTests) {
    if (packageScript(root, 'test')) out.push(npmCommand(root, 'test', 'test'));
    else out.push(bareCommand(process.execPath, ['--test'], 'test'));
  }
  return out;
}

function pythonCommands(files: string[], policy: VerifyPolicy): VerifyCommand[] {
  const out: VerifyCommand[] = [];
  if (policy.requireLint && files.length) {
    out.push(bareCommand('black', ['--check', ...files], 'format'));
    out.push(bareCommand('ruff', ['check', ...files], 'lint'));
  }
  if (policy.requireTypecheck && files.length) out.push(bareCommand('mypy', files, 'typecheck'));
  if (policy.requireTests) out.push(bareCommand('pytest', [], 'test'));
  return out;
}

function rustCommands(files: string[], policy: VerifyPolicy): VerifyCommand[] {
  const out: VerifyCommand[] = [];
  if (policy.requireLint && files.length) {
    out.push(bareCommand('rustfmt', ['--check', ...files], 'format'));
    out.push(bareCommand('cargo', ['clippy', '--offline'], 'lint'));
  }
  if (policy.requireTypecheck) {
    out.push(bareCommand('cargo', ['check', '--offline'], 'typecheck'));
    out.push(bareCommand('cargo', ['check', '--offline'], 'build'));
  }
  if (policy.requireTests) out.push(bareCommand('cargo', ['test', '--offline'], 'test'));
  return out;
}

function goCommands(files: string[], policy: VerifyPolicy): VerifyCommand[] {
  const out: VerifyCommand[] = [];
  if (policy.requireLint && files.length) out.push(bareCommand('gofmt', ['-l', ...files], 'format'));
  if (policy.requireLint) out.push(bareCommand('go', ['vet', './...'], 'lint'));
  if (policy.requireTypecheck) {
    out.push(bareCommand('go', ['build', './...'], 'typecheck'));
    out.push(bareCommand('go', ['build', './...'], 'build'));
  }
  if (policy.requireTests) out.push(bareCommand('go', ['test', './...'], 'test'));
  return out;
}

function inferConfiguredStage(command: string): VerifyStage {
  const lower = command.toLowerCase();
  if (lower.includes('format') || lower.includes('prettier')) return 'format';
  if (lower.includes('lint') || lower.includes('eslint') || lower.includes('oxlint') || lower.includes('ruff') || lower.includes('clippy')) return 'lint';
  if (lower.includes('typecheck') || lower.includes('type-check') || lower.includes('tsc') || lower.includes('mypy')) return 'typecheck';
  if (lower.includes('build')) return 'build';
  return 'test';
}

/** Parses a configured `verify.commands` entry: plain argv split, or `!cmd` via bash. */
export function parseConfiguredCommand(raw: string): VerifyCommand | undefined {
  const trimmed = raw.trim();
  if (!trimmed) return undefined;
  if (trimmed.startsWith('!')) {
    const script = trimmed.slice(1).trim();
    if (!script) return undefined;
    return { stage: inferConfiguredStage(script), command: 'bash', args: ['-c', script], display: trimmed };
  }
  const [command, ...args] = trimmed.split(/\s+/);
  return { stage: inferConfiguredStage(trimmed), command, args, display: trimmed };
}

/** Shell-equivalent command line for a verify command (used for risk and network checks). */
export function verifyCommandLine(command: VerifyCommand): string {
  if (command.command === 'bash' && command.args[0] === '-c' && command.args.length === 2) return command.args[1];
  const quote = (part: string) => (/^[A-Za-z0-9_@%+=:,./-]+$/.test(part) ? part : `'${part.replace(/'/g, `'\\''`)}'`);
  return [command.command, ...command.args].map(quote).join(' ');
}

export interface VerifyPlanAssessment {
  /** Highest risk among the plan's commands (low for an empty plan). */
  risk: RiskClass;
  /** Set when a command must not run at all (critical risk or blocked by the execution policy). */
  denied?: string;
}

/**
 * Verification commands are shell commands too: they get the same critical-risk and network
 * execution-policy gates as the bash tool before anything is offered for approval. The returned
 * risk is passed to the approval callback so auto mode never runs a high-risk verify plan.
 */
export function assessVerifyPlan(plan: Pick<VerifyPlan, 'commands'>, policy?: ExecutionPolicy): VerifyPlanAssessment {
  let risk: RiskClass = 'low';
  for (const command of plan.commands) {
    const line = verifyCommandLine(command);
    const commandRisk = classifyBashRisk(line);
    if (riskLevel(commandRisk) > riskLevel(risk)) risk = commandRisk;
    if (commandRisk === 'critical') {
      return { risk: 'critical', denied: `verification command is critical risk and cannot run: ${command.display}` };
    }
    if (policy && !isNetworkAllowedForCommand(policy, line)) {
      return { risk, denied: `verification command needs network access blocked by the execution policy: ${command.display}` };
    }
  }
  return { risk };
}

function dedupeCommands(commands: VerifyCommand[]): VerifyCommand[] {
  const seen = new Set<string>();
  const out: VerifyCommand[] = [];
  for (const command of commands) {
    const key = JSON.stringify([command.command, command.args]);
    if (seen.has(key)) continue;
    seen.add(key);
    out.push(command);
  }
  return out;
}

/**
 * Builds the verification plan without running anything. Package-level commands run at the
 * project root; file-scoped tools receive the changed files.
 */
export function planVerification(
  cwd: string,
  changedFiles: string[],
  config: Pick<VerifyConfig, 'commands'>,
  policy: VerifyPolicy,
): VerifyPlan {
  const projects = detectProjects(cwd, changedFiles);
  let commands: VerifyCommand[] = [];
  if (config.commands.length) {
    commands = config.commands.map(parseConfiguredCommand).filter((command): command is VerifyCommand => command !== undefined);
  } else {
    for (const project of projects) {
      const files = scopeFiles(changedFiles, project);
      if (project.kind === 'node') commands.push(...nodeCommands(project.root, files, policy));
      else if (project.kind === 'python') commands.push(...pythonCommands(files, policy));
      else if (project.kind === 'rust') commands.push(...rustCommands(files, policy));
      else if (project.kind === 'go') commands.push(...goCommands(files, policy));
    }
  }
  commands = dedupeCommands(commands);
  const stages = new Set<VerifyStage>(changedFiles.length ? ['syntax'] : []);
  for (const command of commands) stages.add(command.stage);
  return {
    stages: STAGE_ORDER.filter(stage => stages.has(stage)),
    commands,
    projects,
  };
}

function truncateIssueText(text: string): string {
  return text.length <= MAX_ISSUE_CHARS ? text : `${text.slice(0, MAX_ISSUE_CHARS)}\n...[truncated ${text.length - MAX_ISSUE_CHARS} chars]`;
}

function capIssues(issues: VerifyIssue[]): VerifyIssue[] {
  const capped: VerifyIssue[] = [];
  let total = 0;
  let dropped = 0;
  for (const issue of issues) {
    const message = truncateIssueText(issue.message);
    if (capped.length >= MAX_ISSUES || total + message.length > MAX_TOTAL_ISSUE_CHARS) {
      dropped++;
      continue;
    }
    total += message.length;
    capped.push({ ...issue, message });
  }
  if (dropped > 0) {
    capped.push({ stage: issues[issues.length - 1]?.stage ?? 'test', path: '', message: `[+${dropped} more]` });
  }
  return capped;
}

function cleanPath(path: string): string {
  return path.trim().replace(/\\/g, '/').replace(/^(?:\.\/)+/, '');
}

/** Extracts `path:line[:column]: message` and `path(line,column): message` issues. */
export function parseIssues(stage: VerifyStage, output: string): VerifyIssue[] {
  const issues: VerifyIssue[] = [];
  for (const rawLine of output.split(/\r?\n/)) {
    const line = rawLine.trim();
    if (!line) continue;
    let match = line.match(/^(.+?)\((\d+),(\d+)\):\s*(.+)$/);
    if (match) {
      issues.push({ stage, path: cleanPath(match[1]), line: Number(match[2]), column: Number(match[3]), message: match[4] });
      continue;
    }
    match = line.match(/^(.+?):(\d+)(?::(\d+))?[:,]?\s+(.+)$/);
    if (match) {
      issues.push({
        stage,
        path: cleanPath(match[1]),
        line: Number(match[2]),
        ...(match[3] ? { column: Number(match[3]) } : {}),
        message: match[4],
      });
    }
  }
  return issues;
}

function parseWarnings(stage: VerifyStage, output: string): VerifyIssue[] {
  const issues: VerifyIssue[] = [];
  for (const rawLine of output.split(/\r?\n/)) {
    const line = rawLine.trim();
    if (!line || !/\bwarning\b/i.test(line)) continue;
    if (/\b(?:no|0)\s+warnings?\b/i.test(line)) continue;
    const parsed = parseIssues(stage, line);
    issues.push(parsed[0] ?? { stage, path: '', message: line });
  }
  return issues;
}

/**
 * Interprets one command result. Timeout is always a failure. A missing tool signature may skip
 * a stage; a generic missing module or missing file always fails.
 */
export function parseCommandResult(
  stage: VerifyStage,
  result: VerifyRunnerResult,
  policy: VerifyPolicy,
): VerifyStageResult {
  const output = truncateIssueText(result.output ?? '');
  if (result.timedOut) {
    return {
      stage,
      severity: 'fail',
      durationMs: 0,
      output,
      reason: 'timeout',
      issues: [{ stage, path: '', message: `Command timed out.${output.trim() ? `\n${output.trim()}` : ''}` }],
    };
  }
  if (result.notFound) {
    return { stage, severity: 'skip', durationMs: 0, output, reason: 'tool not found', issues: [] };
  }
  if (result.code === 0) {
    // `gofmt -l` lists unformatted files but exits 0, so non-empty format output is a warning.
    if (stage === 'format' && result.output.trim()) {
      const issues = result.output.trim().split(/\r?\n/).filter(Boolean)
        .map(line => ({ stage, path: cleanPath(line), message: 'File is not formatted.' }));
      return {
        stage,
        severity: policy.allowWarnings ? 'warn' : 'fail',
        durationMs: 0,
        output,
        issues: issues.length ? issues : [{ stage, path: '', message: 'Files are not formatted.' }],
        reason: policy.allowWarnings ? 'not formatted' : 'format required',
      };
    }
    const warnings = parseWarnings(stage, output);
    if (warnings.length) {
      return {
        stage,
        severity: policy.allowWarnings ? 'warn' : 'fail',
        durationMs: 0,
        output,
        reason: policy.allowWarnings ? 'warnings' : 'warnings not allowed',
        issues: warnings,
      };
    }
    return { stage, severity: 'pass', durationMs: 0, output, issues: [] };
  }
  if (MISSING_TOOL_RE.test(output)) {
    return { stage, severity: 'skip', durationMs: 0, output, reason: 'tool not found', issues: [] };
  }
  const issues = parseIssues(stage, output);
  if (!issues.length) {
    const detail = output.split(/\r?\n/).map(line => line.trim()).filter(Boolean).slice(-5).join('\n');
    issues.push({ stage, path: '', message: detailsOrExit(detail, result.code) });
  }
  return {
    stage,
    severity: 'fail',
    durationMs: 0,
    output,
    reason: `exit code ${result.code}`,
    issues,
  };
}

function detailsOrExit(detail: string, code: number): string {
  const text = detail || `Command exited with code ${code}.`;
  return text.length > MAX_ISSUE_CHARS ? truncateIssueText(text) : text;
}

function buildVerdict(stages: VerifyStageResult[]): VerifyVerdict {
  const failed = stages.find(stage => stage.severity === 'fail');
  const warned = stages.find(stage => stage.severity === 'warn');
  const errors = capIssues(stages.filter(stage => stage.severity === 'fail').flatMap(stage => stage.issues));
  const warnings = capIssues(stages.filter(stage => stage.severity === 'warn').flatMap(stage => stage.issues));
  let severity: VerifySeverity = 'pass';
  if (failed) severity = 'fail';
  else if (warned) severity = 'warn';
  else if (!stages.length || stages.every(stage => stage.severity === 'skip')) severity = 'skip';
  const reason = failed
    ? `${failed.stage} failed${failed.reason ? ` (${failed.reason})` : ''}`
    : warned ? `warnings from ${warned.stage}`
      : severity === 'skip' ? 'no runnable checks' : 'ok';
  return { pass: severity !== 'fail', severity, stages, errors, warnings, reason };
}

async function runSyntaxStage(cwd: string, changedFiles: string[]): Promise<VerifyStageResult> {
  const started = Date.now();
  const root = resolve(cwd);
  const files = scopeFiles(changedFiles, { kind: 'unknown', root, files: changedFiles });
  const issues: VerifyIssue[] = [];
  for (const file of files) {
    let content: string;
    try {
      content = await readFile(join(root, file), 'utf8');
    } catch {
      continue; // deleted or unreadable: nothing to validate here
    }
    try {
      validateSyntax(file, content);
    } catch (error) {
      const message = (error as Error).message;
      const line = Number(message.match(/\bline (\d+)\b/)?.[1]);
      issues.push({ stage: 'syntax', path: file, ...(Number.isFinite(line) && line > 0 ? { line } : {}), message });
    }
  }
  return {
    stage: 'syntax',
    severity: issues.length ? 'fail' : 'pass',
    durationMs: Date.now() - started,
    issues,
    ...(issues.length ? { reason: 'syntax' } : {}),
  };
}

function envForCommand(command: VerifyCommand): NodeJS.ProcessEnv | undefined {
  if (command.command.replace(/\.exe$/i, '') === 'go') {
    // Offline-by-default: never download modules while verifying.
    return { ...process.env, GOPROXY: 'off', GOFLAGS: '-mod=readonly' };
  }
  return undefined;
}

/**
 * Runs a verification plan. `approve` is required whenever the plan contains commands; it must
 * present them to the user. Denial returns a skipped verdict (pass=true), not a failure.
 */
export async function runVerification(
  cwd: string,
  changedFiles: string[],
  config: VerifyConfig,
  policy: VerifyPolicy,
  runner: VerifyRunner,
  signal: AbortSignal,
  plan?: VerifyPlan,
  approve?: (plan: VerifyPlan) => Promise<boolean> | boolean,
): Promise<VerifyVerdict> {
  const resolvedPlan = plan ?? planVerification(cwd, changedFiles, config, policy);
  if (resolvedPlan.commands.length > 0 && resolvedPlan.approved !== true) {
    if (!approve) throw new VerificationApprovalError();
    const allowed = await approve(resolvedPlan);
    if (!allowed) {
      return { pass: true, severity: 'skip', stages: [], errors: [], warnings: [], reason: 'approval denied' };
    }
    resolvedPlan.approved = true;
  }
  const stages: VerifyStageResult[] = [];
  if (changedFiles.length) {
    const syntax = await runSyntaxStage(cwd, changedFiles);
    stages.push(syntax);
    if (syntax.severity === 'fail') return buildVerdict(stages);
  }
  const timeoutMs = clampVerifyTimeout(config.timeoutMs);
  for (const command of resolvedPlan.commands) {
    if (signal.aborted) {
      stages.push({
        stage: command.stage,
        severity: 'fail',
        durationMs: 0,
        issues: [{ stage: command.stage, path: '', message: 'Command cancelled.' }],
        reason: 'cancelled',
      });
      break;
    }
    const started = Date.now();
    const env = envForCommand(command);
    let result: VerifyRunnerResult;
    try {
      result = await runner.run(command.command, command.args, {
        cwd,
        timeoutMs,
        signal,
        ...(env ? { env } : {}),
      });
    } catch (error) {
      if ((error as Error)?.name === 'AbortError' || signal.aborted) {
        result = { code: 130, output: 'Command cancelled.' };
      } else {
        result = { code: 1, output: `Failed to run command: ${(error as Error)?.message ?? String(error)}` };
      }
    }
    const stageResult = parseCommandResult(command.stage, result, policy);
    stageResult.durationMs = Date.now() - started;
    stages.push(stageResult);
    if (stageResult.severity === 'fail') break;
  }
  return buildVerdict(stages);
}

/** Short user/model-facing error text (caller redacts before pushing to the session). */
export function formatVerifyErrors(verdict: VerifyVerdict): string {
  const lines = [`Verification failed: ${verdict.reason}`];
  for (const issue of verdict.errors.slice(0, MAX_ISSUES)) {
    const location = issue.path
      ? ` ${issue.path}${issue.line !== undefined ? `:${issue.line}${issue.column !== undefined ? `:${issue.column}` : ''}` : ''}`
      : '';
    lines.push(`- ${issue.stage}${location}: ${truncateIssueText(issue.message)}`);
  }
  if (verdict.errors.length > MAX_ISSUES) lines.push(`[+${verdict.errors.length - MAX_ISSUES} more]`);
  return lines.join('\n');
}

export function formatVerifyWarnings(verdict: VerifyVerdict): string {
  if (!verdict.warnings.length) return '';
  const lines = ['Verification warnings:'];
  for (const issue of verdict.warnings.slice(0, MAX_ISSUES)) {
    const location = issue.path
      ? ` ${issue.path}${issue.line !== undefined ? `:${issue.line}${issue.column !== undefined ? `:${issue.column}` : ''}` : ''}`
      : '';
    lines.push(`- ${issue.stage}${location}: ${truncateIssueText(issue.message)}`);
  }
  if (verdict.warnings.length > MAX_ISSUES) lines.push(`[+${verdict.warnings.length - MAX_ISSUES} more]`);
  return lines.join('\n');
}

/** Redacts literal secrets, bearer tokens and key/value credentials from every verdict string. */
export function redactVerdict(verdict: VerifyVerdict, secrets: Array<string | undefined>): VerifyVerdict {
  const clean = (text: string): string => redactText(text, secrets);
  return {
    ...verdict,
    reason: clean(verdict.reason),
    stages: verdict.stages.map(stage => ({
      ...stage,
      ...(stage.output !== undefined ? { output: clean(stage.output) } : {}),
      ...(stage.reason !== undefined ? { reason: clean(stage.reason) } : {}),
      issues: stage.issues.map(issue => ({ ...issue, message: clean(issue.message) })),
    })),
    errors: verdict.errors.map(issue => ({ ...issue, message: clean(issue.message) })),
    warnings: verdict.warnings.map(issue => ({ ...issue, message: clean(issue.message) })),
  };
}

export function redactText(text: string, secrets: Array<string | undefined>): string {
  let out = text;
  for (const secret of secrets) {
    if (secret && secret.length >= 4) out = out.split(secret).join('[redacted]');
  }
  out = out.replace(/\b(bearer)\s+[A-Za-z0-9._~+/=-]+/gi, '$1 [redacted]');
  out = out.replace(
    /("?(?:api[_-]?key|access[_-]?token|refresh[_-]?token|client[_-]?secret|token|secret|password|passwd|pwd)"?\s*[:=]\s*)(?:"[^"]*"|'[^']*'|[^\s"',}]+)/gi,
    '$1[redacted]',
  );
  return out;
}

/**
 * Real runner backed by `runCommand`. It maps spawn errors to explicit outcomes so the parser
 * can distinguish a missing tool (skip) from a timeout (fail) and a generic runtime error (fail).
 * `runCommand`/`childEnv` strip provider credentials from the child environment.
 */
export function createVerifyRunner(): VerifyRunner {
  return {
    async run(command, args, opts): Promise<VerifyRunnerResult> {
      try {
        const result = await runCommand(command, args, {
          cwd: opts.cwd,
          signal: opts.signal,
          timeoutMs: opts.timeoutMs,
          ...(opts.env ? { env: opts.env } : {}),
        });
        return { code: result.code, output: result.output };
      } catch (error) {
        if (opts.signal.aborted) return { code: 130, output: 'Command cancelled.' };
        const failure = error as NodeJS.ErrnoException;
        if (failure?.code === 'ENOENT') return { code: 127, output: failure.message, notFound: true };
        const message = failure instanceof Error ? failure.message : String(error);
        if (/timed out/i.test(message)) return { code: 124, output: message, timedOut: true };
        if (/cancelled/i.test(message)) return { code: 130, output: message };
        return { code: 1, output: message };
      }
    },
  };
}
