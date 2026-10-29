import test from 'node:test';
import assert from 'node:assert/strict';
import { join } from 'node:path';
import { mkdir, writeFile } from 'node:fs/promises';
import { fixture } from './helpers.mjs';
import {
  detectProjects,
  scopeFiles,
  resolveLocalTool,
  planVerification,
  parseCommandResult,
  runVerification,
  formatVerifyErrors,
  formatVerifyWarnings,
  redactVerdict,
  redactText,
  createVerifyRunner,
  parseConfiguredCommand,
  VerificationApprovalError,
} from '../dist/verify.js';

test('detectProjects maps manifests to kinds and stops at the workspace boundary', async t => {
  const f = await fixture(t);
  await writeFile(join(f.cwd, 'package.json'), '{}');
  await writeFile(join(f.cwd, 'pyproject.toml'), '');
  let projects = detectProjects(f.cwd, ['src/a.ts', 'lib/b.py']);
  assert.deepEqual(projects.map(p => p.kind), ['node', 'python']);
  assert.deepEqual(projects[0].files, ['src/a.ts']);
  assert.deepEqual(projects[1].files, ['lib/b.py']);

  const parent = f.root;
  await writeFile(join(parent, 'Cargo.toml'), '');
  const child = join(f.cwd, 'nested');
  await mkdir(child, { recursive: true });
  const unknown = detectProjects(child, ['x.rs']);
  assert.equal(unknown.length, 1);
  assert.equal(unknown[0].kind, 'unknown');
  assert.deepEqual(unknown[0].files, ['x.rs']);

  await writeFile(join(f.cwd, 'go.mod'), '');
  assert(detectProjects(f.cwd, ['main.go']).some(project => project.kind === 'go'));
});

test('scopeFiles filters, deduplicates, sorts and skips generated directories', async t => {
  const f = await fixture(t);
  await writeFile(join(f.cwd, 'a.ts'), '');
  await mkdir(join(f.cwd, 'node_modules/x'), { recursive: true });
  await writeFile(join(f.cwd, 'node_modules/x/b.ts'), '');
  const project = { kind: 'node', root: f.cwd, files: [] };
  const result = scopeFiles(['./b.ts', 'a.ts', 'a.ts', 'node_modules/x/b.ts', '.git/x', 'dist/y', 'missing.ts'], project);
  assert.deepEqual(result, ['a.ts']);
});

test('resolveLocalTool uses node_modules and the current Node binary', async t => {
  const f = await fixture(t);
  await mkdir(join(f.cwd, 'node_modules/typescript/bin'), { recursive: true });
  await writeFile(join(f.cwd, 'node_modules/typescript/bin/tsc'), '');
  const resolved = resolveLocalTool(f.cwd, 'tsc');
  assert.ok(resolved);
  assert.equal(resolved.command, process.execPath);
  assert.match(resolved.args[0], /typescript[\\/]+bin[\\/]+tsc$/);
  assert.equal(resolveLocalTool(f.cwd, 'prettier'), undefined);
});

test('planVerification defaults to syntax only', async t => {
  const f = await fixture(t);
  await writeFile(join(f.cwd, 'package.json'), '{"scripts":{"test":"node --test"}}');
  const plan = planVerification(f.cwd, ['src/a.ts'], { commands: [] }, {
    requireTests: false, requireLint: false, requireTypecheck: false, allowWarnings: true,
  });
  assert.deepEqual(plan.stages, ['syntax']);
  assert.deepEqual(plan.commands, []);
  assert.equal(plan.projects[0].kind, 'node');
});

