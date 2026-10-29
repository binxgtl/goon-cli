import test from 'node:test';
import assert from 'node:assert/strict';
import { classifyBashRisk, riskLevel } from '../dist/risk.js';
import { ToolRegistry } from '../dist/tools.js';
import { bashApprovalExact, isApprovedBashExact, canAutoApprove } from '../dist/approval.js';
import { executeWorkflow } from '../dist/workflow.js';

test('risk: safe read/build/test commands are low', () => {
  for (const cmd of ['cat README.md', 'ls -la', 'grep -R foo src', 'npm test', 'npm run build', 'npm run lint', 'npm run typecheck', 'node --test tests/x.test.mjs', 'pytest', 'go test ./...', 'cargo test', 'git status', 'git diff HEAD']) {
    assert.equal(classifyBashRisk(cmd), 'low', cmd);
  }
});

test('risk: mutating filesystem and unknown commands are medium', () => {
  for (const cmd of ['touch foo.txt', 'rm file.txt', 'cp a b', 'some-unknown-tool --flag']) {
    assert.equal(classifyBashRisk(cmd), 'medium', cmd);
  }
});

test('risk: network commands are high', () => {
  for (const cmd of ['curl https://example.com', 'wget x', 'npm install', 'git push origin main', 'git fetch', 'npm test && curl x']) {
    assert.equal(classifyBashRisk(cmd), 'high', cmd);
  }
});

test('risk: destructive commands are critical', () => {
  assert.equal(classifyBashRisk('rm -rf /'), 'critical');
  assert.equal(classifyBashRisk('rm -rf /*'), 'critical');
  assert.equal(classifyBashRisk('rm -rf $HOME'), 'critical');
  assert.equal(classifyBashRisk('rm -rf ~'), 'critical');
  assert.equal(classifyBashRisk('dd if=/dev/zero of=/dev/sda'), 'critical');
  assert.equal(classifyBashRisk('eval "id"'), 'critical');
  assert.equal(classifyBashRisk('bash -c "id"'), 'critical');
  assert.equal(classifyBashRisk('source x.sh'), 'critical');
});

test('risk: rm -rf node_modules is high, not critical', () => {
  assert.equal(classifyBashRisk('rm -rf node_modules'), 'high');
});

test('risk: rm flags cannot hide behind line continuation or ANSI-C quoting', () => {
  // `\<newline>` is a line continuation: must classify exactly like the flattened command.
  assert.equal(classifyBashRisk('rm \\\n-rf /'), 'critical');
  assert.equal(classifyBashRisk('rm \\\r\n-rf /'), 'critical');
  assert.equal(classifyBashRisk('rm -rf \\\n/'), 'critical');
  // ANSI-C quoting decodes escapes, so $'\x2drf' is the -rf flag.
  assert.equal(classifyBashRisk("rm $'\\x2drf' /"), 'critical');
  assert.equal(classifyBashRisk("rm $'\\x2d\\x72\\x66' /"), 'critical');
  assert.equal(classifyBashRisk("rm $'-rf' /"), 'critical');
  assert.equal(classifyBashRisk("rm $'\\055rf' /"), 'critical');
  // Benign continuations and ANSI-C strings stay at their normal level.
  assert.equal(classifyBashRisk('npm test \\\n  -- --runInBand'), 'low');
  assert.equal(classifyBashRisk("echo $'\\n'"), 'low');
});

test('risk: shell substitution outside single quotes is critical', () => {
  assert.equal(classifyBashRisk('echo "$(id)"'), 'critical');
  assert.equal(classifyBashRisk('echo `id`'), 'critical');
  assert.equal(classifyBashRisk('echo $(id)'), 'critical');
});

test('risk: shell substitution inside single quotes is not critical', () => {
  assert.equal(classifyBashRisk("echo '$(id)'"), 'low');
});

test('risk: arithmetic substitution is not critical', () => {
  assert.equal(classifyBashRisk('echo $((1 + 2))'), 'low');
});

test('risk: quote-aware splitting keeps quoted semicolons low', () => {
  assert.equal(classifyBashRisk('echo "a;b"'), 'low');
  assert.equal(classifyBashRisk("echo 'a;b'"), 'low');
});

test('risk: redirection raises the floor to medium', () => {
  assert.equal(classifyBashRisk('npm test > /tmp/out'), 'medium');
});

