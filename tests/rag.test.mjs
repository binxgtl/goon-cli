import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdir, writeFile, symlink, readFile, rm } from 'node:fs/promises';
import { join } from 'node:path';
import {
  EMBEDDING_DIM,
  HashEmbedder,
  LexicalIndex,
  chunkFile,
  cosineSimilarity,
  fnv1a,
  indexRepo,
  isIgnored,
  tokenize,
  loadRagSnapshot,
  looksBinary,
  parseGoonignore,
  projectIdFor,
  ragSnapshotPath,
  saveRagSnapshot,
  walkRepo,
} from '../dist/rag.js';
import { fixture } from './helpers.mjs';

test('rag: fnv1a and embeddings are deterministic, dim 384, L2-normalized', async () => {
  assert.equal(fnv1a('hello'), fnv1a('hello'));
  assert.notEqual(fnv1a('hello'), fnv1a('hellp'));
  const a = new HashEmbedder();
  const b = new HashEmbedder();
  const [va] = await a.embed(['function alpha() { return 1; }']);
  const [vb] = await b.embed(['function alpha() { return 1; }']);
  assert.equal(va.length, EMBEDDING_DIM);
  assert.deepEqual(va, vb);
  let norm = 0;
  for (const value of va) norm += value * value;
  assert.ok(Math.abs(Math.sqrt(norm) - 1) < 1e-9, 'vector must be L2-normalized');
  // Cosine of identical text is 1; unrelated text is well below 1.
  const [v1] = await a.embed(['authentication login password session']);
  const [v2] = await a.embed(['authentication login password session']);
  const [v3] = await a.embed(['rendering viewport css layout']);
  assert.ok(Math.abs(cosineSimilarity(v1, v2) - 1) < 1e-9);
  assert.ok(cosineSimilarity(v1, v3) < 0.5);
});

test('rag: tokenize lowercases, keeps [a-z0-9_]+ and drops tokens shorter than 2', () => {
  assert.deepEqual(tokenize('Hello_world a I code9 x _'), ['hello_world', 'code9']);
});

test('rag: chunkFile splits TS/JS/Go/Python at declarations with 1-based ranges', () => {
  const content = [
    'export function alpha() {',
    '  return 1;',
    '}',
    '',
    'export function beta() {',
    '  return 2;',
    '}',
  ].join('\n');
  const chunks = chunkFile('src/a.ts', content);
  assert.equal(chunks.length, 2);
  assert.equal(chunks[0].startLine, 1);
  assert.equal(chunks[0].endLine, 3);
  assert.match(chunks[0].content, /alpha/);
  assert.equal(chunks[1].startLine, 5);
  assert.equal(chunks[1].endLine, 7);
  const py = chunkFile('m.py', 'class A:\n    def go(self):\n        pass\n\ndef top():\n    return 3\n');
  assert.ok(py.length >= 2);
  assert.match(py[0].content, /class A/);
  const go = chunkFile('m.go', 'func Main() {\n}\n\nfunc (s *S) Aux() {\n}\n');
  assert.equal(go.length, 2);
});

test('rag: chunkFile falls back to 40-line windows with 8-line overlap', () => {
  const lines = Array.from({ length: 100 }, (_, i) => `line ${i + 1}`);
  const chunks = chunkFile('notes.txt', lines.join('\n'));
  assert.equal(chunks[0].startLine, 1);
  assert.equal(chunks[0].endLine, 40);
  // Second window starts 32 lines later (40 - 8 overlap).
  assert.equal(chunks[1].startLine, 33);
  const last = chunks[chunks.length - 1];
  assert.equal(last.endLine, 100);
});

test('rag: chunks over the soft limit are re-split; hard cap bounds every piece', () => {
  const body = Array.from({ length: 200 }, (_, i) => `x = ${i}; // ${'y'.repeat(30)}`);
  const content = `export function big() {\n${body.join('\n')}\n}\n`;
  const chunks = chunkFile('src/big.ts', content);
  assert.ok(chunks.length > 1, 'oversized declaration chunk must be re-split');
  for (const chunk of chunks) {
    assert.ok(chunk.content.length <= 4000, `chunk exceeds hard cap: ${chunk.content.length}`);
  }
  // Every chunk keeps a valid 1-based range inside the file.
  const total = content.split('\n').length;
  for (const chunk of chunks) {
    assert.ok(chunk.startLine >= 1 && chunk.endLine <= total);
  }
});