test('planVerification honours require flags and package scripts', async t => {
  const f = await fixture(t);
  await writeFile(join(f.cwd, 'package.json'), JSON.stringify({
    scripts: { lint: 'oxlint src', typecheck: 'tsc --noEmit', build: 'tsc', test: 'node --test' },
  }));
  await writeFile(join(f.cwd, 'tsconfig.json'), '{}');
  await mkdir(join(f.cwd, 'node_modules/typescript/bin'), { recursive: true });
  await writeFile(join(f.cwd, 'node_modules/typescript/bin/tsc'), '');
  await mkdir(join(f.cwd, 'src'), { recursive: true });
  await writeFile(join(f.cwd, 'src/a.ts'), 'export const x = 1;\n');
  const policy = { requireTests: true, requireLint: true, requireTypecheck: true, allowWarnings: true };
  const plan = planVerification(f.cwd, ['src/a.ts'], { commands: [] }, policy);
  assert.deepEqual(plan.stages, ['syntax', 'lint', 'typecheck', 'build', 'test']);
  assert.deepEqual(plan.commands.map(c => c.display), [
    'npm run lint',
    'npm run typecheck',
    'npm run build',
    'npm run test',
  ]);
  // Local tsc fallback when there is no typecheck script.
  const dir = await fixture(t);
  await writeFile(join(dir.cwd, 'package.json'), '{}');
  await writeFile(join(dir.cwd, 'tsconfig.json'), '{}');
  await mkdir(join(dir.cwd, 'node_modules/typescript/bin'), { recursive: true });
  await writeFile(join(dir.cwd, 'node_modules/typescript/bin/tsc'), '');
  await mkdir(join(dir.cwd, 'src'), { recursive: true });
  await writeFile(join(dir.cwd, 'src/a.ts'), 'export const x = 1;\n');
  const plan2 = planVerification(dir.cwd, ['src/a.ts'], { commands: [] }, { ...policy, requireTests: false });
  assert.ok(plan2.commands.some(c => c.display === 'tsc --noEmit -p tsconfig.json'));
});

test('planVerification configured commands replace the matrix', async t => {
  const f = await fixture(t);
  const plan = planVerification(f.cwd, ['src/a.ts'], { commands: ['!echo custom', 'node --version'] }, {
    requireTests: true, requireLint: true, requireTypecheck: true, allowWarnings: true,
  });
  assert.deepEqual(plan.commands.map(c => c.display), ['!echo custom', 'node --version']);
  assert.equal(plan.commands[0].command, 'bash');
  assert.deepEqual(plan.commands[0].args, ['-c', 'echo custom']);
  assert.equal(parseConfiguredCommand('!echo custom').stage, 'test');
});

test('parseCommandResult maps exit codes, timeouts, missing tools and missing modules', () => {
  const policy = { requireTests: true, requireLint: true, requireTypecheck: true, allowWarnings: true };
  const strict = { ...policy, allowWarnings: false };
  assert.equal(parseCommandResult('lint', { code: 0, output: '' }, policy).severity, 'pass');

  const failed = parseCommandResult('lint', { code: 1, output: 'src/a.ts:3:5: Unexpected token' }, policy);
  assert.equal(failed.severity, 'fail');
  assert.equal(failed.issues?.[0]?.path, 'src/a.ts');
  assert.equal(failed.issues?.[0]?.line, 3);
  assert.equal(failed.issues?.[0]?.column, 5);

  const paren = parseCommandResult('typecheck', { code: 1, output: 'src/a.ts(3,5): error TS2322: boom' }, policy);
  assert.equal(paren.issues?.[0]?.line, 3);
  assert.equal(paren.issues?.[0]?.column, 5);

  assert.equal(parseCommandResult('test', { code: 1, output: '', timedOut: true }, policy).reason, 'timeout');
  assert.equal(parseCommandResult('lint', { code: 127, output: 'sh: 1: eslint: not found', notFound: true }, policy).severity, 'skip');
  assert.equal(parseCommandResult('lint', { code: 1, output: 'sh: 1: eslint: not found' }, policy).severity, 'skip');
  assert.equal(parseCommandResult('lint', { code: 1, output: 'bash: line 1: foo: command not found' }, policy).severity, 'skip');
  assert.equal(parseCommandResult('test', { code: 1, output: "Error: Cannot find module './app'" }, policy).severity, 'fail');
  assert.equal(parseCommandResult('test', { code: 1, output: "ENOENT: no such file or directory, open 'fixture.json'" }, policy).severity, 'fail');
  // Generic ': not found' messages are test failures, not missing tools.
  for (const output of ['AssertionError: not found', 'Error: expected element: not found', 'AssertionError [ERR_ASSERTION]: not found']) {
    assert.equal(parseCommandResult('test', { code: 1, output }, policy).severity, 'fail', output);
  }

  const warning = parseCommandResult('lint', { code: 0, output: 'src/a.ts:3: warning: unused' }, policy);
  assert.equal(warning.severity, 'warn');
  assert.equal(parseCommandResult('lint', { code: 0, output: 'src/a.ts:3: warning: unused' }, strict).severity, 'fail');

  const gofmt = parseCommandResult('format', { code: 0, output: 'src/a.go\n' }, policy);
  assert.equal(gofmt.severity, 'warn');
});

