/**
 * Memory store for goon (v0.6). Six kinds of memory with one write path:
 *
 * - `working`    — per-turn RAM (task, state, notes); never persisted.
 * - `conversation`— read-only view over the live session messages; never re-retrieved.
 * - `exec`       — per-project rolling list of compact task lessons (max 50), persisted.
 * - `repo`       — per-project cached repo tree summary, persisted.
 * - `knowledge`  — per-project lexical index over the workspace (`rag.json`) + insights.
 * - `preference` — per-user key/value `preferences.json` (separate from `memories.json`).
 *
 * Everything that reaches disk is redacted first (`redactSecrets`). All writes are serialized
 * through one promise chain and land via `atomicJson`, so concurrent `handleEvent`/`persist`
 * calls can never interleave or corrupt a file. A failing memory operation must never fail the
 * run — callers catch and surface `memory_error` events instead.
 */
import { mkdir, readFile } from 'node:fs/promises';
import { join } from 'node:path';
import { atomicJson, configDir } from './config.js';
import { collectSecrets, redactSecrets } from './redact.js';
import {
  chunkFile,
  indexRepo,
  LexicalIndex,
  loadRagSnapshot,
  looksBinary,
  ragSnapshotPath,
  saveRagSnapshot,
  tokenize,
  walkRepo,
  type AddDocument,
  type Embedder,
  type RagSnapshot,
  type RetrievedChunk,
} from './rag.js';

export type MemoryKind = 'working' | 'conversation' | 'exec' | 'repo' | 'knowledge' | 'preference';

export interface MemoryPromptContext {
  rag: RetrievedChunk[];
  repoTree?: string;
  workingNotes?: string;
  preferences?: string[];
}

export type MemoryEvent =
  | { type: 'task_started'; task: string }
  | { type: 'state_changed'; state: string; note?: string }
  | { type: 'task_completed'; outcome: string }
  | { type: 'assistant_message'; text: string }
  | { type: 'tool_result'; tool: string; summary: string }
  | { type: 'patch_applied'; path: string; bytes: number }
  | { type: 'verify_result'; status: string; issues: number }
  | { type: 'user_preference'; key: string; value: string };

const MEMORY_FILE_VERSION = 1;
const EXEC_LIMIT = 50;
const MAX_NOTE_CHARS = 500;
const MAX_TASK_CHARS = 500;
const MAX_LESSON_CHARS = 400;
const MAX_PREFERENCES_IN_CONTEXT = 8;

/** Rough token estimate: ~4 characters per token, conservative for non-ASCII code. */
function estimateTokens(text: string): number {
  return Math.max(1, Math.ceil(text.length / 4));
}

function truncate(text: string, max: number): string {
  const clean = text.replace(/\s+/g, ' ').trim();
  return clean.length > max ? `${clean.slice(0, max - 1)}…` : clean;
}

// ---------------------------------------------------------------------------
// Sub-stores
// ---------------------------------------------------------------------------

export interface WorkingSnapshot {
  task?: string;
  state?: string;
  notes: string[];
}

/** Per-turn RAM: the current task, a small state string and free-form notes. Never persisted. */
export class WorkingMemory {
  private task?: string;
  private state?: string;
  private notes: string[] = [];

  setTask(task: string): void {
    this.task = truncate(task, MAX_TASK_CHARS) || undefined;
  }

  setState(state: string): void {
    this.state = truncate(state, 200) || undefined;
  }

  addNote(note: string): void {
    const clean = truncate(note, MAX_NOTE_CHARS);
    if (clean) this.notes.push(clean);
  }

  snapshot(): WorkingSnapshot {
    return { ...(this.task ? { task: this.task } : {}), ...(this.state ? { state: this.state } : {}), notes: [...this.notes] };
  }

  /** Rendered for prompt injection, or undefined when nothing is tracked. */
  render(): string | undefined {
    const lines: string[] = [];
    if (this.task) lines.push(`Task: ${this.task}`);
    if (this.state) lines.push(`State: ${this.state}`);
    for (const note of this.notes) lines.push(`- ${note}`);
    return lines.length ? lines.join('\n') : undefined;
  }

