import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile, writeFile, mkdir, rm } from 'node:fs/promises';
import { join } from 'node:path';
import { MemoryStore, WorkingMemory, ExecMemory, RepoMemory, ConversationMemory } from '../dist/memory.js';
import { projectIdFor, ragSnapshotPath } from '../dist/rag.js';
import { isSensitivePath, redactObject, redactSecrets } from '../dist/redact.js';
import { configSchema } from '../dist/config.js';
import { SLASH_COMMANDS } from '../dist/tui.js';
import { fixture } from './helpers.mjs';

const SECRET = 'goon-test-secret-abc123';

async function openStore(t, extra = {}) {
  const f = await fixture(t);
  await mkdir(join(f.cwd, 'src'), { recursive: true });
  await writeFile(join(f.cwd, 'src', 'widget.ts'), 'export function quuxflurb(x: number) {\n  return x * 2;\n}\n');
  const store = await MemoryStore.open({
    dataDir: f.data,
    projectId: projectIdFor(f.cwd),
    configDir: f.config,
    root: f.cwd,
    secrets: [SECRET],
    ...extra,
  });
  return { f, store };
}

test('memory: store exposes the six kinds and conversation is a live view', async t => {
  const { store } = await openStore(t);
  const source = { messages: [{ role: 'user', content: 'hi' }, { role: 'assistant', content: 'hello' }] };
  const conversation = new ConversationMemory(source);
  assert.equal(conversation.size, 2);
  assert.equal(conversation.recent(1).length, 1);
  assert.equal(new ConversationMemory().size, 0);
  assert.ok(store.working instanceof WorkingMemory);
  assert.ok(store.exec instanceof ExecMemory);
  assert.ok(store.repo instanceof RepoMemory);
  assert.ok(store.knowledge && store.preference);
  await store.close();
});

test('memory: handleEvent routes every kind; task_completed writes a lesson and clears working', async t => {
  const { f, store } = await openStore(t);
  await store.handleEvent({ type: 'task_started', task: 'fix the widget bug' });
  assert.match(store.working.snapshot().task, /widget bug/);
  await store.handleEvent({ type: 'state_changed', state: 'investigating', note: 'reading widget.ts' });
  await store.handleEvent({ type: 'tool_result', tool: 'read_file', summary: 'read src/widget.ts' });
  await store.handleEvent({ type: 'patch_applied', path: 'src/widget.ts', bytes: 12 });
  await store.handleEvent({ type: 'verify_result', status: 'pass', issues: 0 });
  await store.handleEvent({ type: 'assistant_message', text: 'Fixed the widget multiplication bug.' });
  assert.ok(store.working.render().includes('verify pass'));
  await store.handleEvent({ type: 'task_completed', outcome: 'done' });
  assert.equal(store.exec.size, 1);
  assert.equal(store.working.snapshot().task, undefined);
  assert.equal(store.working.snapshot().notes.length, 0);
  // patch_applied invalidates the cached repo tree.
  store.repo.refresh('src/widget.ts');
  await store.handleEvent({ type: 'patch_applied', path: 'other.ts', bytes: 1 });
  assert.equal(store.repo.treeSummary(), undefined);
  await store.close();
  const raw = JSON.parse(await readFile(join(f.data, 'memory', projectIdFor(f.cwd), 'memory.json'), 'utf8'));
  assert.equal(raw.version, 1);
  assert.equal(raw.exec.length, 1);
  assert.match(raw.exec[0].text, /task: fix the widget bug/);
});

test('memory: concurrent handleEvent + persist never corrupts (single-writer chain)', async t => {
  const { f, store } = await openStore(t);
  const jobs = [];
  for (let i = 0; i < 30; i++) {
    jobs.push(store.handleEvent({ type: 'tool_result', tool: `tool${i}`, summary: `step ${i}` }));
    if (i % 10 === 0) jobs.push(store.persist());
  }
  jobs.push(store.persist(), store.persist());
  await Promise.all(jobs);
  await store.close();
  const raw = JSON.parse(await readFile(join(f.data, 'memory', projectIdFor(f.cwd), 'memory.json'), 'utf8'));
  assert.ok(Array.isArray(raw.exec));
});