test('parseCommandResult normalizes Windows paths', () => {
  const policy = { requireTests: true, requireLint: true, requireTypecheck: true, allowWarnings: true };
  const result = parseCommandResult('lint', { code: 1, output: 'src\\a.ts:3: boom' }, policy);
  assert.equal(result.issues?.[0]?.path, 'src/a.ts');
});

test('runVerification runs the syntax stage without a runner', async t => {
  const f = await fixture(t);
  await writeFile(join(f.cwd, 'bad.ts'), 'export const x = {\n');
  let runnerCalled = false;
  const runner = { async run() { runnerCalled = true; return { code: 0, output: '' }; } };
  const verdict = await runVerification(f.cwd, ['bad.ts'], { enabled: true, commands: [], timeoutMs: 1000, maxRepairRounds: 1, policy: { requireTests: false, requireLint: false, requireTypecheck: false, allowWarnings: true } }, { requireTests: false, requireLint: false, requireTypecheck: false, allowWarnings: true }, runner, new AbortController().signal);
  assert.equal(runnerCalled, false);
  assert.equal(verdict.pass, false);
  assert.equal(verdict.stages[0].stage, 'syntax');
  assert.equal(verdict.stages[0].severity, 'fail');

  await writeFile(join(f.cwd, 'good.ts'), 'export const x = 1;\n');
  const ok = await runVerification(f.cwd, ['good.ts'], { enabled: true, commands: [], timeoutMs: 1000, maxRepairRounds: 1, policy: { requireTests: false, requireLint: false, requireTypecheck: false, allowWarnings: true } }, { requireTests: false, requireLint: false, requireTypecheck: false, allowWarnings: true }, runner, new AbortController().signal);
  assert.equal(ok.pass, true);
});

test('runVerification is fail-closed on approval', async t => {
  const f = await fixture(t);
  const config = { enabled: true, commands: ['node --version'], timeoutMs: 1000, maxRepairRounds: 1, policy: { requireTests: false, requireLint: false, requireTypecheck: false, allowWarnings: true } };
  const policy = { requireTests: false, requireLint: false, requireTypecheck: false, allowWarnings: true };
  await assert.rejects(
    runVerification(f.cwd, [], config, policy, createVerifyRunner(), new AbortController().signal),
    VerificationApprovalError,
  );

  let runnerCalled = false;
  const runner = { async run() { runnerCalled = true; return { code: 0, output: '' }; } };
  const denied = await runVerification(f.cwd, [], config, policy, runner, new AbortController().signal, undefined, async () => false);
  assert.equal(denied.severity, 'skip');
  assert.equal(denied.pass, true);
  assert.equal(runnerCalled, false);

  const allowed = await runVerification(f.cwd, [], config, policy, runner, new AbortController().signal, undefined, async () => true);
  assert.equal(allowed.pass, true);
  assert.equal(runnerCalled, true);
});

