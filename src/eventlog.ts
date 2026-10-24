import {
  closeSync,
  existsSync,
  fchmodSync,
  fsyncSync,
  ftruncateSync,
  mkdirSync,
  openSync,
  readFileSync,
  readSync,
  renameSync,
  statSync,
  unlinkSync,
  writeFileSync,
  writeSync,
} from 'node:fs';
import { basename, dirname, join } from 'node:path';
import { collectSecrets, isSensitivePath, redactObject, redactSecrets } from './redact.js';

/** Bump only when the envelope shape changes in a way old readers cannot understand. */
export const EVENT_LOG_VERSION = 1 as const;

export interface EventEnvelope {
  v: typeof EVENT_LOG_VERSION;
  seq: number;
  at: string;
  sessionId: string;
  type: string;
  event: unknown;
}

export type LoggedEvent = { type: string; [key: string]: unknown };

export interface EnvelopeContext {
  sessionId?: string;
  seq?: number;
  at?: string;
}

export function eventLogPath(dataDir: string, sessionId: string): string {
  return join(dataDir, 'sessions', `${sessionId}.events.jsonl`);
}

export function eventLogSidecarPath(path: string): string {
  return `${path}.seq`;
}

/** Session id encoded in an event log path (`<id>.events.jsonl`). */
export function sessionIdFromEventLogPath(path: string): string {
  const name = basename(path);
  return name.endsWith('.events.jsonl') ? name.slice(0, -'.events.jsonl'.length) : name.replace(/\.[^.]+$/, '');
}

export function normalizeEventEnvelope(value: unknown, ctx: EnvelopeContext = {}): EventEnvelope {
  if (!value || typeof value !== 'object' || Array.isArray(value)) {
    throw new Error('Invalid event envelope: expected an object.');
  }
  const raw = value as Record<string, unknown>;
  if (raw.v !== EVENT_LOG_VERSION) {
    throw new Error(`Unsupported event log version ${String(raw.v)}; expected ${EVENT_LOG_VERSION}.`);
  }
  const seq = typeof raw.seq === 'number' && Number.isSafeInteger(raw.seq) && raw.seq > 0 ? raw.seq : ctx.seq;
  if (!seq) throw new Error('Invalid event envelope: missing or invalid seq.');
  const sessionId = typeof raw.sessionId === 'string' && raw.sessionId ? raw.sessionId : ctx.sessionId;
  if (!sessionId) throw new Error('Invalid event envelope: missing sessionId.');
  if (typeof raw.type !== 'string' || !raw.type) throw new Error('Invalid event envelope: missing type.');
  if (!('event' in raw)) throw new Error('Invalid event envelope: missing event body.');
  const at = typeof raw.at === 'string' && raw.at ? raw.at : ctx.at ?? new Date(0).toISOString();
  return { v: EVENT_LOG_VERSION, seq, at, sessionId, type: raw.type, event: raw.event };
}

/**
 * Holds back a tail of a streamed text/reasoning field so a secret split across two deltas is
 * redacted as one unit. The withheld tail is flushed by `flushAll()` at done/error/close.
 */
export class StreamRedactor {
  private carry = '';
  private readonly known: string[];
  private readonly tailSize: number;
  constructor(secrets?: Iterable<string>) {
    this.known = [...new Set(secrets ?? [])].filter(secret => typeof secret === 'string' && secret.length > 0);
    const longest = this.known.reduce((max, secret) => Math.max(max, secret.length), 0);
    this.tailSize = Math.max(256, longest - 1);
  }
  push(chunk: string): string {
    if (!chunk) return '';
    this.carry += chunk;
    if (this.carry.length <= this.tailSize) return '';
    const cut = this.carry.length - this.tailSize;
    const safe = this.carry.slice(0, cut);
    this.carry = this.carry.slice(cut);
    return redactSecrets(safe, this.known);
  }
  flushAll(): string {
    const tail = this.carry;
    this.carry = '';
    return redactSecrets(tail, this.known);
  }
}