  clear(): void {
    this.task = undefined;
    this.state = undefined;
    this.notes = [];
  }
}

/** Structural view over live session messages (no copy, no retrieval). */
export interface ConversationSource {
  readonly messages: readonly unknown[];
}

export class ConversationMemory {
  constructor(private readonly source?: ConversationSource) {}

  get size(): number {
    return this.source?.messages.length ?? 0;
  }

  /** Last `n` messages as-is. Conversation memory is a view, never re-embedded or re-ranked. */
  recent(n: number): readonly unknown[] {
    const messages = this.source?.messages ?? [];
    return messages.slice(Math.max(0, messages.length - Math.max(0, n)));
  }
}

export interface ExecLesson {
  text: string;
  at: string;
}

/** Rolling per-project task lessons (max 50). Lessons are compact summaries, never raw output. */
export class ExecMemory {
  private entries: ExecLesson[] = [];

  constructor(private readonly onChange: () => void = () => {}) {}

  get size(): number {
    return this.entries.length;
  }

  append(text: string): void {
    const clean = truncate(text, MAX_LESSON_CHARS);
    if (!clean) return;
    this.entries.push({ text: clean, at: new Date().toISOString() });
    if (this.entries.length > EXEC_LIMIT) this.entries.splice(0, this.entries.length - EXEC_LIMIT);
    this.onChange();
  }

  recent(n: number): ExecLesson[] {
    return this.entries.slice(Math.max(0, this.entries.length - Math.max(0, n)));
  }

  all(): readonly ExecLesson[] {
    return this.entries;
  }

  load(entries: unknown): void {
    if (!Array.isArray(entries)) return;
    this.entries = entries
      .filter(entry => entry && typeof entry === 'object' && typeof (entry as ExecLesson).text === 'string')
      .map(entry => ({ text: String((entry as ExecLesson).text), at: String((entry as ExecLesson).at ?? new Date().toISOString()) }))
      .slice(-EXEC_LIMIT);
  }

  clear(): void {
    this.entries = [];
    this.onChange();
  }
}

/** Per-project repo tree summary cache. Invalidated when the workspace changes. */
export class RepoMemory {
  private tree?: string;
  private refreshedAt?: string;

  constructor(private readonly onChange: () => void = () => {}) {}

  /** Replaces the cached tree summary (already-sized by the caller). */
  refresh(tree: string): void {
    this.tree = tree;
    this.refreshedAt = new Date().toISOString();
    this.onChange();
  }

  /** Drops the cached summary; the next `refresh` rebuilds it. */
  invalidate(): void {
    if (this.tree === undefined) return;
    this.tree = undefined;
    this.refreshedAt = undefined;
    this.onChange();
  }

  treeSummary(): string | undefined {
    return this.tree;
  }

  lastRefreshedAt(): string | undefined {
    return this.refreshedAt;
  }

  load(data: unknown): void {
    if (!data || typeof data !== 'object') return;
    const record = data as { tree?: unknown; refreshedAt?: unknown };
    if (typeof record.tree === 'string') this.tree = record.tree;
    if (typeof record.refreshedAt === 'string') this.refreshedAt = record.refreshedAt;
  }
}

export interface KnowledgeOptions {
  ragFile: string;
  capacity?: number;
  threshold?: number;
  topK?: number;
  maxFiles?: number;
  maxFileBytes?: number;
  embedder?: Embedder;
  secrets?: Iterable<string>;
  enqueue: <T>(job: () => Promise<T>) => Promise<T>;
}

export interface KnowledgeStats {
  files: number;
  chunks: number;
  insights: number;
  indexedAt?: string;
}

export interface EnsureResult {
  files: number;
  chunks: number;
  durationMs: number;
  reindexed: 'full' | 'partial' | 'none';
  truncated: boolean;
}

/**
 * Per-project lexical knowledge base: chunks of workspace files stored in `rag.json` plus
 * distilled insights. Index construction is lazy; `ensureRepoIndexed` is idempotent, dedups
 * in-flight calls and only re-indexes files whose mtime/size changed.
 */
