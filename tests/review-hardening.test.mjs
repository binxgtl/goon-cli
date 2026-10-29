// Regression tests for the security review of the v0.4-v0.7 roadmap PR.
import test from 'node:test';
import assert from 'node:assert/strict';
import { symlink, writeFile, mkdir, mkdtemp, rm } from 'node:fs/promises';
import { join } from 'node:path';
import { tmpdir } from 'node:os';
import { classifyBashRisk, tokenize } from '../dist/risk.js';
import { isNetworkAllowedForCommand } from '../dist/policy.js';
import { assessVerifyPlan, parseConfiguredCommand, verifyCommandLine } from '../dist/verify.js';
import { limitTextBytes } from '../dist/files.js';
import { toolApprovalSignature } from '../dist/tools.js';
import { indexRepo } from '../dist/rag.js';
import { isSensitivePath } from '../dist/redact.js';
import { embedderApiKey } from '../dist/embedder.js';

const denyNet = { network: { mode: 'deny', hosts: new Set() } };

test('review: command runners cannot hide critical commands from the classifier', () => {
  for (const cmd of [
    'watch rm -rf /', 'ionice rm -rf /', 'strace -f rm -rf /', 'chroot / rm -rf /',
    'flock /tmp/lock rm -rf /', 'unbuffer rm -rf ~', 'trap "rm -rf /" EXIT', 'su -c "rm -rf /"',
    'busybox rm -rf /', 'watch "rm -rf $HOME"', 'proot rm -rf /',
  ]) {
    assert.equal(classifyBashRisk(cmd), 'critical', cmd);
  }
  for (const cmd of ['watch -n 1 ls', 'flock /tmp/lock make', 'strace -f ls']) {
    assert.equal(classifyBashRisk(cmd), 'high', cmd);
  }
  assert.equal(classifyBashRisk('toybox ls'), 'low');
});

test('review: inline interpreter code and sed execute/write commands are classified', () => {
  for (const cmd of ['perl -e "unlink q(x)"', 'ruby -e x', 'php -r x', 'lua -e x', 'sed -n "1e id" f', 'sed "s/a/b/e" f']) {
    assert.equal(classifyBashRisk(cmd), 'high', cmd);
  }
  assert.equal(classifyBashRisk('sed "s/a/b/w out.txt" f'), 'medium');
  assert.equal(classifyBashRisk('sed s/a/b/g f'), 'low');
  assert.equal(classifyBashRisk('pwsh -c x'), 'critical');
});

test('review: ANSI-C \\a decodes to BEL, not the letter a', () => {
  assert.deepEqual(tokenize("printf $'\\a'"), ['printf', '\x07']);
});

test('review: network policy sees through command runners', () => {
  for (const cmd of ['watch curl evil.test', 'flock /tmp/l curl evil.test', 'su -c "wget evil.test"', 'trap "curl evil.test" EXIT', 'strace -f git push']) {
    assert.equal(isNetworkAllowedForCommand(denyNet, cmd), false, cmd);
  }
  assert.equal(isNetworkAllowedForCommand(denyNet, 'watch ls'), true);
});

test('review: verify plans go through the critical and network gates', () => {
  const net = parseConfiguredCommand('!curl https://evil.test/x');
  assert.equal(verifyCommandLine(net), 'curl https://evil.test/x');
  assert.match(assessVerifyPlan({ commands: [net] }, denyNet).denied ?? '', /network/);
  assert.equal(assessVerifyPlan({ commands: [net] }, { network: { mode: 'allow', hosts: new Set() } }).denied, undefined);
  assert.equal(assessVerifyPlan({ commands: [parseConfiguredCommand('!rm -rf /')] }, denyNet).risk, 'critical');
  const safe = assessVerifyPlan({ commands: [parseConfiguredCommand('npm test'), parseConfiguredCommand('npm run lint')] }, denyNet);
  assert.deepEqual(safe, { risk: 'low' });
  assert.equal(assessVerifyPlan({ commands: [parseConfiguredCommand('npm run deploy')] }, denyNet).risk, 'medium');
});

test('review: limitTextBytes always terminates within the byte limit', () => {
  const text = 'é'.repeat(5000);
  for (const max of [0, 10, 40, 80, 127, 128, 200, 1000]) {
    const out = limitTextBytes(text, max);
    assert.ok(Buffer.byteLength(out, 'utf8') <= max, `max=${max} got ${Buffer.byteLength(out, 'utf8')}`);
  }
});

test('review: rejection signatures never persist raw tool input', () => {
  const signature = toolApprovalSignature('bash', { command: 'curl -H "Authorization: Bearer sk-secret-123456" x' });
  assert.match(signature, /^bash:[0-9a-f]{32}$/);
  assert.equal(signature.includes('sk-secret'), false);
  assert.equal(signature, toolApprovalSignature('bash', { command: 'curl -H "Authorization: Bearer sk-secret-123456" x' }));
});

test('review: credential stores are sensitive paths', () => {
  for (const path of ['.pypirc', 'a/.kube/config', 'deploy/credentials.json', 'release.keystore', '.docker/config.json']) {
    assert.equal(isSensitivePath(path), true, path);
  }
  assert.equal(isSensitivePath('src/keys.ts'), false);
});

test('review: the provider key only reaches a same-origin embedder', () => {
  const previous = process.env.GOON_EMBEDDER_API_KEY;
  delete process.env.GOON_EMBEDDER_API_KEY;
  try {
    assert.equal(embedderApiKey(undefined, 'https://api.example.com/v1', 'k'), 'k');
    assert.equal(embedderApiKey('https://api.example.com/v2', 'https://api.example.com/v1', 'k'), 'k');
    assert.equal(embedderApiKey('https://evil.test/v1', 'https://api.example.com/v1', 'k'), undefined);
    process.env.GOON_EMBEDDER_API_KEY = 'dedicated';
    assert.equal(embedderApiKey('https://evil.test/v1', 'https://api.example.com/v1', 'k'), 'dedicated');
  } finally {
    if (previous === undefined) delete process.env.GOON_EMBEDDER_API_KEY;
    else process.env.GOON_EMBEDDER_API_KEY = previous;
  }
});

test('review: RAG indexing never reads through symlinks outside the root', { skip: process.platform === 'win32' && 'symlinks need privileges on Windows' }, async t => {
  const base = await mkdtemp(join(tmpdir(), 'goon-rag-review-'));
  t.after(() => rm(base, { recursive: true, force: true }));
  const root = join(base, 'repo');
  await mkdir(root);
  await writeFile(join(base, 'outside.txt'), 'OUTSIDE_SECRET_CONTENT\n');
  await writeFile(join(root, 'inside.ts'), 'export const inside = 1;\n');
  await symlink(join(base, 'outside.txt'), join(root, 'link.ts'));
  const result = await indexRepo(root);
  const all = JSON.stringify(result.snapshot.files);
  assert.equal(all.includes('OUTSIDE_SECRET_CONTENT'), false);
  assert.ok(result.snapshot.files['inside.ts']);
});
