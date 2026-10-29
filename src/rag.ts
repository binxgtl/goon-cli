/**
 * Lexical RAG for repo understanding. This is intentionally *not* semantic search: text is hashed
 * into fixed-size term-frequency vectors (FNV-1a, "lexical (hashed term-frequency)"), so retrieval
 * is deterministic, offline and free. A real embedder can later replace `HashEmbedder` through the
 * `Embedder` seam without touching callers.
 *
 * Indexing is defensive by design: symlinks are skipped, files outside the workspace root are
 * skipped, sensitive paths (credentials) are skipped, file contents are redacted before chunking,
 * and binary/non-text or oversized files are skipped.
 */
import { createHash } from 'node:crypto';
import { constants as fsConstants } from 'node:fs';
import { lstat, mkdir, open, readdir, readFile, realpath } from 'node:fs/promises';
import { dirname, isAbsolute, join, relative, resolve } from 'node:path';
import { atomicJson } from './config.js';
import { isSensitivePath, redactSecrets } from './redact.js';

// ---------------------------------------------------------------------------
// Embedder seam
// ---------------------------------------------------------------------------

export const EMBEDDING_DIM = 384;

export interface Embedder {
  /** Embeds each text into a fixed-length vector. Implementations must be deterministic. */
  embed(texts: string[], signal?: AbortSignal): Promise<number[][]>;
}

/** FNV-1a 32-bit; stable across platforms and runs. */
export function fnv1a(text: string): number {
  let hash = 0x811c9dc5;
  for (let i = 0; i < text.length; i++) {
    hash ^= text.charCodeAt(i);
    hash = Math.imul(hash, 0x01000193);
  }
  return hash >>> 0;
}

const TOKEN_RE = /[a-z0-9_]+/g;

/** Lowercase alphanumeric/underscore tokens of length >= 2. */
export function tokenize(text: string): string[] {
  return (text.toLowerCase().match(TOKEN_RE) ?? []).filter(token => token.length >= 2);
}

/**
 * Hashed term-frequency embedder: each token lands in one of `EMBEDDING_DIM` buckets (sign taken
 * from the high bit to soften bucket collisions), then the vector is L2-normalized so cosine
 * similarity reduces to a dot product.
 */
export class HashEmbedder implements Embedder {
  readonly dim = EMBEDDING_DIM;

  async embed(texts: string[], _signal?: AbortSignal): Promise<number[][]> {
    return texts.map(text => this.embedOne(text));
  }

  private embedOne(text: string): number[] {
    const vector: number[] = Array.from({ length: EMBEDDING_DIM }, () => 0);
    for (const token of tokenize(text)) {
      const hash = fnv1a(token);
      const sign = (hash >>> 31) & 1 ? -1 : 1;
      vector[hash % EMBEDDING_DIM] += sign;
    }
    let norm = 0;
    for (const value of vector) norm += value * value;
    norm = Math.sqrt(norm);
    if (norm > 0) {
      for (let i = 0; i < EMBEDDING_DIM; i++) vector[i] /= norm;
    }
    return vector;
  }
}

export function cosineSimilarity(a: readonly number[], b: readonly number[]): number {
  const length = Math.min(a.length, b.length);
  let dot = 0;
  let normA = 0;
  let normB = 0;
  for (let i = 0; i < length; i++) {
    dot += a[i] * b[i];
    normA += a[i] * a[i];
    normB += b[i] * b[i];
  }
  if (normA === 0 || normB === 0) return 0;
  return dot / (Math.sqrt(normA) * Math.sqrt(normB));
}

// ---------------------------------------------------------------------------
// Chunking
// ---------------------------------------------------------------------------

export interface Chunk {
  /** Posix-style path relative to the workspace root. */
  path: string;
  /** 1-based inclusive line range. */
  startLine: number;
  endLine: number;
  content: string;
}

export interface RetrievedChunk extends Chunk {
  score: number;
}