export class KnowledgeMemory {
  private index?: LexicalIndex;
  private snapshot?: RagSnapshot;
  private loadPromise?: Promise<void>;
  private ensurePromise?: Promise<EnsureResult>;
  private dirty = false;
  private readonly topK: number;
  private readonly maxFiles: number;
  private readonly maxFileBytes: number;
  private readonly secrets: string[];
  private readonly enqueue: <T>(job: () => Promise<T>) => Promise<T>;

  constructor(private readonly options: KnowledgeOptions) {
    this.topK = options.topK ?? 10;
    this.maxFiles = options.maxFiles ?? 2000;
    this.maxFileBytes = options.maxFileBytes ?? 1_048_576;
    this.secrets = [...options.secrets ?? []];
    this.enqueue = options.enqueue;
  }

  get size(): number {
    return this.index?.size ?? 0;
  }

  get topKValue(): number {
    return this.topK;
  }

  /** Stats derived from the persisted snapshot (cheap; does not build the in-memory index). */
  async stats(): Promise<KnowledgeStats> {
    await this.ensureSnapshotLoaded();
    const files = Object.keys(this.snapshot?.files ?? {}).length;
    let chunks = 0;
    for (const record of Object.values(this.snapshot?.files ?? {})) chunks += record.chunks.length;
    return {
      files,
      chunks,
      insights: this.insightCount,
      ...(this.snapshot?.indexedAt ? { indexedAt: this.snapshot.indexedAt } : {}),
    };
  }

  private insightCount = 0;
  private insights: string[] = [];

  /** Loads the on-disk snapshot only (no vectors, no embedding). */
  private async ensureSnapshotLoaded(): Promise<void> {
    if (this.snapshot) return;
    this.snapshot = await loadRagSnapshot(this.options.ragFile);
  }

  private async ensureLoaded(): Promise<void> {
    if (this.loadPromise) return this.loadPromise;
    this.loadPromise = (async () => {
      await this.ensureSnapshotLoaded();
      const index = new LexicalIndex({
        capacity: this.options.capacity,
        threshold: this.options.threshold,
        embedder: this.options.embedder,
      });
      const docs: AddDocument[] = [];
      for (const [path, record] of Object.entries(this.snapshot?.files ?? {})) {
        for (const chunk of record.chunks) {
          docs.push({ path, startLine: chunk.startLine, endLine: chunk.endLine, content: chunk.content });
        }
      }
      for (const [i, text] of this.insights.entries()) {
        for (const chunk of chunkFile('memory://insight', text)) {
          docs.push({ id: `insight:${i}:${chunk.startLine}`, path: 'memory://insight', startLine: chunk.startLine, endLine: chunk.endLine, content: chunk.content });
        }
      }
      await index.add(docs);
      this.index = index;
    })();
    try {
      await this.loadPromise;
    } catch (error) {
      this.loadPromise = undefined;
      throw error;
    }
  }

  private async chunkPath(path: string, root: string): Promise<AddDocument[]> {
    const buffer = await readFile(join(root, path));
    if (looksBinary(buffer)) return [];
    const content = buffer.toString('utf8');
    return chunkFile(path, content)
      .map(chunk => ({ ...chunk, content: redactSecrets(chunk.content, this.secrets) }))
      .filter(chunk => chunk.content.trim().length > 0)
      .map(chunk => ({ path: chunk.path, startLine: chunk.startLine, endLine: chunk.endLine, content: chunk.content }));
  }