function lastSeqFromText(text: string, startedMidFile: boolean): number | undefined {
  const lines = text.split('\n');
  // A trailing newline yields an empty element; a torn tail yields an unparsable element. Drop it.
  lines.pop();
  if (startedMidFile) lines.shift();
  for (let i = lines.length - 1; i >= 0; i--) {
    const line = lines[i].trim();
    if (!line) continue;
    try {
      const parsed = JSON.parse(line) as { seq?: unknown };
      if (typeof parsed.seq === 'number' && Number.isSafeInteger(parsed.seq) && parsed.seq > 0) return parsed.seq;
    } catch {
      // torn/corrupt line: keep looking earlier
    }
  }
  return undefined;
}

function readTail(file: string, start: number, length: number): string {
  const fd = openSync(file, 'r');
  try {
    const buffer = Buffer.alloc(length);
    let offset = 0;
    while (offset < length) {
      const read = readSync(fd, buffer, offset, length - offset, start + offset);
      if (read <= 0) break;
      offset += read;
    }
    return buffer.subarray(0, offset).toString('utf8');
  } finally {
    closeSync(fd);
  }
}

/**
 * Sequence recovery: the sidecar is authoritative when it is not behind the log, otherwise the
 * tail scan wins (the sidecar is best-effort and may be missing or stale).
 */
export function readLastSeq(path: string): number {
  const scanned = scanLastSeq(path);
  const sidecar = readSidecarSeq(path);
  return sidecar !== undefined && sidecar >= scanned ? sidecar : scanned;
}

/**
 * Scans only the tail (64 KiB, doubling) and falls back to the whole file when a single line is
 * larger than 1 MiB. Never returns 0 for a file with a readable envelope: resuming a session must
 * not reuse sequence numbers.
 */
function scanLastSeq(path: string): number {
  let size = 0;
  try { size = statSync(path).size; } catch { return 0; }
  if (size === 0) return 0;
  let window = Math.min(64 * 1024, size);
  for (;;) {
    const start = Math.max(0, size - window);
    const found = lastSeqFromText(readTail(path, start, size - start), start > 0);
    if (found !== undefined) return found;
    if (start === 0) return 0;
    if (window >= Math.min(1024 * 1024, size)) {
      // One line is bigger than the scan cap: read the whole file, bounded by its size.
      const foundWhole = lastSeqFromText(readTail(path, 0, size), false);
      return foundWhole ?? 0;
    }
    window = Math.min(window * 2, 1024 * 1024, size);
  }
}

function readSidecarSeq(path: string): number | undefined {
  try {
    const raw = readFileSync(eventLogSidecarPath(path), 'utf8').trim();
    if (!/^\d+$/.test(raw)) return undefined;
    const value = Number(raw);
    return Number.isSafeInteger(value) && value >= 0 ? value : undefined;
  } catch {
    return undefined;
  }
}

function writeSidecarSeq(path: string, seq: number): void {
  const sidecar = eventLogSidecarPath(path);
  const temporary = `${sidecar}.${process.pid}.tmp`;
  try {
    writeFileSync(temporary, `${seq}\n`, { mode: 0o600 });
    renameSync(temporary, sidecar);
  } catch {
    try { unlinkSync(temporary); } catch { /* best effort */ }
  }
}

function readAt(fd: number, position: number, length: number): Buffer {
  const buffer = Buffer.alloc(length);
  let offset = 0;
  while (offset < length) {
    const read = readSync(fd, buffer, offset, length - offset, position + offset);
    if (read <= 0) break;
    offset += read;
  }
  return buffer.subarray(0, offset);
}