export interface ChunkOptions {
  /** Preferred chunk size in characters; larger chunks are re-split into windows. */
  softLimitChars?: number;
  /** Absolute maximum chunk size in characters. */
  hardLimitChars?: number;
  /** Fallback window size in lines. */
  windowLines?: number;
  /** Overlap between fallback windows in lines. */
  overlapLines?: number;
}

const DEFAULT_CHUNK_OPTIONS = {
  softLimitChars: 1500,
  hardLimitChars: 4000,
  windowLines: 40,
  overlapLines: 8,
};

type Language = 'ts' | 'js' | 'go' | 'python' | 'unknown';

function languageFor(path: string): Language {
  const base = path.split('/').pop() ?? path;
  if (/\.(?:ts|tsx|mts|cts)$/.test(base)) return 'ts';
  if (/\.(?:js|jsx|mjs|cjs)$/.test(base)) return 'js';
  if (base.endsWith('.go')) return 'go';
  if (base.endsWith('.py')) return 'python';
  return 'unknown';
}

const DECLARATION_PATTERNS: Record<Exclude<Language, 'unknown'>, RegExp> = {
  ts: /^(?:export\s+)?(?:default\s+)?(?:declare\s+)?(?:abstract\s+)?(?:async\s+)?(?:function\s*\*?\s*\w+|(?:class|interface|enum|namespace)\s+\w+|type\s+\w+\s*=|(?:const|let|var)\s+\w+\s*=\s*(?:async\s*)?(?:function\b|\())/,
  js: /^(?:export\s+)?(?:default\s+)?(?:async\s+)?(?:function\s*\*?\s*\w+|class\s+\w+|(?:const|let|var)\s+\w+\s*=\s*(?:async\s*)?(?:function\b|\())/,
  go: /^(?:func\s+(?:\([^)]*\)\s*)?\w+|type\s+\w+\s+(?:struct|interface)\b)/,
  python: /^\s*(?:async\s+)?def\s+\w+|^\s*class\s+\w+/,
};

function trimBlankLines(lines: string[]): { lines: string[]; offset: number } {
  let start = 0;
  let end = lines.length;
  while (start < end && lines[start].trim() === '') start++;
  while (end > start && lines[end - 1].trim() === '') end--;
  return { lines: lines.slice(start, end), offset: start };
}

function makeChunk(path: string, lines: string[], startIdx: number, endIdx: number): Chunk | undefined {
  const trimmed = trimBlankLines(lines.slice(startIdx, endIdx));
  if (!trimmed.lines.length) return undefined;
  return {
    path,
    startLine: startIdx + trimmed.offset + 1,
    endLine: startIdx + trimmed.offset + trimmed.lines.length,
    content: trimmed.lines.join('\n'),
  };
}

/** Splits a chunk into line windows with overlap, then enforces the hard character cap. */
function splitChunk(chunk: Chunk, options: typeof DEFAULT_CHUNK_OPTIONS): Chunk[] {
  if (chunk.content.length <= options.softLimitChars) return [chunk];
  const lines = chunk.content.split('\n');
  const stride = Math.max(1, options.windowLines - options.overlapLines);
  // makeChunk takes 0-based indices into `lines` and reports 1-based line numbers relative to the
  // whole chunk; shift them into absolute file line numbers once, here.
  const lineOffset = chunk.startLine - 1;
  const out: Chunk[] = [];
  for (let i = 0; i < lines.length; i += stride) {
    const slice = lines.slice(i, i + options.windowLines);
    if (!slice.length) break;
    // 0-based window bounds over the full `lines` array: i is the window start, and the window is
    // `slice.length` lines long (shorter for the final window). Passing 1-based line numbers here
    // would slice past the window and silently drop every line after the first window.
    let piece: Chunk | undefined = makeChunk(chunk.path, lines, i, i + slice.length);
    if (!piece) continue;
    piece = { ...piece, startLine: piece.startLine + lineOffset, endLine: piece.endLine + lineOffset };
    // A window can still exceed the hard cap when lines are very long; split it at line
    // boundaries. A single line longer than the cap is kept whole (documented limitation) and is
    // pushed exactly once.
    if (piece.content.length > options.hardLimitChars) {
      const pieceLines: string[] = piece.content.split('\n');
      let take = 0;
      while (take < pieceLines.length) {
        let length = pieceLines[take]!.length;
        let next = take + 1;
        while (next < pieceLines.length && length + 1 + pieceLines[next]!.length <= options.hardLimitChars) {
          length += 1 + pieceLines[next]!.length;
          next++;
        }
        out.push({
          path: piece.path,
          startLine: piece.startLine + take,
          endLine: piece.startLine + next - 1,
          content: pieceLines.slice(take, next).join('\n'),
        });
        take = next;
      }
    } else {
      out.push(piece);
    }
    if (i + options.windowLines >= lines.length) break;
  }
  return out;
}

/**
 * Chunks a file's text. TS/JS/Go/Python files are split at function/class declarations detected by
 * regex; files without recognizable declarations (or unknown languages) fall back to fixed
 * 40-line windows with 8-line overlap. Chunks over the soft limit are re-split; the hard limit is
 * only exceeded by a single line longer than the cap.
 */
export function chunkFile(path: string, content: string, options: ChunkOptions = {}): Chunk[] {
  const resolved = { ...DEFAULT_CHUNK_OPTIONS, ...options };
  const lines = content.split(/\r?\n/);
  const language = languageFor(path);
  const declarationLines: number[] = [];
  if (language !== 'unknown') {
    const pattern = DECLARATION_PATTERNS[language];
    for (let i = 0; i < lines.length; i++) {
      if (pattern.test(lines[i])) declarationLines.push(i);
    }
  }
  const raw: Chunk[] = [];
  if (declarationLines.length > 0) {
    for (const [index, start] of declarationLines.entries()) {
      const end = index + 1 < declarationLines.length ? declarationLines[index + 1] : lines.length;
      const chunk = makeChunk(path, lines, start, end);
      if (chunk) raw.push(chunk);
    }
  } else {
    const stride = Math.max(1, resolved.windowLines - resolved.overlapLines);
    for (let i = 0; i < lines.length; i += stride) {
      const chunk = makeChunk(path, lines, i, Math.min(i + resolved.windowLines, lines.length));
      if (!chunk) break;
      raw.push(chunk);
      if (i + resolved.windowLines >= lines.length) break;
    }
  }
  return raw.flatMap(chunk => splitChunk(chunk, resolved));
}

// ---------------------------------------------------------------------------
// .goonignore (gitignore subset)
// ---------------------------------------------------------------------------

export interface GoonignoreRule {
  pattern: string;
  negated: boolean;
  dirOnly: boolean;
  anchored: boolean;
  regex: RegExp;
}

function escapeRegex(text: string): string {
  return text.replace(/[.+^${}()|[\]\\]/g, '\\$&');
}

function translatePattern(pattern: string): string {
  let out = '';
  let i = 0;
  while (i < pattern.length) {
    const char = pattern[i];
    if (pattern.startsWith('/**/', i)) {
      // `a/**/b` matches `a/b` and any depth below `a/` (zero or more directories).
      out += '(?:/.*)?/';
      i += 4;
      continue;
    }
    if (pattern.startsWith('**/', i)) {
      out += '(?:.*/)?';
      i += 3;
      continue;
    }
    if (pattern.startsWith('/**', i)) {
      // Trailing `/**` matches everything below the directory.
      out += '/.*';
      i += 3;
      continue;
    }
    if (char === '*') {
      // A lone `*` stays within one path segment (subset).
      out += '[^/]*';
      i += 1;
      continue;
    }
    if (char === '?') {
      out += '[^/]';
      i += 1;
      continue;
    }
    out += escapeRegex(char);
    i += 1;
  }
  return out;
}

/** Parses `.goonignore` content: comments, blanks, negation `!`, trailing slash (dir-only), star, question mark, and the leading double-star glob. */
export function parseGoonignore(text: string): GoonignoreRule[] {
  const rules: GoonignoreRule[] = [];
  for (const rawLine of text.split(/\r?\n/)) {
    const line = rawLine.trim();
    if (!line || line.startsWith('#')) continue;
    let pattern = line;
    let negated = false;
    if (pattern.startsWith('!')) {
      negated = true;
      pattern = pattern.slice(1);
    }
    if (!pattern) continue;
    let dirOnly = false;
    if (pattern.endsWith('/')) {
      dirOnly = true;
      pattern = pattern.slice(0, -1);
    }
    if (!pattern) continue;
    const anchored = pattern.includes('/');
    const cleaned = pattern.startsWith('/') ? pattern.slice(1) : pattern;
    if (!cleaned) continue;
    const body = translatePattern(cleaned);
    const regex = new RegExp(anchored ? `^${body}$` : `^(?:.*/)?${body}$`);
    rules.push({ pattern: line, negated, dirOnly, anchored, regex });
  }
  return rules;
}

/**
 * Gitignore-style last-match-wins evaluation: a pattern matches the file itself or any of its
 * parent directories (`dirOnly` rules only match directories). Later rules override earlier ones,
 * so `!keep.txt` can re-include inside an ignored directory.
 */
export function isIgnored(relPath: string, rules: readonly GoonignoreRule[]): boolean {
  const clean = relPath.replace(/\\/g, '/').replace(/^\/+/, '');
  if (!clean) return false;
  const segments = clean.split('/');
  let ignored = false;
  for (let depth = 1; depth <= segments.length; depth++) {
    const prefix = segments.slice(0, depth).join('/');
    const isDirectory = depth < segments.length;
    for (const rule of rules) {
      if (rule.dirOnly && !isDirectory) continue;
      if (rule.regex.test(prefix)) ignored = !rule.negated;
    }
  }
  return ignored;
}

// ---------------------------------------------------------------------------
// Repo walking + indexRepo
// ---------------------------------------------------------------------------

/** Directories skipped on a cold start (never indexed, never walked). */
export const DEFAULT_SKIP_DIRS = new Set([
  '.git', 'node_modules', 'vendor', 'dist', 'build', 'out', '.next', '.cache', '.turbo',
  'coverage', '.venv', 'venv', '__pycache__', '.idea', '.vscode', 'target', 'bin', 'obj',
]);

export interface WalkOptions {
  ignoreRules?: readonly GoonignoreRule[];
  maxFileBytes?: number;
  maxFiles?: number;
  /** Extra directories to skip (merged with DEFAULT_SKIP_DIRS). */
  skipDirs?: Iterable<string>;
}

export interface WalkedFile {
  /** Posix-style path relative to root. */
  path: string;
  absolutePath: string;
  size: number;
  mtimeMs: number;
}

export interface WalkResult {
  files: WalkedFile[];
  truncated: boolean;
}

function isWithinRoot(root: string, target: string): boolean {
  const rel = relative(root, target);
  return rel !== '' && !rel.startsWith('..') && !isAbsolute(rel);
}

async function readGoonignore(root: string): Promise<GoonignoreRule[]> {
  try {
    const raw = await readFile(join(root, '.goonignore'), 'utf8');
    return parseGoonignore(raw);
  } catch {
    return [];
  }
}

/**
 * Walks the workspace for indexable text files. Symlinks are skipped, files whose real path
 * escapes the workspace root are skipped, `.goonignore` rules and sensitive paths are skipped.
 * Sorted for deterministic ordering.
 */
export async function walkRepo(root: string, options: WalkOptions = {}): Promise<WalkResult> {
  const skipDirs = new Set(DEFAULT_SKIP_DIRS);
  for (const dir of options.skipDirs ?? []) skipDirs.add(dir);
  const rules = options.ignoreRules ?? await readGoonignore(root);
  const maxFileBytes = options.maxFileBytes ?? 1_048_576;
  const maxFiles = options.maxFiles ?? 2000;
  const files: WalkedFile[] = [];
  let truncated = false;

  let rootReal = root;
  try {
    rootReal = await realpath(root);
  } catch {
    rootReal = resolve(root);
  }

  const visit = async (dir: string, relDir: string, depth: number): Promise<void> => {
    if (files.length >= maxFiles) {
      truncated = true;
      return;
    }
    if (depth > 32) return;
    let entries;
    try {
      entries = await readdir(dir, { withFileTypes: true });
    } catch {
      return;
    }
    entries.sort((a, b) => a.name.localeCompare(b.name));
    for (const entry of entries) {
      if (files.length >= maxFiles) {
        truncated = true;
        return;
      }
      const rel = relDir ? `${relDir}/${entry.name}` : entry.name;
      if (entry.isDirectory()) {
        if (skipDirs.has(entry.name)) continue;
        if (isIgnored(rel, rules)) continue;
        await visit(join(dir, entry.name), rel, depth + 1);
        continue;
      }
      if (!entry.isFile()) continue; // symlinks, sockets, fifos
      if (isIgnored(rel, rules)) continue;
      if (isSensitivePath(rel)) continue;
      const absolutePath = join(dir, entry.name);
      let info;
      try {
        info = await lstat(absolutePath);
      } catch {
        continue;
      }
      if (info.isSymbolicLink()) continue;
      if (info.size > maxFileBytes) continue;
      // Defense in depth: even without a symlinked leaf, a renamed/re-linked ancestor could point
      // outside the workspace; the real path must stay within the real root.
      let real = absolutePath;
      try {
        real = await realpath(absolutePath);
      } catch {
        continue;
      }
      if (!isWithinRoot(rootReal, real)) continue;
      files.push({ path: rel, absolutePath, size: info.size, mtimeMs: info.mtimeMs });
    }
  };

  await visit(rootReal, '', 0);
  files.sort((a, b) => a.path.localeCompare(b.path));
  return { files, truncated };
}

/**
 * Opens without following a final symlink (where the platform supports O_NOFOLLOW), requires a
 * regular file within the size cap whose real path is still inside `rootReal`, and reads at most
 * the size observed on the open handle.
 */
async function readWorkspaceFileSafely(path: string, rootReal: string, maxBytes: number): Promise<Buffer | undefined> {
  let handle: Awaited<ReturnType<typeof open>> | undefined;
  try {
    handle = await open(path, fsConstants.O_RDONLY | (fsConstants.O_NOFOLLOW ?? 0));
    const info = await handle.stat();
    if (!info.isFile() || info.size > maxBytes) return undefined;
    if (!isWithinRoot(rootReal, await realpath(path))) return undefined;
    const buffer = Buffer.alloc(info.size);
    let offset = 0;
    while (offset < buffer.length) {
      const { bytesRead } = await handle.read(buffer, offset, buffer.length - offset, offset);
      if (bytesRead === 0) break;
      offset += bytesRead;
    }
    return buffer.subarray(0, offset);
  } catch {
    return undefined;
  } finally {
    await handle?.close().catch(() => {});
  }
}

const BINARY_SNIFF_BYTES = 8192;

/** True when the buffer looks binary (NUL byte in the first 8 KiB, or mostly non-printable). */
export function looksBinary(buffer: Buffer): boolean {
  const sample = buffer.subarray(0, BINARY_SNIFF_BYTES);
  let suspicious = 0;
  for (const byte of sample) {
    if (byte === 0) return true;
    if (byte < 9 || (byte > 13 && byte < 32)) suspicious++;
  }
  return sample.length > 0 && suspicious / sample.length > 0.3;
}

export interface IndexRepoOptions extends WalkOptions {
  embedder?: Embedder;
  secrets?: Iterable<string>;
  chunkOptions?: ChunkOptions;
}

export interface RepoIndexResult {
  snapshot: RagSnapshot;
  files: number;
  chunks: number;
  durationMs: number;
  truncated: boolean;
}

export const RAG_SNAPSHOT_VERSION = 1;

export interface RagFileRecord {
  mtimeMs: number;
  size: number;
  chunks: Chunk[];
}

export interface RagSnapshot {
  version: number;
  cwd: string;
  indexedAt: string;
  files: Record<string, RagFileRecord>;
}

/**
 * Reads, redacts and chunks every eligible file under `root`, producing a persistable snapshot.
 * `redactSecrets` runs on each chunk before anything is stored, so credentials never reach the
 * index or `rag.json`.
 */
export async function indexRepo(root: string, options: IndexRepoOptions = {}): Promise<RepoIndexResult> {
  const startedAt = Date.now();
  // walkRepo already applies maxFileBytes / ignore rules / sensitive-path skips.
  const walk = await walkRepo(root, options);
  const files: Record<string, RagFileRecord> = {};
  let chunkCount = 0;
  let rootReal = resolve(root);
  try { rootReal = await realpath(root); } catch { /* walkRepo used the same fallback */ }
  const maxFileBytes = options.maxFileBytes ?? 1_048_576;
  for (const file of walk.files) {
    // The walk and the read are separate steps: re-check at read time so a file swapped for a
    // symlink (or an ancestor re-linked) in between cannot pull content from outside the root.
    const buffer = await readWorkspaceFileSafely(file.absolutePath, rootReal, maxFileBytes);
    if (!buffer) continue;
    if (looksBinary(buffer)) continue;
    const content = buffer.toString('utf8');
    const chunks = chunkFile(file.path, content, options.chunkOptions)
      .map(chunk => ({ ...chunk, content: redactSecrets(chunk.content, options.secrets) }))
      .filter(chunk => chunk.content.trim().length > 0);
    if (!chunks.length) continue;
    files[file.path] = { mtimeMs: file.mtimeMs, size: file.size, chunks };
    chunkCount += chunks.length;
  }
  return {
    snapshot: {
      version: RAG_SNAPSHOT_VERSION,
      cwd: root,
      indexedAt: new Date().toISOString(),
      files,
    },
    files: Object.keys(files).length,
    chunks: chunkCount,
    durationMs: Date.now() - startedAt,
    truncated: walk.truncated,
  };
}

// ---------------------------------------------------------------------------
// Lexical index (in-memory, linear scan, LRU eviction)
// ---------------------------------------------------------------------------

export interface LexicalEntry {
  id: string;
  chunk: Chunk;
  vector: number[];
  lastAccess: number;
}

export interface LexicalIndexOptions {
  capacity?: number;
  embedder?: Embedder;
  /** Minimum cosine score to return (0 keeps everything). */
  threshold?: number;
}

export interface AddDocument {
  id?: string;
  path: string;
  startLine: number;
  endLine: number;
  content: string;
}

/**
 * In-memory lexical index. Linear-scan cosine retrieval, top-k with deterministic tie-breaking
 * (score desc, then path, then start line), LRU eviction by `lastAccess` when over capacity.
 */
export class LexicalIndex {
  private readonly entries = new Map<string, LexicalEntry>();
  private readonly embedder: Embedder;
  private readonly capacity: number;
  private readonly threshold: number;
  private clock = 0;

  constructor(options: LexicalIndexOptions = {}) {
    this.embedder = options.embedder ?? new HashEmbedder();
    this.capacity = Math.max(1, options.capacity ?? 5000);
    this.threshold = options.threshold ?? 0;
  }

  get size(): number {
    return this.entries.size;
  }

  get maxCapacity(): number {
    return this.capacity;
  }

  /** Adds documents (embedding their content). Evicts least-recently-used entries over capacity. */
  async add(docs: readonly AddDocument[], signal?: AbortSignal): Promise<void> {
    if (!docs.length) return;
    const vectors = await this.embedder.embed(docs.map(doc => doc.content), signal);
    for (const [index, doc] of docs.entries()) {
      const id = doc.id ?? `${doc.path}:${doc.startLine}`;
      this.entries.set(id, {
        id,
        chunk: { path: doc.path, startLine: doc.startLine, endLine: doc.endLine, content: doc.content },
        vector: vectors[index] ?? Array.from({ length: EMBEDDING_DIM }, () => 0),
        lastAccess: ++this.clock,
      });
    }
    this.evict();
  }

  private evict(): void {
    if (this.entries.size <= this.capacity) return;
    const overflow = this.entries.size - this.capacity;
    const candidates = [...this.entries.values()].sort((a, b) => a.lastAccess - b.lastAccess);
    for (const entry of candidates.slice(0, overflow)) this.entries.delete(entry.id);
  }

  /** Removes every entry from one file path. Returns the number of removed entries. */
  removeByPath(path: string): number {
    let removed = 0;
    for (const [id, entry] of this.entries) {
      if (entry.chunk.path === path) {
        this.entries.delete(id);
        removed++;
      }
    }
    return removed;
  }

  clear(): void {
    this.entries.clear();
  }

  async retrieve(query: string, k: number, signal?: AbortSignal): Promise<RetrievedChunk[]> {
    if (!query.trim() || k <= 0 || this.entries.size === 0) return [];
    const [queryVector] = await this.embedder.embed([query], signal);
    if (!queryVector) return [];
    const scored: RetrievedChunk[] = [];
    for (const entry of this.entries.values()) {
      entry.lastAccess = ++this.clock;
      const score = cosineSimilarity(queryVector, entry.vector);
      if (score < this.threshold) continue;
      scored.push({ ...entry.chunk, score });
    }
    scored.sort((a, b) =>
      b.score - a.score ||
      a.path.localeCompare(b.path) ||
      a.startLine - b.startLine,
    );
    return scored.slice(0, Math.max(0, k));
  }

  // -- persistence ----------------------------------------------------------

  /** Serializes chunks without vectors; vectors are recomputed on load. */
  toJSON(): RagSnapshot['files'] {
    const files: RagSnapshot['files'] = {};
    for (const entry of this.entries.values()) {
      const record = files[entry.chunk.path] ?? { mtimeMs: 0, size: 0, chunks: [] };
      record.chunks.push(entry.chunk);
      files[entry.chunk.path] = record;
    }
    return files;
  }

  /** Rebuilds an index from snapshot chunks, recomputing every vector via the embedder. */
  static async fromSnapshot(
    snapshot: RagSnapshot,
    options: LexicalIndexOptions = {},
  ): Promise<LexicalIndex> {
    const index = new LexicalIndex(options);
    const docs: AddDocument[] = [];
    for (const [path, record] of Object.entries(snapshot.files)) {
      for (const chunk of record.chunks) {
        docs.push({ path, startLine: chunk.startLine, endLine: chunk.endLine, content: chunk.content });
      }
    }
    await index.add(docs);
    return index;
  }
}

// ---------------------------------------------------------------------------
// rag.json persistence
// ---------------------------------------------------------------------------

export function ragSnapshotPath(dataDir: string, projectId: string): string {
  return join(dataDir, 'memory', projectId, 'rag.json');
}

export async function loadRagSnapshot(file: string): Promise<RagSnapshot | undefined> {
  try {
    const raw = JSON.parse(await readFile(file, 'utf8'));
    if (!raw || typeof raw !== 'object' || raw.version !== RAG_SNAPSHOT_VERSION) return undefined;
    const files = raw.files ?? {};
    return { version: RAG_SNAPSHOT_VERSION, cwd: String(raw.cwd ?? ''), indexedAt: String(raw.indexedAt ?? ''), files };
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === 'ENOENT') return undefined;
    return undefined;
  }
}

export async function saveRagSnapshot(file: string, snapshot: RagSnapshot): Promise<void> {
  await mkdir(dirname(file), { recursive: true, mode: 0o700 });
  await atomicJson(file, snapshot, { compact: true });
}

// ---------------------------------------------------------------------------
// project id
// ---------------------------------------------------------------------------

/** Stable per-workspace identifier: first 16 hex chars of the sha256 of the absolute cwd. */
export function projectIdFor(cwd: string): string {
  return createHash('sha256').update(resolve(cwd)).digest('hex').slice(0, 16);
}