  /**
   * Idempotent, in-flight-deduped index refresh. Full index when no usable snapshot exists;
   * partial re-index (changed/new/deleted files by mtime/size) otherwise; no-op when nothing
   * changed. Persists `rag.json` on any change.
   */
  async ensureRepoIndexed(root: string): Promise<EnsureResult> {
    if (this.ensurePromise) return this.ensurePromise;
    this.ensurePromise = this.enqueue(async () => {
      await this.ensureLoaded();
      const index = this.index!;
      const startedAt = Date.now();
      let reindexed: EnsureResult['reindexed'] = 'none';
      let truncated = false;
      if (!this.snapshot || this.snapshot.cwd !== root) {
        const result = await indexRepo(root, {
          maxFiles: this.maxFiles,
          maxFileBytes: this.maxFileBytes,
          secrets: this.secrets,
          embedder: this.options.embedder,
        });
        this.snapshot = result.snapshot;
        index.clear();
        const docs: AddDocument[] = [];
        for (const [path, record] of Object.entries(result.snapshot.files)) {
          for (const chunk of record.chunks) {
            docs.push({ path, startLine: chunk.startLine, endLine: chunk.endLine, content: chunk.content });
          }
        }
        await index.add(docs);
        reindexed = 'full';
        truncated = result.truncated;
      } else {
        // Staleness check: stat-only walk, re-index only changed/new files, drop removed ones.
        const walk = await walkRepo(root, { maxFiles: this.maxFiles, maxFileBytes: this.maxFileBytes });
        truncated = walk.truncated;
        const current = new Map(walk.files.map(file => [file.path, file]));
        const changed: string[] = [];
        for (const file of walk.files) {
          const previous = this.snapshot!.files[file.path];
          if (!previous || Math.abs(previous.mtimeMs - file.mtimeMs) > 0.5 || previous.size !== file.size) {
            changed.push(file.path);
          }
        }
        const removed = Object.keys(this.snapshot!.files).filter(path => !current.has(path));
        if (changed.length || removed.length) {
          reindexed = 'partial';
          for (const path of removed) {
            index.removeByPath(path);
            delete this.snapshot!.files[path];
          }
          for (const path of changed) {
            index.removeByPath(path);
            const docs = await this.chunkPath(path, root).catch(() => []);
            if (docs.length) {
              await index.add(docs);
              const info = current.get(path)!;
              this.snapshot!.files[path] = { mtimeMs: info.mtimeMs, size: info.size, chunks: docs.map(doc => ({ path, startLine: doc.startLine, endLine: doc.endLine, content: doc.content })) };
            } else {
              delete this.snapshot!.files[path];
            }
          }
          this.snapshot!.indexedAt = new Date().toISOString();
        }
      }
      if (reindexed !== 'none') {
        this.dirty = true;
        await this.save();
      }
      const files = Object.keys(this.snapshot?.files ?? {}).length;
      let chunks = 0;
      for (const record of Object.values(this.snapshot?.files ?? {})) chunks += record.chunks.length;
      return { files, chunks, durationMs: Date.now() - startedAt, reindexed, truncated };
    });
    try {
      return await this.ensurePromise;
    } finally {
      this.ensurePromise = undefined;
    }
  }

  /** Forces a full re-index regardless of staleness. */
  async reindex(root: string): Promise<EnsureResult> {
    return this.enqueue(async () => {
      await this.ensureLoaded();
      const startedAt = Date.now();
      const result = await indexRepo(root, {
        maxFiles: this.maxFiles,
        maxFileBytes: this.maxFileBytes,
        secrets: this.secrets,
        embedder: this.options.embedder,
      });
      this.snapshot = result.snapshot;
      const index = new LexicalIndex({
        capacity: this.options.capacity,
        threshold: this.options.threshold,
        embedder: this.options.embedder,
      });
      const docs: AddDocument[] = [];
      for (const [path, record] of Object.entries(result.snapshot.files)) {
        for (const chunk of record.chunks) {
          docs.push({ path, startLine: chunk.startLine, endLine: chunk.endLine, content: chunk.content });
        }
      }
      for (const [i, text] of this.insights.entries()) {
        for (const chunk of chunkFile('memory://insight', text)) {
          docs.push({ id: `insight:${i}:${chunk.startLine}`, path: 'memory://insight', startLine: chunk.startLine, endLine: chunk.endLine, content: chunk.content });
        }
      }
      await index.add(docs);
      this.index = index;
      this.dirty = true;
      await this.save();
      return { files: result.files, chunks: result.chunks, durationMs: Date.now() - startedAt, reindexed: 'full' as const, truncated: result.truncated };
    });
  }