test('risk: wrapper peeling escalates privilege', () => {
  assert.equal(classifyBashRisk('sudo rm -rf /'), 'critical');
  assert.equal(classifyBashRisk('env FOO=1 npm test'), 'low');
  assert.equal(classifyBashRisk('timeout 5 curl x'), 'high');
  assert.equal(classifyBashRisk('xargs rm -rf /'), 'critical');
});

test('risk: compound command takes maximum risk', () => {
  assert.equal(classifyBashRisk('npm test && curl https://example.com'), 'high');
  assert.equal(classifyBashRisk('git status; rm -rf /'), 'critical');
});

test('risk: riskLevel ordering', () => {
  assert.equal(riskLevel('low'), 0);
  assert.equal(riskLevel('critical'), 3);
});

test('risk: critical tool is denied without calling approve', async () => {
  let approved = false;
  const reg = new ToolRegistry('/tmp', async () => { approved = true; return true; }, false);
  const result = await reg.execute({ toolCallId: '1', toolName: 'bash', input: { command: 'rm -rf /' } }, new AbortController().signal);
  assert.equal(result.denied, true);
  assert.equal(approved, false);
  assert.match(result.text, /critical/i);
});

test('risk: high risk tool requires an approval callback', async () => {
  let approvals = 0;
  const reg = new ToolRegistry('/tmp', async () => { approvals++; return true; }, false);
  reg.registerJson('danger_tool', { description: 'danger', schema: { type: 'object' }, execute: async () => 'ok', readOnly: false, risk: 'high' });
  const result = await reg.execute({ toolCallId: '1', toolName: 'danger_tool', input: {} }, new AbortController().signal);
  assert.equal(result.text, 'ok');
  assert.equal(approvals, 1);
});

test('risk: rejection loop marks third identical denial', async () => {
  const reg = new ToolRegistry('/tmp', async () => false, false);
  reg.rejectionGuard = {
    count: 0,
    async recordDenied() { this.count++; return this.count; },
    async reset() { this.count = 0; },
  };
  const signal = new AbortController().signal;
  const first = await reg.execute({ toolCallId: '1', toolName: 'bash', input: { command: 'echo hi' } }, signal);
  assert.equal(first.denied, true);
  assert.equal(first.loop, undefined);
  const second = await reg.execute({ toolCallId: '2', toolName: 'bash', input: { command: 'echo hi' } }, signal);
  assert.equal(second.loop, undefined);
  const third = await reg.execute({ toolCallId: '3', toolName: 'bash', input: { command: 'echo hi' } }, signal);
  assert.equal(third.loop, true);
});

test('risk: successful execution resets rejection streak', async () => {
  const reg = new ToolRegistry('/tmp', async () => true, false);
  reg.registerJson('medium_tool', { description: 'medium', schema: { type: 'object' }, execute: async () => 'ok', readOnly: false, risk: 'medium' });
  const guard = {
    count: 2,
    async recordDenied() { this.count++; return this.count; },
    async reset() { this.count = 0; },
  };
  reg.rejectionGuard = guard;
  const result = await reg.execute({ toolCallId: '1', toolName: 'medium_tool', input: {} }, new AbortController().signal);
  assert.equal(result.text, 'ok');
  assert.equal(guard.count, 0);
});

test('risk: trusted medium auto-approves without calling approve', async () => {
  let approved = false;
  const reg = new ToolRegistry('/tmp', async () => { approved = true; return true; }, false);
  reg.registerJson('mcp_tool', { description: 'mcp', schema: { type: 'object' }, execute: async () => 'ok', readOnly: false, risk: 'medium', trusted: true });
  const result = await reg.execute({ toolCallId: '1', toolName: 'mcp_tool', input: {} }, new AbortController().signal);
  assert.equal(result.text, 'ok');
  assert.equal(approved, false);
});

test('risk: exact allowlist normalizes and rejects shell metacharacters', () => {
  assert.equal(bashApprovalExact({ command: '  npm   test ' }), 'npm test');
  assert.equal(bashApprovalExact({ command: 'echo "hi"' }), undefined);
  assert.equal(bashApprovalExact({ command: 'echo hi; rm -rf x' }), undefined);
  assert.equal(bashApprovalExact({ command: 'echo $(id)' }), undefined);
  const set = new Set(['npm test']);
  assert.equal(isApprovedBashExact({ command: 'npm test' }, set), true);
  assert.equal(isApprovedBashExact({ command: 'npm test --watch' }, set), false);
});