test('runVerification executes commands and reports failures', async t => {
  const f = await fixture(t);
  const config = { enabled: true, commands: ['node --version'], timeoutMs: 1000, maxRepairRounds: 1, policy: { requireTests: false, requireLint: false, requireTypecheck: false, allowWarnings: true } };
  const policy = { requireTests: false, requireLint: false, requireTypecheck: false, allowWarnings: true };
  const runner = { async run() { return { code: 0, output: 'v22.0.0' }; } };
  const pass = await runVerification(f.cwd, [], config, policy, runner, new AbortController().signal, undefined, async () => true);
  assert.equal(pass.pass, true);

  const failingRunner = { async run() { return { code: 1, output: 'src/a.ts:3: message' }; } };
  const fail = await runVerification(f.cwd, [], config, policy, failingRunner, new AbortController().signal, undefined, async () => true);
  assert.equal(fail.pass, false);
  assert.equal(fail.errors[0].path, 'src/a.ts');
  assert.equal(fail.errors[0].line, 3);

  const timeoutRunner = { async run() { return { code: 124, output: '', timedOut: true }; } };
  const timeout = await runVerification(f.cwd, [], config, policy, timeoutRunner, new AbortController().signal, undefined, async () => true);
  assert.equal(timeout.pass, false);
  assert.equal(timeout.stages.find(s => s.severity === 'fail')?.reason, 'timeout');

  const notFoundRunner = { async run() { return { code: 127, output: '', notFound: true }; } };
  const skipped = await runVerification(f.cwd, [], config, policy, notFoundRunner, new AbortController().signal, undefined, async () => true);
  assert.equal(skipped.severity, 'skip');
  assert.equal(skipped.pass, true);
});

test('runVerification passes offline Go environment to the runner', async t => {
  const f = await fixture(t);
  const plan = { stages: ['test'], commands: [{ stage: 'test', command: 'go', args: ['test', './...'], display: 'go test ./...' }], projects: [], approved: true };
  let captured = undefined;
  const runner = { async run(_c, _a, opts) { captured = opts.env; return { code: 0, output: '' }; } };
  const config = { enabled: true, commands: [], timeoutMs: 1000, maxRepairRounds: 1, policy: { requireTests: false, requireLint: false, requireTypecheck: false, allowWarnings: true } };
  const policy = { requireTests: false, requireLint: false, requireTypecheck: false, allowWarnings: true };
  const verdict = await runVerification(f.cwd, [], config, policy, runner, new AbortController().signal, plan, async () => true);
  assert.equal(verdict.pass, true);
  assert.equal(captured.GOPROXY, 'off');
  assert.equal(captured.GOFLAGS, '-mod=readonly');
});

test('createVerifyRunner maps success, missing tool and timeout', async () => {
  const runner = createVerifyRunner();
  const ok = await runner.run(process.execPath, ['-e', "console.log('ok')"], { cwd: process.cwd(), timeoutMs: 2000, signal: new AbortController().signal });
  assert.equal(ok.code, 0);
  assert.match(ok.output, /ok/);

  const missing = await runner.run('definitely-not-a-real-tool-xyz', [], { cwd: process.cwd(), timeoutMs: 2000, signal: new AbortController().signal });
  assert.equal(missing.notFound, true);

  const timed = await runner.run(process.execPath, ['-e', 'setInterval(()=>{},1000)'], { cwd: process.cwd(), timeoutMs: 200, signal: new AbortController().signal });
  assert.equal(timed.timedOut, true);
});

test('redactVerdict and redactText remove secrets and credential patterns', () => {
  const verdict = {
    pass: false,
    severity: 'fail',
    stages: [{ stage: 'test', severity: 'fail', durationMs: 1, issues: [{ stage: 'test', path: '', message: 'token=secret-token bearer abc.def' }], output: 'Authorization: Bearer abc.def' }],
    errors: [{ stage: 'test', path: '', message: 'token=secret-token' }],
    warnings: [],
    reason: 'secret-token failed',
  };
  const redacted = redactVerdict(verdict, ['secret-token']);
  assert.equal(JSON.stringify(redacted).includes('secret-token'), false);
  assert.match(redacted.stages[0].output, /\[redacted\]/);
  assert.match(redactText('{"token":"hunter2"}', []), /\[redacted\]/);
});

test('formatVerifyErrors and formatVerifyWarnings produce readable text', () => {
  const verdict = {
    pass: false,
    severity: 'fail',
    stages: [],
    errors: [{ stage: 'lint', path: 'src/a.ts', line: 3, column: 5, message: 'boom' }],
    warnings: [{ stage: 'lint', path: 'src/b.ts', line: 1, message: 'warn' }],
    reason: 'lint failed',
  };
  assert.match(formatVerifyErrors(verdict), /lint failed/);
  assert.match(formatVerifyErrors(verdict), /src\/a\.ts:3:5: boom/);
  assert.match(formatVerifyWarnings(verdict), /src\/b\.ts:1: warn/);
});