  /** Stores a distilled insight as an extra searchable chunk (`memory://insight`). */
  async addInsight(text: string): Promise<void> {
    await this.enqueue(async () => {
      const clean = redactSecrets(text, this.secrets).trim();
      if (!clean) return;
      if (this.insights.includes(clean)) return;
      this.insights.push(clean);
      this.insightCount = this.insights.length;
      await this.ensureLoaded();
      const docs: AddDocument[] = [];
      for (const chunk of chunkFile('memory://insight', clean)) {
        docs.push({ id: `insight:${this.insights.length - 1}:${chunk.startLine}`, path: 'memory://insight', startLine: chunk.startLine, endLine: chunk.endLine, content: chunk.content });
      }
      await this.index?.add(docs);
      this.dirty = true;
    });
  }

  /** Called by MemoryStore after loading memory.json so insights survive restarts. */
  seedInsights(insights: string[]): void {
    this.insights = [...insights];
    this.insightCount = this.insights.length;
  }

  insightList(): readonly string[] {
    return this.insights;
  }

  async retrieve(query: string, k = this.topK, budgetTokens = Number.POSITIVE_INFINITY): Promise<RetrievedChunk[]> {
    if (!query.trim()) return [];
    await this.ensureLoaded();
    const hits = await this.index?.retrieve(query, Math.max(1, k)) ?? [];
    // Respect the token budget: keep highest-ranked chunks that still fit, skip oversized ones.
    const selected: RetrievedChunk[] = [];
    let used = 0;
    for (const hit of hits) {
      const cost = estimateTokens(hit.content);
      if (used + cost > budgetTokens) continue;
      selected.push(hit);
      used += cost;
    }
    return selected;
  }

  /** Writes `rag.json` when the in-memory state is ahead of disk. */
  async save(): Promise<void> {
    if (!this.snapshot) return;
    await saveRagSnapshot(this.options.ragFile, this.snapshot);
    this.dirty = false;
  }

  async saveIfDirty(): Promise<void> {
    if (!this.dirty) return;
    await this.save();
  }
}

export interface PreferenceMemoryData {
  file: string;
  enqueue: <T>(job: () => Promise<T>) => Promise<T>;
}

/** Per-user key/value preferences in `preferences.json` (never touches `memories.json`). */
export class PreferenceMemory {
  private values: Record<string, string> = {};
  readonly file: string;
  private readonly enqueue: <T>(job: () => Promise<T>) => Promise<T>;

  constructor(options: PreferenceMemoryData) {
    this.file = options.file;
    this.enqueue = options.enqueue;
  }

  get(key: string): string | undefined {
    return this.values[key];
  }

  async set(key: string, value: string): Promise<void> {
    const cleanKey = key.trim();
    if (!cleanKey) throw new Error('Preference key must not be empty.');
    await this.enqueue(() => this.applySet(cleanKey, value));
  }

  /** Cache + direct write, for use inside the store's write chain (no nested enqueue). */
  async applySet(key: string, value: string): Promise<void> {
    this.values[key] = value;
    await atomicJson(this.file, this.values);
  }

  all(): Record<string, string> {
    return { ...this.values };
  }

  async remove(key: string): Promise<boolean> {
    if (!(key in this.values)) return false;
    await this.enqueue(async () => {
      delete this.values[key];
      await atomicJson(this.file, this.values);
    });
    return true;
  }

  get size(): number {
    return Object.keys(this.values).length;
  }

  load(data: unknown): void {
    if (!data || typeof data !== 'object' || Array.isArray(data)) return;
    for (const [key, value] of Object.entries(data as Record<string, unknown>)) {
      if (typeof value === 'string') this.values[key] = value;
    }
  }
}

// ---------------------------------------------------------------------------
// MemoryStore
// ---------------------------------------------------------------------------

export interface MemoryFileData {
  version: number;
  exec: ExecLesson[];
  insights: string[];
  repo?: { tree?: string; refreshedAt?: string };
}

export interface MemoryStoreOptions {
  dataDir: string;
  projectId: string;
  capacity?: number;
  configDir?: string;
  /** Workspace root for repo indexing (defaults to `ensureRepoIndexed(root)` callers passing it). */
  root?: string;
  /** Literal secrets redacted before anything is stored. */
  secrets?: Iterable<string>;
  /** Live session message view for conversation memory. */
  conversation?: ConversationSource;
  rag?: {
    topK?: number;
    capacity?: number;
    threshold?: number;
    maxFiles?: number;
    maxFileBytes?: number;
    embedder?: Embedder;
  };
}