test('memory: exec rolls at 50 entries', async t => {
  const { store } = await openStore(t);
  for (let i = 0; i < 60; i++) store.exec.append(`lesson ${i}`);
  const all = store.exec.all();
  assert.equal(all.length, 50);
  assert.match(all[0].text, /lesson 10/);
  assert.match(store.exec.recent(2)[1].text, /lesson 59/);
  store.exec.clear();
  assert.equal(store.exec.size, 0);
  await store.close();
});

test('memory: preferences live in preferences.json, separate from memories.json', async t => {
  const { f, store } = await openStore(t);
  const memoriesFile = join(f.config, 'memories.json');
  await writeFile(memoriesFile, JSON.stringify([{ id: '1', text: 'keep me', createdAt: '2026-01-01T00:00:00.000Z' }]));
  await store.handleEvent({ type: 'user_preference', key: 'style', value: 'concise answers' });
  await store.handleEvent({ type: 'user_preference', key: 'editor', value: 'vim bindings' });
  await store.close();
  const preferences = JSON.parse(await readFile(join(f.config, 'preferences.json'), 'utf8'));
  assert.equal(preferences.style, 'concise answers');
  assert.equal(preferences.editor, 'vim bindings');
  const memories = JSON.parse(await readFile(memoriesFile, 'utf8'));
  assert.deepEqual(memories, [{ id: '1', text: 'keep me', createdAt: '2026-01-01T00:00:00.000Z' }]);
  assert.equal(store.preference.get('style'), 'concise answers');
  assert.ok(await store.preference.remove('editor'));
  assert.equal(store.preference.get('editor'), undefined);
});

test('memory: everything persisted is redacted (literal secret, bearer, key=value)', async t => {
  const { f, store } = await openStore(t);
  // Working notes are RAM-only, so exercise the paths that actually reach disk: exec lessons
  // (via task_started/task_completed) and knowledge insights (rag.json).
  await store.handleEvent({ type: 'task_started', task: 'curl -H "Authorization: Bearer eyJhbGciOi.J9" and token=plainvalue1 with goon-test-secret-abc123' });
  await store.handleEvent({ type: 'task_completed', outcome: 'done' });
  await store.knowledge.addInsight(`export API_KEY=goon-test-secret-abc123 in prod`);
  await store.persist();
  await store.close();
  const raw = await readFile(join(f.data, 'memory', projectIdFor(f.cwd), 'memory.json'), 'utf8');
  assert.ok(!raw.includes('goon-test-secret-abc123'), 'literal secret must not be persisted');
  assert.ok(!raw.includes('eyJhbGciOi.J9'), 'bearer token must not be persisted');
  assert.ok(!raw.includes('plainvalue1'), 'key=value secret must not be persisted');
  assert.ok(raw.includes('[REDACTED]'));
  assert.ok(raw.includes('API_KEY=[REDACTED]'), 'stored insight text is redacted');
});