test('risk: canAutoApprove refuses high and critical even in auto mode', () => {
  assert.equal(canAutoApprove('auto', 'high'), false);
  assert.equal(canAutoApprove('auto', 'critical'), false);
  assert.equal(canAutoApprove('auto', 'medium'), true);
  assert.equal(canAutoApprove('ask', 'low'), false);
});

test('risk: red-team package scripts stay low only for known safe scripts', () => {
  assert.equal(classifyBashRisk('npm test'), 'low');
  assert.equal(classifyBashRisk('npm run build'), 'low');
  assert.equal(classifyBashRisk('npm run deploy'), 'medium');
  assert.equal(classifyBashRisk('npm test && curl https://evil.example'), 'high');
  // Known issue 7 (accepted product decision, documented in CHANGELOG/README): in auto mode the
  // pinned safe package scripts run without a prompt. This is accepted because auto mode already
  // auto-approves `node <script>` (medium), and `npm test` additionally executes the package.json
  // lifecycle scripts the model can edit in the same session — the residual risk is equivalent
  // and explicitly accepted. What must NEVER inherit that trust:
  // - unknown/lifecycle scripts (`npm run pretest`, `npm run deploy`) -> medium,
  // - flag-redirected invocations (`npm --prefix <dir> test`) -> medium, never low,
  // - arbitrary execution (`npm exec`, `npx`, `pnpx`) -> high,
  // - any compound with a network/destructive segment -> high (and high is never auto-approved
  //   or remembered in the session allowlist).
  assert.equal(classifyBashRisk('npm run pretest'), 'medium');
  assert.equal(classifyBashRisk('npm run posttest'), 'medium');
  assert.equal(classifyBashRisk('npm --prefix /tmp/evil test'), 'medium');
  assert.equal(classifyBashRisk('yarn --cwd /tmp/evil test'), 'medium');
  assert.equal(classifyBashRisk('npm exec foo'), 'high');
  assert.equal(classifyBashRisk('npx foo'), 'high');
  assert.equal(classifyBashRisk('make evil-target'), 'medium');
  assert.equal(canAutoApprove('auto', classifyBashRisk('npm test')), true);
  assert.equal(canAutoApprove('auto', classifyBashRisk('npm run build')), true);
  // Residual accepted risk: a lifecycle script is medium, same posture as `node <script>`.
  assert.equal(canAutoApprove('auto', classifyBashRisk('npm run pretest')), true);
  assert.equal(canAutoApprove('auto', classifyBashRisk('npm test && curl https://evil.example')), false);
  assert.equal(canAutoApprove('auto', classifyBashRisk('npm exec foo')), false);
  assert.equal(classifyBashRisk('npm run build && curl https://evil.example'), 'high');
});

test('risk: workflow critical shell step refuses before callback', async () => {
  let called = false;
  const result = await executeWorkflow(
    { name: 'w', description: 'd', scope: 'builtin', steps: [{ id: 's', type: 'shell', command: 'rm -rf /' }] },
    '',
    { cwd: '/tmp', signal: new AbortController().signal, approveShell: async () => { called = true; return true; }, runPrompt: async () => '' },
  );
  assert.equal(called, false);
  assert.equal(result.status, 'failed');
  assert.match(result.finalOutput, /critical/i);
});

test('risk: workflow missing approval handler is an explicit error', async () => {
  const result = await executeWorkflow(
    { name: 'w', description: 'd', scope: 'builtin', steps: [{ id: 's', type: 'shell', command: 'echo hi' }] },
    '',
    { cwd: '/tmp', signal: new AbortController().signal, runPrompt: async () => '' },
  );
  assert.equal(result.status, 'failed');
  assert.match(result.finalOutput, /explicit approval/i);
});

test('risk: workflow shell high risk passes high to the callback', async () => {
  let received;
  const result = await executeWorkflow(
    { name: 'w', description: 'd', scope: 'builtin', steps: [{ id: 's', type: 'shell', command: 'curl https://example.com' }] },
    '',
    { cwd: '/tmp', signal: new AbortController().signal, approveShell: async (_command, _signal, risk) => { received = risk; return false; }, runPrompt: async () => '' },
  );
  assert.equal(received, 'high');
  assert.equal(result.status, 'failed');
});