export class MemoryStore {
  readonly working = new WorkingMemory();
  readonly conversation: ConversationMemory;
  readonly exec: ExecMemory;
  readonly repo: RepoMemory;
  readonly knowledge: KnowledgeMemory;
  readonly preference: PreferenceMemory;
  readonly projectId: string;
  private readonly root?: string;
  private readonly secrets: string[];
  private readonly memoryFile: string;
  private chain: Promise<void> = Promise.resolve();

  private constructor(options: MemoryStoreOptions) {
    this.projectId = options.projectId;
    this.root = options.root;
    // Same contract as the event log (eventlog.ts merges collectSecrets() into its redactor):
    // credential-looking env vars (`*_API_KEY|*_TOKEN|*_SECRET|*_PASSWORD`) are always merged in
    // so a caller that forgets explicit secrets cannot leak another provider's credential from
    // the environment into rag.json/memory.json or back out through retrieval.
    this.secrets = [...new Set([...collectSecrets(), ...(options.secrets ?? [])])];
    this.conversation = new ConversationMemory(options.conversation);
    this.exec = new ExecMemory();
    this.repo = new RepoMemory();
    this.knowledge = new KnowledgeMemory({
      ragFile: ragSnapshotPath(options.dataDir, options.projectId),
      capacity: options.rag?.capacity ?? options.capacity ?? 5000,
      threshold: options.rag?.threshold,
      topK: options.rag?.topK,
      maxFiles: options.rag?.maxFiles,
      maxFileBytes: options.rag?.maxFileBytes,
      embedder: options.rag?.embedder,
      secrets: this.secrets,
      enqueue: (job) => this.enqueue(job),
    });
    this.preference = new PreferenceMemory({
      file: join(options.configDir ?? configDir(), 'preferences.json'),
      enqueue: (job) => this.enqueue(job),
    });
    this.memoryFile = join(options.dataDir, 'memory', options.projectId, 'memory.json');
  }

  static async open(options: MemoryStoreOptions): Promise<MemoryStore> {
    const store = new MemoryStore(options);
    const memoryDir = join(options.dataDir, 'memory', options.projectId);
    await mkdir(memoryDir, { recursive: true, mode: 0o700 });
    await mkdir(join(options.configDir ?? configDir()), { recursive: true, mode: 0o700 }).catch(() => {});
    let data: MemoryFileData | undefined;
    try {
      data = JSON.parse(await readFile(store.memoryFile, 'utf8')) as MemoryFileData;
    } catch {
      data = undefined;
    }
    store.exec.load(data?.exec);
    store.knowledge.seedInsights(Array.isArray(data?.insights) ? data.insights.filter(item => typeof item === 'string') : []);
    store.repo.load(data?.repo);
    let preferences: unknown;
    try {
      preferences = JSON.parse(await readFile(store.preference.file, 'utf8'));
    } catch {
      preferences = undefined;
    }
    store.preference.load(preferences);
    return store;
  }

  /**
   * Single entry point for memory updates. Events are serialized onto one promise chain
   * (single-writer); a failing event never breaks the chain nor the caller's run.
   */
  handleEvent(event: MemoryEvent): Promise<void> {
    return this.enqueue(async () => {
      await this.applyEvent(event);
      await this.persistNow();
    }).catch(() => {});
  }