function isValidEnvelopeLine(line: string): boolean {
  try {
    const parsed = JSON.parse(line) as Record<string, unknown>;
    return Boolean(parsed && typeof parsed === 'object' && !Array.isArray(parsed)
      && parsed.v === EVENT_LOG_VERSION
      && typeof parsed.seq === 'number' && Number.isSafeInteger(parsed.seq) && parsed.seq > 0
      && typeof parsed.type === 'string'
      && typeof parsed.sessionId === 'string'
      && 'event' in parsed);
  } catch {
    return false;
  }
}

/**
 * A crash can leave a half-written line. Appending after it would turn the fragment into
 * mid-file corruption, so drop the fragment (or add the missing newline for a complete line)
 * before opening the writer. Regular readers never mutate the log.
 */
function repairTornTail(path: string, onError: (message: string) => void): void {
  let size = 0;
  try { size = statSync(path).size; } catch { return; }
  if (size === 0) return;
  let fd: number | undefined;
  try {
    fd = openSync(path, 'r+');
    const lastByte = readAt(fd, size - 1, 1);
    const hasNewline = lastByte.length === 1 && lastByte[0] === 0x0a;
    const end = hasNewline ? size - 1 : size;
    let start = 0;
    let window = 64 * 1024;
    let position = end;
    while (position > 0) {
      const from = Math.max(0, position - window);
      const chunk = readAt(fd, from, position - from);
      const newline = chunk.lastIndexOf(0x0a);
      if (newline >= 0) { start = from + newline + 1; break; }
      position = from;
      if (window >= 2 * 1024 * 1024) break;
      window = Math.min(window * 2, 2 * 1024 * 1024);
    }
    const lineLength = end - start;
    if (lineLength <= 0) return;
    const line = readAt(fd, start, lineLength).toString('utf8');
    if (!isValidEnvelopeLine(line)) {
      ftruncateSync(fd, start);
    } else if (!hasNewline) {
      writeSync(fd, '\n', size);
    }
  } catch (error) {
    onError(`cannot repair ${path}: ${error instanceof Error ? error.message : String(error)}`);
  } finally {
    if (fd !== undefined) {
      try { closeSync(fd); } catch { /* ignore */ }
    }
  }
}

export interface EventLogWriterOptions {
  sessionId?: string;
  secrets?: Iterable<string>;
  onError?: (message: string) => void;
  now?: () => Date;
}