test('memory: retrieve returns working notes, repo tree, chunks and relevant preferences only', async t => {
  const { store } = await openStore(t);
  await store.handleEvent({ type: 'task_started', task: 'extend quuxflurb behavior' });
  await store.handleEvent({ type: 'user_preference', key: 'rag-hint', value: 'quuxflurb lives in src/widget.ts' });
  await store.handleEvent({ type: 'tool_result', tool: 'bash', summary: `export TOKEN=${SECRET}` });
  const result = await store.ensureRepoIndexed();
  assert.equal(result.reindexed, 'full');
  store.repo.refresh('src/\n  widget.ts (12 lines)');
  const context = await store.retrieve('quuxflurb', 4000);
  assert.ok(context.workingNotes.includes('extend quuxflurb behavior'));
  assert.ok(context.repoTree.includes('widget.ts'));
  assert.ok(context.rag.length >= 1);
  assert.equal(context.rag[0].path, 'src/widget.ts');
  assert.ok(context.preferences.some(p => p.startsWith('rag-hint:')));
  const serialized = JSON.stringify(context);
  assert.ok(!serialized.includes(SECRET), 'no secret ever leaves the store');
  assert.ok(!serialized.includes('outcome:'), 'exec lessons are not injected raw');
  // Token budget: 0 tokens -> no chunks; conversation/exec never present.
  const none = await store.retrieve('quuxflurb', 0);
  assert.equal(none.rag.length, 0);
  assert.ok(!('conversation' in none) && !('exec' in none));
  await store.close();
});

test('memory: ensureRepoIndexed is idempotent, detects staleness and dedups in-flight calls', async t => {
  const { f, store } = await openStore(t);
  const first = await store.ensureRepoIndexed();
  assert.equal(first.reindexed, 'full');
  assert.ok(first.files >= 1 && first.chunks >= 1);
  const second = await store.ensureRepoIndexed();
  assert.equal(second.reindexed, 'none');
  await writeFile(join(f.cwd, 'src', 'widget.ts'), 'export function quuxflurb(x: number) {\n  return x * 3; // changed\n}\n');
  const third = await store.ensureRepoIndexed();
  assert.equal(third.reindexed, 'partial');
  const [a, b] = await Promise.all([store.ensureRepoIndexed(), store.ensureRepoIndexed()]);
  assert.ok(a.files >= 1 && b.files >= 1);
  assert.ok(await readFile(ragSnapshotPath(f.data, projectIdFor(f.cwd)), 'utf8'));
  const stats = await store.knowledge.stats();
  assert.ok(stats.files >= 1 && stats.chunks >= 1 && stats.indexedAt);
  await store.close();
});

test('memory: reindex forces a full rebuild and insights survive restart', async t => {
  const { f, store } = await openStore(t);
  await store.knowledge.addInsight('The widget entrypoint is src/widget.ts with quuxflurb.');
  await store.knowledge.addInsight('The widget entrypoint is src/widget.ts with quuxflurb.'); // dedup
  await store.close();
  const reopened = await MemoryStore.open({
    dataDir: f.data,
    projectId: projectIdFor(f.cwd),
    configDir: f.config,
    root: f.cwd,
    secrets: [SECRET],
  });
  const result = await reopened.knowledge.reindex(f.cwd);
  assert.equal(result.reindexed, 'full');
  const hits = await reopened.retrieve('quuxflurb entrypoint', 2000);
  assert.ok(hits.rag.some(hit => hit.path === 'memory://insight'), 'insight chunk must be searchable');
  await reopened.close();
  const raw = JSON.parse(await readFile(join(f.data, 'memory', projectIdFor(f.cwd), 'memory.json'), 'utf8'));
  assert.equal(raw.insights.length, 1);
});

