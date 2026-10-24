/**
 * Real embedder seam (v0.7, P24).
 *
 * The default provider is `hash`: the deterministic offline `HashEmbedder` from `src/rag.ts`,
 * byte-for-byte unchanged. `openai` and `ollama` wrap an HTTP embeddings endpoint through an
 * injectable `fetch` (tests never touch the network) with request batching and a hard timeout.
 *
 * Security invariants:
 * - The API key is only ever placed in the `Authorization` header of an outgoing request. It is
 *   never logged, never echoed in error messages (bodies are scrubbed before surfacing), and
 *   never forwarded to `ollama` (local, keyless) or into any child environment (no child
 *   process is spawned at all).
 * - Failures surface as errors for the memory layer to degrade on (`memory_error`); they never
 *   crash a run.
 */
import { HashEmbedder, type Embedder } from './rag.js';

export type EmbedderProvider = 'hash' | 'openai' | 'ollama';

export interface EmbedderFactoryOptions {
  provider: EmbedderProvider;
  /** Embedding model name for `openai`/`ollama`; ignored by `hash`. */
  model?: string;
  /**
   * API base URL. `openai` defaults to the chat gateway (OpenAI-compatible), `ollama` to
   * `http://127.0.0.1:11434`.
   */
  baseUrl?: string;
  /** Bearer token for `openai` only. Never logged and never sent to `ollama`. */
  apiKey?: string;
  /** Injectable fetch for tests; defaults to `globalThis.fetch`. */
  fetchImpl?: typeof fetch;
  /** Per-request timeout; clamped to at least 1s (default 60s). */
  timeoutMs?: number;
  /** Texts per HTTP request (OpenAI-compatible APIs accept arrays). */
  batchSize?: number;
}

export const DEFAULT_OLLAMA_BASE_URL = 'http://127.0.0.1:11434';
export const DEFAULT_EMBEDDER_BATCH_SIZE = 64;
const DEFAULT_EMBEDDER_TIMEOUT_MS = 60_000;

/** Error message built without credentials: status plus an apiKey-scrubbed body snippet. */
export class EmbedderError extends Error {
  constructor(message: string) {
    super(message);
    this.name = 'EmbedderError';
  }
}

function scrub(text: string, secret: string | undefined): string {
  const clean = typeof text === 'string' ? text : String(text);
  return (secret ? clean.split(secret).join('[REDACTED]') : clean).slice(0, 200);
}

function batch<T>(items: readonly T[], size: number): T[][] {
  const out: T[][] = [];
  for (let i = 0; i < items.length; i += size) out.push(items.slice(i, i + size));
  return out;
}

function vectorOf(value: unknown, source: string): number[] {
  if (!Array.isArray(value) || !value.every(item => typeof item === 'number' && Number.isFinite(item))) {
    throw new EmbedderError(`${source} returned a malformed embedding vector.`);
  }
  return value;
}

export interface RemoteEmbedderOptions {
  model: string;
  baseUrl: string;
  apiKey?: string;
  fetchImpl: typeof fetch;
  timeoutMs: number;
  batchSize: number;
  /** Endpoint path appended to baseUrl (no trailing slash on either side). */
  path: string;
  /** Friendly name used in error messages ("openai" / "ollama"). */
  name: 'openai' | 'ollama';
}

/** Shared batching/HTTP machinery for the remote embedders. */
abstract class RemoteEmbedder implements Embedder {
  protected readonly opts: RemoteEmbedderOptions;

  constructor(opts: RemoteEmbedderOptions) {
    this.opts = opts;
  }

  async embed(texts: string[], signal?: AbortSignal): Promise<number[][]> {
    if (!texts.length) return [];
    const vectors: number[][] = [];
    for (const chunk of batch(texts, this.opts.batchSize)) {
      vectors.push(...await this.embedBatch(chunk, signal));
    }
    return vectors;
  }

  protected abstract embedBatch(texts: string[], signal?: AbortSignal): Promise<number[][]>;

  protected async post(body: unknown, signal: AbortSignal | undefined): Promise<unknown> {
    const timeout = AbortSignal.timeout(this.opts.timeoutMs);
    const abort = signal ? AbortSignal.any([signal, timeout]) : timeout;
    let response: Response;
    try {
      response = await this.opts.fetchImpl(`${this.opts.baseUrl}${this.opts.path}`, {
        method: 'POST',
        headers: this.headers(),
        body: JSON.stringify(body),
        signal: abort,
      });
    } catch (error) {
      if (signal?.aborted) throw error;
      throw new EmbedderError(`${this.opts.name} embedder request failed: ${scrub(error instanceof Error ? error.message : String(error), this.opts.apiKey)}`);
    }
    const raw = await response.text().catch(() => '');
    if (!response.ok) {
      throw new EmbedderError(`${this.opts.name} embedder request failed with status ${response.status}: ${scrub(raw, this.opts.apiKey) || '(no body)'}`);
    }
    try {
      return JSON.parse(raw) as unknown;
    } catch {
      throw new EmbedderError(`${this.opts.name} embedder returned non-JSON output: ${scrub(raw, this.opts.apiKey)}`);
    }
  }