// Regression (reviewer finding): splitChunk passed 1-based line numbers as 0-based slice
// indices, silently dropping lines after the first window and pushing an oversized line twice.
test('rag: a single line over the soft limit still produces exactly one chunk', () => {
  const line = `const x = "${'A'.repeat(1600)}";`;
  const chunks = chunkFile('long.ts', `${line}\n`);
  assert.equal(chunks.length, 1, JSON.stringify(chunks));
  assert.equal(chunks[0].startLine, 1);
  assert.equal(chunks[0].endLine, 1);
  assert.equal(chunks[0].content, line, 'the whole line is kept so the file is indexed');
});

test('rag: a single line over the hard cap is emitted once, never duplicated or dropped', () => {
  const line = 'X'.repeat(6000);
  const chunks = chunkFile('huge.ts', `function f() {\n${line}\n}\n`);
  assert.deepEqual(chunks.map(chunk => [chunk.startLine, chunk.endLine]), [[1, 1], [2, 2], [3, 3]], JSON.stringify(chunks));
  const oversized = chunks.filter(chunk => chunk.content === line);
  assert.equal(oversized.length, 1, 'the oversized line is pushed exactly once');
  // No duplicate identical chunks (the old bug emitted two identical [2,2] chunks).
  const keys = chunks.map(chunk => `${chunk.startLine}:${chunk.endLine}:${chunk.content}`);
  assert.equal(new Set(keys).size, keys.length, 'chunks are unique');
});

test('rag: a declaration chunk spanning more than two windows covers every line', () => {
  // 122 lines, ~40 chars each (~4.9 KiB > soft 1500): the old bug produced only [2,40] and
  // [34,40], dropping line 1 and lines 41-122 from the index.
  const body = Array.from({ length: 120 }, (_, i) => `  const v${i} = ${i} + "pad-pad-pad";`);
  const content = `function big() {\n${body.join('\n')}\n}\n`;
  const chunks = chunkFile('big.ts', content);
  assert.ok(chunks.length >= 3, `expected overlapping windows, got ${JSON.stringify(chunks.map(c => [c.startLine, c.endLine]))}`);
  const covered = new Set();
  for (const chunk of chunks) {
    for (let line = chunk.startLine; line <= chunk.endLine; line++) covered.add(line);
  }
  const total = content.split('\n').length - 1; // trailing newline
  const missing = [];
  for (let line = 1; line <= total; line++) {
    if (!covered.has(line)) missing.push(line);
  }
  assert.deepEqual(missing, [], 'every line of the oversized function is indexed');
  assert.equal(chunks[0].startLine, 1, 'the first window starts at line 1');
  assert.equal(chunks.at(-1).endLine, total, 'the last window ends at the last line');
});

test('rag: goonignore subset (comments, blank, negation, dir-only, globs, anchored)', () => {
  const rules = parseGoonignore([
    '# comment line',
    '',
    'node_modules',
    'build/',
    '*.log',
    '!important.log',
    'data/**/*.tmp',
    '?temp.txt',
    '/anchored.txt',
  ].join('\n'));
  assert.equal(isIgnored('node_modules/pkg/index.js', rules), true);
  assert.equal(isIgnored('src/build/out.js', rules), true); // dir-only matches a directory segment
  assert.equal(isIgnored('debug.log', rules), true);
  assert.equal(isIgnored('sub/debug.log', rules), true);
  assert.equal(isIgnored('important.log', rules), false); // negation wins (last match)
  assert.equal(isIgnored('data/x/y.tmp', rules), true);
  assert.equal(isIgnored('data/x.tmp', rules), true);
  assert.equal(isIgnored('xtemp.txt', rules), true);
  assert.equal(isIgnored('temp.txt', rules), false); // ? matches exactly one char
  assert.equal(isIgnored('anchored.txt', rules), true);
  assert.equal(isIgnored('sub/anchored.txt', rules), false); // leading slash anchors to root
  assert.equal(isIgnored('src/index.ts', rules), false);
});