const EDIT_TOOLS = new Set(['write_file', 'edit_file', 'apply_patch']);
const PATH_TOKEN_RE = /[^\s"'`,;()[\]{}<>]+/g;
const PRIVATE_KEY_RE = /-----BEGIN [A-Z ]*PRIVATE KEY-----/;

function mentionsSensitivePath(value: unknown): boolean {
  if (typeof value !== 'string' || !value) return false;
  if (PRIVATE_KEY_RE.test(value)) return true;
  if (isSensitivePath(value)) return true;
  for (const match of value.matchAll(PATH_TOKEN_RE)) {
    const token = match[0].replace(/^[+\-*]+/, '').replace(/[.:]+$/, '');
    if (token && isSensitivePath(token)) return true;
  }
  return false;
}

function touchesSensitivePath(event: LoggedEvent): boolean {
  const candidates: unknown[] = [];
  const meta = event.meta;
  if (meta && typeof meta === 'object' && !Array.isArray(meta)) {
    const record = meta as Record<string, unknown>;
    candidates.push(record.summary, record.diff, record.outputPreview);
  }
  if (event.type === 'tool_stream') candidates.push(event.preview);
  return candidates.some(mentionsSensitivePath);
}

/**
 * Keeps a tool event's path/counts but drops file content when the tool touched a sensitive path
 * (`meta.diff`, `meta.outputPreview`, `meta.omittedLines`). Edit-tool stream previews are always
 * blanked because the path may only become visible after content has already streamed.
 */
function stripSensitiveContent(event: LoggedEvent): LoggedEvent {
  if (event.type === 'tool') {
    if (!touchesSensitivePath(event)) return event;
    const meta = event.meta as Record<string, unknown>;
    const safeMeta: Record<string, unknown> = {};
    for (const key of ['kind', 'summary', 'verb', 'added', 'removed', 'durationMs', 'exitCode']) {
      if (key in meta) safeMeta[key] = meta[key];
    }
    return { ...event, meta: safeMeta };
  }
  if (event.type === 'tool_stream') {
    const name = typeof event.name === 'string' ? event.name : '';
    if (EDIT_TOOLS.has(name) || touchesSensitivePath(event)) return { ...event, preview: '' };
  }
  return event;
}

/** Append-only JSONL writer. A write failure is reported once per call and never stops the run. */
export class EventLogWriter {
  private fd: number | undefined;
  private seq: number;
  private sessionId: string;
  private textRedactor: StreamRedactor;
  private reasoningRedactor: StreamRedactor;
  private broken = false;
  private readonly secrets: string[];
  private readonly onError: (message: string) => void;
  private readonly now: () => Date;

  constructor(private filePath: string, options: EventLogWriterOptions = {}) {
    this.sessionId = options.sessionId ?? sessionIdFromEventLogPath(filePath);
    this.secrets = [...collectSecrets(), ...(options.secrets ?? [])];
    this.onError = options.onError ?? (() => {});
    this.now = options.now ?? (() => new Date());
    this.textRedactor = new StreamRedactor(this.secrets);
    this.reasoningRedactor = new StreamRedactor(this.secrets);
    this.repairTornTail();
    this.seq = this.initialSeq();
    this.openFd();
  }

  get path(): string { return this.filePath; }
  get lastSeq(): number { return this.seq; }

  private repairTornTail(): void {
    repairTornTail(this.filePath, this.onError);
    this.broken = false;
  }

  private initialSeq(): number {
    // readLastSeq already prefers a non-stale sidecar and falls back to the tail scan.
    return readLastSeq(this.filePath);
  }

  private openFd(): void {
    try {
      mkdirSync(dirname(this.filePath), { recursive: true, mode: 0o700 });
      this.fd = openSync(this.filePath, 'a', 0o600);
      if (process.platform !== 'win32') {
        try { fchmodSync(this.fd, 0o600); } catch { /* filesystem without mode bits */ }
      }
    } catch (error) {
      this.fd = undefined;
      this.onError(`cannot open ${this.filePath}: ${error instanceof Error ? error.message : String(error)}`);
    }
  }

  append(event: LoggedEvent): void {
    if (this.fd === undefined) return;
    if (this.broken) {
      // A previous write failed mid-line: repair before appending, then rescan the sequence.
      this.repairTornTail();
      this.seq = this.initialSeq();
    }
    // A step's text is complete when its usage arrives; flushing there keeps text before usage.
    if (event.type === 'done' || event.type === 'error' || event.type === 'usage') this.flushStreams();
    const redacted = this.redactStream(stripSensitiveContent(event));
    if (redacted) this.writeEnvelope(redacted);
  }

  private redactStream(event: LoggedEvent): LoggedEvent | undefined {
    if (event.type === 'text' && typeof event.text === 'string') {
      const safe = this.textRedactor.push(event.text);
      return safe ? { ...event, text: safe } : undefined;
    }
    if (event.type === 'reasoning' && typeof event.text === 'string') {
      const safe = this.reasoningRedactor.push(event.text);
      return safe ? { ...event, text: safe } : undefined;
    }
    return event;
  }

  private flushStreams(): void {
    if (this.fd === undefined) return;
    const text = this.textRedactor.flushAll();
    if (text) this.writeEnvelope({ type: 'text', text });
    const reasoning = this.reasoningRedactor.flushAll();
    if (reasoning) this.writeEnvelope({ type: 'reasoning', text: reasoning, source: 'provider' });
  }

  private writeEnvelope(event: LoggedEvent): void {
    if (this.fd === undefined) return;
    const safe = redactObject(event, this.secrets);
    const seq = this.seq + 1;
    const envelope: EventEnvelope = {
      v: EVENT_LOG_VERSION,
      seq,
      at: this.now().toISOString(),
      sessionId: this.sessionId,
      type: typeof safe.type === 'string' ? safe.type : 'unknown',
      event: safe,
    };
    try {
      const payload = Buffer.from(`${JSON.stringify(envelope)}\n`);
      let offset = 0;
      while (offset < payload.length) {
        const written = writeSync(this.fd, payload, offset, payload.length - offset);
        if (written <= 0) throw new Error('short write');
        offset += written;
      }
      fsyncSync(this.fd);
      this.seq = seq;
      writeSidecarSeq(this.filePath, seq);
    } catch (error) {
      this.broken = true;
      this.onError(`cannot append to ${this.filePath}: ${error instanceof Error ? error.message : String(error)}`);
    }
  }

  /** Flushes the withheld stream tail, closes the fd and reopens on the next session's log. */
  switchSession(sessionId: string, path = join(dirname(this.filePath), `${sessionId}.events.jsonl`)): void {
    this.flushStreams();
    this.closeFd();
    this.filePath = path;
    this.sessionId = sessionId;
    this.broken = false;
    this.repairTornTail();
    this.seq = this.initialSeq();
    this.textRedactor = new StreamRedactor(this.secrets);
    this.reasoningRedactor = new StreamRedactor(this.secrets);
    this.openFd();
  }

  close(): void {
    this.flushStreams();
    this.closeFd();
  }

  private closeFd(): void {
    if (this.fd === undefined) return;
    try { closeSync(this.fd); } catch { /* ignore */ }
    this.fd = undefined;
  }
}

export function openEventLog(path: string, options: EventLogWriterOptions = {}): EventLogWriter {
  return new EventLogWriter(path, options);
}

export interface ReadEventLogResult {
  envelopes: EventEnvelope[];
  truncated: boolean;
}

/**
 * Parses a log. A malformed final line is a torn append (`truncated: true`); malformed lines in
 * the middle are corruption and throw. Unknown extra fields are ignored for forward compatibility.
 */
export function readEventLog(path: string): ReadEventLogResult {
  const text = readFileSync(path, 'utf8');
  const lines = text.split('\n');
  let lastNonEmpty = -1;
  for (let i = lines.length - 1; i >= 0; i--) {
    if (lines[i].trim()) { lastNonEmpty = i; break; }
  }
  const envelopes: EventEnvelope[] = [];
  let truncated = false;
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    if (!line.trim()) continue;
    let parsed: unknown;
    try {
      parsed = JSON.parse(line);
    } catch (error) {
      if (i === lastNonEmpty) { truncated = true; break; }
      throw new Error(`Invalid JSON in event log ${path} at line ${i + 1}: ${error instanceof Error ? error.message : String(error)}`);
    }
    try {
      const envelope = normalizeEventEnvelope(parsed, { sessionId: sessionIdFromEventLogPath(path) });
      const previous = envelopes[envelopes.length - 1];
      if (previous && envelope.seq <= previous.seq) {
        throw new Error(`seq must increase (${previous.seq} -> ${envelope.seq})`);
      }
      envelopes.push(envelope);
    } catch (error) {
      if (i === lastNonEmpty) { truncated = true; break; }
      throw new Error(`Invalid event envelope in ${path} at line ${i + 1}: ${error instanceof Error ? error.message : String(error)}`);
    }
  }
  return { envelopes, truncated };
}

/** True when the two envelopes leave a hole in the sequence (reported by replay, not fatal). */
export function findSeqGap(previous: EventEnvelope | undefined, next: EventEnvelope): boolean {
  return Boolean(previous && next.seq !== previous.seq + 1);
}

export function isEventLogPath(path: string): boolean {
  return path.endsWith('.events.jsonl') && existsSync(path);
}