  protected headers(): Record<string, string> {
    const headers: Record<string, string> = { 'content-type': 'application/json' };
    // The credential exists only in this header — never in a log line, a thrown message or a
    // child environment (this module spawns nothing).
    if (this.opts.apiKey) headers.authorization = `Bearer ${this.opts.apiKey}`;
    return headers;
  }
}

/** OpenAI-compatible `POST {base}/embeddings` with `{ model, input: string[] }`. */
export class OpenAIEmbedder extends RemoteEmbedder {
  constructor(opts: Omit<RemoteEmbedderOptions, 'path' | 'name'>) {
    super({ ...opts, path: '/embeddings', name: 'openai' });
  }

  protected async embedBatch(texts: string[], signal?: AbortSignal): Promise<number[][]> {
    const payload = await this.post({ model: this.opts.model, input: texts }, signal) as
      { data?: Array<{ index?: number; embedding?: unknown }> } | undefined;
    const data = payload?.data;
    if (!Array.isArray(data) || data.length !== texts.length) {
      throw new EmbedderError(`openai embedder returned ${Array.isArray(data) ? data.length : 'no'} vectors for ${texts.length} input(s).`);
    }
    // The API documents `index` order; map by index when present, fall back to array order.
    const vectors: number[][] = Array.from({ length: texts.length }, () => []);
    for (const [position, entry] of data.entries()) {
      const index = typeof entry.index === 'number' && entry.index >= 0 && entry.index < texts.length
        ? entry.index
        : position;
      vectors[index] = vectorOf(entry.embedding, 'openai embedder');
    }
    return vectors;
  }
}

/** Ollama `POST {base}/api/embed` with `{ model, input: string[] }` → `{ embeddings }`. */
export class OllamaEmbedder extends RemoteEmbedder {
  constructor(opts: Omit<RemoteEmbedderOptions, 'path' | 'name'>) {
    super({ ...opts, path: '/api/embed', name: 'ollama' });
  }

  protected async embedBatch(texts: string[], signal?: AbortSignal): Promise<number[][]> {
    const payload = await this.post({ model: this.opts.model, input: texts }, signal) as
      { embeddings?: unknown[] } | undefined;
    const embeddings = payload?.embeddings;
    if (!Array.isArray(embeddings) || embeddings.length !== texts.length) {
      throw new EmbedderError(`ollama embedder returned ${Array.isArray(embeddings) ? embeddings.length : 'no'} vectors for ${texts.length} input(s).`);
    }
    return embeddings.map(vector => vectorOf(vector, 'ollama embedder'));
  }
}

/**
 * Embedder factory (§8.4): `hash` (default) is unchanged and touches nothing; `openai`/`ollama`
 * require `model`. The caller owns base URL/key selection — the factory never reads env vars or
 * the credential store itself.
 */
export function createEmbedder(options: EmbedderFactoryOptions): Embedder {
  const provider = options.provider ?? 'hash';
  if (provider === 'hash') return new HashEmbedder();
  if (!options.model || !options.model.trim()) {
    throw new EmbedderError(`embedderProvider "${provider}" requires embedderModel to be set.`);
  }
  const baseUrl = (options.baseUrl ?? (provider === 'ollama' ? DEFAULT_OLLAMA_BASE_URL : '')).replace(/\/+$/, '');
  if (!baseUrl) {
    throw new EmbedderError(`embedderProvider "openai" requires embedderBaseUrl (or the chat baseUrl) to be set.`);
  }
  const shared: Omit<RemoteEmbedderOptions, 'path' | 'name'> = {
    model: options.model.trim(),
    baseUrl,
    // `openai` sends the credential as a bearer header; `ollama` is local and keyless, and the
    // key must never reach it.
    ...(provider === 'openai' && options.apiKey ? { apiKey: options.apiKey } : {}),
    fetchImpl: options.fetchImpl ?? ((...args) => globalThis.fetch(...args)),
    timeoutMs: Math.max(1_000, options.timeoutMs ?? DEFAULT_EMBEDDER_TIMEOUT_MS),
    batchSize: Math.max(1, options.batchSize ?? DEFAULT_EMBEDDER_BATCH_SIZE),
  };
  return provider === 'openai' ? new OpenAIEmbedder(shared) : new OllamaEmbedder(shared);
}