test('rag: indexRepo skips sensitive paths, ignore rules, binary, oversize; redacts chunks', async t => {
  const f = await fixture(t);
  await mkdir(join(f.cwd, 'src'), { recursive: true });
  await mkdir(join(f.cwd, 'node_modules', 'pkg'), { recursive: true });
  await mkdir(join(f.cwd, 'secrets'), { recursive: true });
  await mkdir(join(f.cwd, 'skipme'), { recursive: true });
  await writeFile(join(f.cwd, 'src', 'auth.ts'), 'export function login(user: string) {\n  const apiKey = "sk-live-abcdefgh";\n  return user;\n}\n');
  await writeFile(join(f.cwd, '.env'), 'GOON_API_KEY=supersecret\n');
  await writeFile(join(f.cwd, '.env.local'), 'X=1\n');
  await writeFile(join(f.cwd, 'server.pem'), '-----BEGIN\n');
  await writeFile(join(f.cwd, 'secrets', 'key.txt'), 'hidden\n');
  await writeFile(join(f.cwd, 'node_modules', 'pkg', 'index.js'), 'module.exports = 1;\n');
  await writeFile(join(f.cwd, 'skipme', 'a.txt'), 'skipped by goonignore\n');
  await writeFile(join(f.cwd, 'binary.bin'), Buffer.from([0x00, 0x01, 0x02, 0x03]));
  await writeFile(join(f.cwd, 'huge.txt'), 'z'.repeat(2048));
  await writeFile(join(f.cwd, '.goonignore'), 'skipme/\n.goonignore\n');

  const result = await indexRepo(f.cwd, { maxFileBytes: 1024, secrets: ['sk-live-abcdefgh'] });
  const paths = Object.keys(result.snapshot.files);
  assert.deepEqual(paths, ['src/auth.ts']);
  const chunk = result.snapshot.files['src/auth.ts'].chunks[0];
  assert.ok(!chunk.content.includes('sk-live-abcdefgh'), 'literal secret must be redacted');
  assert.ok(chunk.content.includes('[REDACTED]'), 'key=value secret pattern must be redacted');
  assert.ok(result.files === 1 && result.chunks >= 1);
  assert.equal(result.truncated, false);
});

test('rag: walkRepo skips symlinks and links pointing outside the root', async t => {
  const f = await fixture(t);
  const outside = join(f.root, 'outside');
  await mkdir(join(f.cwd, 'src'), { recursive: true });
  await mkdir(outside, { recursive: true });
  await writeFile(join(f.cwd, 'src', 'real.ts'), 'export const real = 1;\n');
  await writeFile(join(outside, 'leak.txt'), 'must not be indexed\n');
  let linkMade = false;
  try {
    await symlink(join(outside, 'leak.txt'), join(f.cwd, 'link.txt'));
    await symlink(outside, join(f.cwd, 'linkdir'), 'junction');
    linkMade = true;
  } catch {
    t.skip('symlink creation not permitted on this platform');
  }
  if (!linkMade) return;
  const walk = await walkRepo(f.cwd);
  assert.deepEqual(walk.files.map(file => file.path), ['src/real.ts']);
});

test('rag: rag.json snapshot round-trip recomputes vectors on load', async t => {
  const f = await fixture(t);
  await mkdir(join(f.cwd, 'src'), { recursive: true });
  await writeFile(join(f.cwd, 'src', 'login.ts'), 'export function login(user: string) {\n  return user;\n}\n');
  const file = ragSnapshotPath(f.data, 'proj1');
  const result = await indexRepo(f.cwd, {});
  await saveRagSnapshot(file, result.snapshot);
  const loaded = await loadRagSnapshot(file);
  assert.ok(loaded && loaded.version === 1);
  const index = await LexicalIndex.fromSnapshot(loaded, { capacity: 100 });
  assert.ok(index.size > 0);
  const hits = await index.retrieve('login', 3);
  assert.ok(hits.length > 0 && hits[0].score > 0);
  // Snapshot format keeps {version, cwd, indexedAt, files:{path:{mtimeMs,size,chunks}}}
  const raw = JSON.parse(await readFile(file, 'utf8'));
  for (const record of Object.values(raw.files)) {
    assert.ok(typeof record.mtimeMs === 'number' && typeof record.size === 'number' && Array.isArray(record.chunks));
    for (const chunk of record.chunks) {
      assert.ok(!('vector' in chunk), 'vectors are not persisted, only recomputed');
    }
  }
});