  private async applyEvent(event: MemoryEvent): Promise<void> {
    const secrets = this.secrets;
    switch (event.type) {
      case 'task_started':
        this.working.setTask(redactSecrets(event.task, secrets));
        break;
      case 'state_changed':
        this.working.setState(redactSecrets(event.state, secrets));
        if (event.note) this.working.addNote(redactSecrets(event.note, secrets));
        break;
      case 'tool_result':
        this.working.addNote(`tool ${event.tool}: ${redactSecrets(event.summary, secrets)}`);
        break;
      case 'patch_applied':
        this.working.addNote(`edited ${event.path} (${event.bytes} bytes)`);
        this.repo.invalidate();
        break;
      case 'verify_result':
        this.working.addNote(`verify ${event.status} (${event.issues} issue(s))`);
        break;
      case 'assistant_message':
        this.working.addNote(redactSecrets(event.text, secrets));
        break;
      case 'task_completed': {
        const snapshot = this.working.snapshot();
        const parts = [snapshot.task ? `task: ${snapshot.task}` : '', `outcome: ${event.outcome}`].filter(Boolean);
        if (snapshot.notes.length) parts.push(`notes: ${snapshot.notes.slice(-3).join(' | ')}`);
        this.exec.append(redactSecrets(parts.join(' | '), secrets));
        this.working.clear();
        break;
      }
      case 'user_preference': {
        const key = redactSecrets(event.key, secrets).trim();
        const value = redactSecrets(event.value, secrets);
        if (key) await this.preference.applySet(key, value).catch(() => {});
        break;
      }
      default:
        break; // Unknown event kinds are ignored (forward compatibility).
    }
  }

  /**
   * Working notes + repo tree + lexical chunks + relevant preferences. Conversation and exec
   * memories are deliberately excluded (no raw history or raw lessons in the prompt).
   */
  async retrieve(query: string, budgetTokens = 1024): Promise<MemoryPromptContext> {
    const rag = await this.knowledge.retrieve(query, this.knowledge.topKValue, Math.max(0, budgetTokens)).catch(() => [] as RetrievedChunk[]);
    const context: MemoryPromptContext = { rag };
    const workingNotes = this.working.render();
    if (workingNotes) context.workingNotes = workingNotes;
    const repoTree = this.repo.treeSummary();
    if (repoTree) context.repoTree = repoTree;
    const preferences = this.relevantPreferences(query);
    if (preferences.length) context.preferences = preferences;
    return context;
  }

  private relevantPreferences(query: string): string[] {
    const tokens = new Set(tokenize(query));
    if (!tokens.size) return [];
    const hits: string[] = [];
    for (const [key, value] of Object.entries(this.preference.all())) {
      const haystack = `${key} ${value}`.toLowerCase();
      if (([...tokens].some(token => haystack.includes(token)))) {
        hits.push(`${key}: ${value}`);
        if (hits.length >= MAX_PREFERENCES_IN_CONTEXT) break;
      }
    }
    return hits;
  }

  /** Idempotent repo indexing using the root given at `open` (in-flight deduped downstream). */
  ensureRepoIndexed(root?: string): Promise<EnsureResult> {
    const target = root ?? this.root;
    if (!target) return Promise.reject(new Error('MemoryStore.open requires `root` (or pass it to ensureRepoIndexed).'));
    return this.knowledge.ensureRepoIndexed(target);
  }

  /** Serializes `job` after every previously queued write (single-writer). */
  private enqueue<T>(job: () => Promise<T>): Promise<T> {
    const next = this.chain.then(job, job);
    this.chain = next.then(() => {}, () => {});
    return next;
  }

  /** Persists exec/insights/repo (`memory.json`) and the knowledge snapshot (`rag.json`). */
  async persist(): Promise<void> {
    return this.enqueue(async () => {
      await this.persistNow();
    });
  }

  private async persistNow(): Promise<void> {
    const data: MemoryFileData = {
      version: MEMORY_FILE_VERSION,
      exec: this.exec.all().map(lesson => ({ ...lesson })),
      insights: [...this.knowledge.insightList()],
      ...(this.repo.treeSummary() || this.repo.lastRefreshedAt()
        ? { repo: { ...(this.repo.treeSummary() ? { tree: this.repo.treeSummary()! } : {}), ...(this.repo.lastRefreshedAt() ? { refreshedAt: this.repo.lastRefreshedAt()! } : {}) } }
        : {}),
    };
    await atomicJson(this.memoryFile, data, { compact: true });
    await this.knowledge.saveIfDirty();
  }

  /** Flushes pending writes and drains the queue. */
  async close(): Promise<void> {
    try {
      await this.persist();
      await this.chain;
    } catch {
      // Memory must never take the run down.
    }
  }
}