test('memory: redactSecrets/isSensitivePath patterns (P17 DoD)', () => {
  // Bearer replacement; with an Authorization prefix the key-value pass over-redacts the
  // scheme too (safe direction), so assert on the secret value rather than the exact wording.
  assert.equal(redactSecrets('Bearer abc123xyzDEF'), 'Bearer [REDACTED]');
  const prefixed = redactSecrets('Authorization: Bearer abc123xyzDEF');
  assert.ok(!prefixed.includes('abc123xyzDEF'));
  assert.ok(prefixed.includes('[REDACTED]'));
  assert.equal(redactSecrets('{"token":"topsecret-value"}'), '{"token":"[REDACTED]"}');
  assert.equal(redactSecrets('api_key = abcdefgh123'), 'api_key = [REDACTED]');
  assert.equal(redactSecrets('call https://x with sk-literal-999 now', ['sk-literal-999']), 'call https://x with [REDACTED] now');
  assert.equal(redactSecrets('GOON_API_KEY=short'), 'GOON_API_KEY=[REDACTED]'); // pattern redaction is conservative
  const obj = redactObject({ apiKey: 'k', nested: { password: 'p', keep: 'v' } });
  assert.equal(obj.apiKey, '[REDACTED]');
  assert.equal(obj.nested.password, '[REDACTED]');
  assert.equal(obj.nested.keep, 'v');
  for (const sensitive of ['.env', '.env.local', '.env.example', 'server.pem', 'id_rsa', 'id_ed25519.pub', 'client.key', '.npmrc', '.netrc', '.git-credentials', '.ssh/id_rsa', 'secrets/a.txt', '.aws/credentials', 'config/gcloud/creds.json']) {
    assert.equal(isSensitivePath(sensitive), true, sensitive);
  }
  for (const normal of ['src/index.ts', 'README.md', 'docs/PLAN.md', 'package.json']) {
    assert.equal(isSensitivePath(normal), false, normal);
  }
});

test('memory: env credentials are auto-collected for redaction (same contract as the event log)', async t => {
  const token = 'envcred-zqxwkj-123456789';
  process.env.GITHUB_TOKEN = token;
  try {
    const { f, store } = await openStore(t);
    // The fake credential appears bare inside an indexed workspace file and in a memory event.
    await writeFile(join(f.cwd, 'src', 'deploy.ts'), `const token = "${token}";\nexport const quuxdeploy = 1;\n`);
    const indexed = await store.ensureRepoIndexed();
    assert.equal(indexed.reindexed, 'full');
    const ragRaw = await readFile(ragSnapshotPath(f.data, projectIdFor(f.cwd)), 'utf8');
    assert.ok(!ragRaw.includes(token), 'env credential must not reach rag.json');
    assert.ok(ragRaw.includes('[REDACTED]'), 'bare env credential is replaced in indexed chunks');
    await store.handleEvent({ type: 'task_started', task: `deploy using ${token}` });
    await store.handleEvent({ type: 'task_completed', outcome: 'done' });
    await store.close();
    const memRaw = await readFile(join(f.data, 'memory', projectIdFor(f.cwd), 'memory.json'), 'utf8');
    assert.ok(!memRaw.includes(token), 'env credential must not reach memory.json');
  } finally {
    delete process.env.GITHUB_TOKEN;
  }
});

test('memory: config defaults keep memory/rag opt-in (backward compatible)', () => {
  const config = configSchema.parse({});
  assert.equal(config.memoryEnabled, false);
  assert.equal(config.ragEnabled, false);
  assert.equal(config.ragTopK, 10);
  assert.equal(config.ragCapacity, 5000);
  assert.equal(config.ragThreshold, 0);
  assert.equal(config.ragIndexMaxFiles, 2000);
  assert.equal(config.ragIndexMaxFileBytes, 1_048_576);
});

test('memory: /memory is registered in the TUI command list next to /memories', () => {
  assert.ok(SLASH_COMMANDS.includes('/memory'));
  assert.ok(SLASH_COMMANDS.indexOf('/memories') < SLASH_COMMANDS.indexOf('/memory'));
});

test('memory: corrupt store files do not prevent opening', async t => {
  const f = await fixture(t);
  const projectDir = join(f.data, 'memory', projectIdFor(f.cwd));
  await mkdir(projectDir, { recursive: true });
  await mkdir(f.config, { recursive: true });
  await writeFile(join(projectDir, 'memory.json'), '{broken json');
  const store = await MemoryStore.open({ dataDir: f.data, projectId: projectIdFor(f.cwd), configDir: f.config, root: f.cwd });
  assert.equal(store.exec.size, 0);
  await store.close();
  await rm(join(f.cwd), { recursive: true, force: true });
});
