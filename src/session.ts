import { open, mkdir, readFile, readdir, unlink, realpath } from 'node:fs/promises';
import { join } from 'node:path';
import { randomUUID } from 'node:crypto';
import { modelMessageSchema, type ModelMessage, type ToolResultPart } from 'ai';
import { z } from 'zod';
import { atomicJson } from './config.js';

export const fileBackupSchema = z.object({
  path: z.string(),
  existed: z.boolean(),
  content: z.string().optional(),
  mode: z.number().int().optional(),
  afterHash: z.string().optional(),
});
export type FileBackup = z.infer<typeof fileBackupSchema>;

export const turnCheckpointSchema = z.object({
  id: z.number().int().positive(),
  createdAt: z.string().datetime(),
  prompt: z.string(),
  messageIndex: z.number().int().nonnegative(),
  tokensBefore: z.number().nonnegative(),
  files: z.array(fileBackupSchema),
});
export type TurnCheckpoint = z.infer<typeof turnCheckpointSchema>;

export const todoItemSchema = z.object({
  content: z.string().min(1),
  status: z.enum(['pending', 'in_progress', 'completed']),
  priority: z.enum(['high', 'medium', 'low']).default('medium'),
});
export type TodoItem = z.infer<typeof todoItemSchema>;

const sessionSchema = z.object({
  version: z.literal(1), id: z.string().uuid(), cwd: z.string(),
  title: z.string().optional(),
  createdAt: z.string().datetime(), updatedAt: z.string().datetime(),
  provider: z.string(), model: z.string(), baseUrl: z.string(),
  messages: z.array(modelMessageSchema), tokens: z.number().nonnegative(), compactions: z.number().int().nonnegative(),
  checkpoints: z.array(turnCheckpointSchema).optional(),
  todos: z.array(todoItemSchema).optional(),
});
export type Session = z.infer<typeof sessionSchema>;
export type SessionSummary = Pick<Session, 'id' | 'cwd' | 'updatedAt' | 'model' | 'title'>;


export function repairInterrupted(messages: ModelMessage[]): boolean {
  const pending = new Map<string, string>();
  for (const message of messages) {
    if (!Array.isArray(message.content)) continue;
    for (const part of message.content) {
      if (part.type === 'tool-call' && !part.providerExecuted) pending.set(part.toolCallId, part.toolName);
      if (part.type === 'tool-result') pending.delete(part.toolCallId);
    }
  }
  if (!pending.size) return false;
  const content: ToolResultPart[] = [...pending].map(([toolCallId, toolName]) => ({
    type: 'tool-result', toolCallId, toolName,
    output: { type: 'error-text', value: 'Previous process was interrupted. Execution status unknown; inspect the workspace before retrying.' },
  }));
  messages.push({ role: 'tool', content });
  return true;
}

export class SessionStore {
  private closed = false;
  private constructor(readonly root: string, readonly session: Session, private readonly lock: string) {}
  static async open(root: string, input: { cwd: string; provider: string; model: string; baseUrl: string }, resume?: string): Promise<SessionStore> {
    await mkdir(root, { recursive: true, mode: 0o700 });
    let id = resume;
    if (id === 'latest') id = (await SessionStore.list(root)).find(s => s.cwd === input.cwd)?.id;
    if (resume && !id) throw new Error('No matching session to resume in this workspace.');
    if (id && !z.string().uuid().safeParse(id).success) throw new Error('Invalid session ID.');
    id ??= randomUUID();
    const lock = join(root, `${id}.lock`);
    // Never steal an existing lock. A stale lock can only be removed after its
    // owner PID is proven dead; races fail closed via exclusive creation.
    try { await SessionStore.acquire(lock); }
    catch (error) {
      if ((error as NodeJS.ErrnoException).code !== 'EEXIST') throw error;
      const recovery = `${lock}.recovery`;
      // Serialize stale-lock recovery: two reclaimers must not unlink the new
      // live owner's lock between the first unlink and exclusive reacquisition.
      try { await SessionStore.acquire(recovery); }
      catch { throw new Error('Session is locked or undergoing recovery.'); }
      try {
        const pid = Number(await readFile(lock, 'utf8'));
        if (!Number.isSafeInteger(pid) || pid <= 0) throw new Error(`Invalid session lock: ${lock}`);
        let dead = false;
        try { process.kill(pid, 0); }
        catch (e) { dead = (e as NodeJS.ErrnoException).code === 'ESRCH'; }
        if (!dead) throw new Error('Session is already open in another process.');
        await unlink(lock);
        await SessionStore.acquire(lock);
      } finally { await unlink(recovery); }
      return SessionStore.load(root, input, id, lock, !!resume);
    }
    return SessionStore.load(root, input, id, lock, !!resume);
  }
  private static async acquire(lock: string) {
    const handle = await open(lock, 'wx', 0o600);
    try { await handle.writeFile(String(process.pid)); } finally { await handle.close(); }
  }
  private static async load(root: string, input: { cwd: string; provider: string; model: string; baseUrl: string }, id: string, lock: string, resume: boolean) {
    try {
      const now = new Date().toISOString();
      // Copy an explicit allowlist, never the caller's runtime configuration:
      // structural typing permits extra fields such as credentials at runtime.
      const route = { cwd: input.cwd, provider: input.provider, model: input.model, baseUrl: input.baseUrl };
      const session: Session = resume ? sessionSchema.parse(JSON.parse(await readFile(join(root, `${id}.json`), 'utf8'))) : {
        version: 1, id, ...route, createdAt: now, updatedAt: now, messages: [], tokens: 0, compactions: 0,
      };
      if (session.id !== id || await realpath(session.cwd) !== input.cwd) throw new Error('Session belongs to a different workspace.');
      const store = new SessionStore(root, session, lock);
      repairInterrupted(session.messages);
      // Provider changes are explicit CLI/config choices, not restored secrets.
      Object.assign(session, route);
      await store.save();
      return store;
    } catch (error) { await unlink(lock); throw error; }
  }
  static async list(root: string): Promise<SessionSummary[]> {
    let names: string[];
    try { names = await readdir(root); } catch (error) { if ((error as NodeJS.ErrnoException).code === 'ENOENT') return []; throw error; }
    const results: SessionSummary[] = [];
    for (const name of names.filter(n => /^[0-9a-f-]{36}\.json$/.test(n))) {
      try {
        const s = sessionSchema.parse(JSON.parse(await readFile(join(root, name), 'utf8')));
        results.push({ id: s.id, cwd: s.cwd, updatedAt: s.updatedAt, model: s.model, ...(s.title ? { title: s.title } : {}) });
      } catch { /* Invalid/incomplete entries never become resumable sessions. */ }
    }
    return results.sort((a, b) => b.updatedAt.localeCompare(a.updatedAt));
  }
  async forkToNewFile(title?: string): Promise<string> {
    const forkedId = randomUUID();
    const now = new Date().toISOString();
    const forked: Session = {
      ...structuredClone(this.session),
      id: forkedId,
      createdAt: now,
      updatedAt: now,
      ...(title ? { title } : this.session.title ? { title: `${this.session.title} (fork)` } : {}),
    };
    await atomicJson(join(this.root, `${forkedId}.json`), forked);
    return forkedId;
  }
  async save() {
    this.session.updatedAt = new Date().toISOString();
    await atomicJson(join(this.root, `${this.session.id}.json`), this.session);
  }
  async close() {
    if (this.closed) return;
    this.closed = true;
    try { await unlink(this.lock); }
    catch (error) { if ((error as NodeJS.ErrnoException).code !== 'ENOENT') throw error; }
  }
}