test('rag: LexicalIndex ranks matching docs first with deterministic ties', async () => {
  const index = new LexicalIndex({ capacity: 10 });
  await index.add([
    { path: 'src/render.ts', startLine: 1, endLine: 2, content: 'rendering viewport css layout engine' },
    { path: 'src/auth.ts', startLine: 1, endLine: 2, content: 'authentication login password session tokens' },
    { path: 'src/auth2.ts', startLine: 1, endLine: 2, content: 'authentication login password session tokens' },
  ]);
  const hits = await index.retrieve('authentication password', 2);
  assert.equal(hits.length, 2);
  assert.equal(hits[0].path, 'src/auth.ts'); // deterministic tie-break by path
  assert.equal(hits[1].path, 'src/auth2.ts');
  assert.ok(hits[0].score > 0.4);
});

test('rag: LexicalIndex evicts least-recently-used over capacity', async () => {
  const index = new LexicalIndex({ capacity: 3, threshold: 0.4 });
  await index.add([
    { path: 'a.txt', startLine: 1, endLine: 1, content: 'alphaunique token one' },
    { path: 'b.txt', startLine: 1, endLine: 1, content: 'betaunique token two' },
    { path: 'c.txt', startLine: 1, endLine: 1, content: 'gammaunique token three' },
  ]);
  assert.equal(index.size, 3);
  await index.add([{ path: 'd.txt', startLine: 1, endLine: 1, content: 'deltaunique token four' }]);
  assert.equal(index.size, 3, 'capacity enforced');
  const gone = await index.retrieve('alphaunique', 5);
  assert.equal(gone.length, 0, 'oldest entry evicted');
  const kept = await index.retrieve('deltaunique', 5);
  assert.equal(kept.length, 1);
  index.removeByPath('d.txt');
  assert.equal(index.size, 2);
  index.clear();
  assert.equal(index.size, 0);
});

test('rag: threshold filters low-score hits', async () => {
  const index = new LexicalIndex({ threshold: 0.4 });
  await index.add([
    { path: 'a.txt', startLine: 1, endLine: 1, content: 'authentication login password session' },
    { path: 'b.txt', startLine: 1, endLine: 1, content: 'rendering viewport css layout' },
  ]);
  assert.equal((await index.retrieve('authentication login', 5)).length, 1);
  assert.equal((await index.retrieve('completely unrelated quantum banana', 5)).length, 0);
});

test('rag: looksBinary detects NUL bytes', () => {
  assert.equal(looksBinary(Buffer.from('plain text file')), false);
  assert.equal(looksBinary(Buffer.from([0x70, 0x00, 0x71])), true);
});

test('rag: projectIdFor is a stable 16-hex workspace id', () => {
  const id = projectIdFor('C:/some/workspace');
  assert.match(id, /^[0-9a-f]{16}$/);
  assert.equal(id, projectIdFor('C:/some/workspace'));
  assert.notEqual(id, projectIdFor('C:/other/workspace'));
});

test('rag: walkRepo caps files and reports truncation', async t => {
  const f = await fixture(t);
  await mkdir(join(f.cwd, 'd'), { recursive: true });
  for (let i = 0; i < 5; i++) await writeFile(join(f.cwd, 'd', `f${i}.txt`), 'x\n');
  const walk = await walkRepo(f.cwd, { maxFiles: 3 });
  assert.equal(walk.files.length, 3);
  assert.equal(walk.truncated, true);
  await rm(join(f.cwd, 'd'), { recursive: true, force: true });
});
