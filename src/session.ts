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

export const sessionGoalSchema = z.object({
  objective: z.string().min(1),
  status: z.enum(['active', 'paused', 'complete']).default('active'),
  createdAt: z.string().datetime(),
  updatedAt: z.string().datetime(),
  tokensAtStart: z.number().nonnegative().default(0),
  tokenBudget: z.number().positive().optional(),
});
export type SessionGoal = z.infer<typeof sessionGoalSchema>;

const sessionSchema = z.object({
  version: z.literal(1), id: z.string().uuid(), cwd: z.string(),
  title: z.string().optional(),
  createdAt: z.string().datetime(), updatedAt: z.string().datetime(),
  provider: z.string(), model: z.string(), baseUrl: z.string(),
  messages: z.array(modelMessageSchema), tokens: z.number().nonnegative(), compactions: z.number().int().nonnegative(),
  checkpoints: z.array(turnCheckpointSchema).optional(),
  todos: z.array(todoItemSchema).optional(),
  goal: sessionGoalSchema.optional(),
});
export type Session = z.infer<typeof sessionSchema>;
export type SessionSummary = Pick<Session, 'id' | 'cwd' | 'updatedAt' | 'model' | 'title'> & {
  messagesCount?: number;
  preview?: string;
};

function firstUserPreview(messages: ModelMessage[]): string | undefined {
  for (const m of messages) {
    if (m.role !== 'user') continue;
    const raw = typeof m.content === 'string'
      ? m.content
      : Array.isArray(m.content)
        ? m.content.map(p => ('text' in p && typeof p.text === 'string' ? p.text : '')).join(' ')
        : '';
    const cleaned = raw.replace(/<referenced_file[\s\S]*?<\/referenced_file>/g, '').replace(/\s+/g, ' ').trim();
    if (cleaned) return cleaned.slice(0, 72);
  }
  return undefined;
}

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

export function sameWorkspace(a: string, b: string): boolean {
  const na = a.normalize('NFC').replace(/[\\/]+$/, '');
  const nb = b.normalize('NFC').replace(/[\\/]+$/, '');
  return process.platform === 'win32' ? na.toLowerCase() === nb.toLowerCase() : na === nb;
}

export class SessionStore {
  private closed = false;
  private constructor(readonly root: string, readonly session: Session, private readonly lock: string) {}
  static async open(root: string, input: { cwd: string; provider: string; model: string; baseUrl: string }, resume?: string, excludeId?: string): Promise<SessionStore> {
    await mkdir(root, { recursive: true, mode: 0o700 });
    let id = resume?.trim();
    if (id === 'latest') {
      const list = await SessionStore.list(root);
      const wsList = list.filter(s => sameWorkspace(s.cwd, input.cwd) && s.id !== excludeId);
      id = wsList.find(s => (s.messagesCount ?? 0) > 0)?.id
        ?? wsList[0]?.id
        ?? list.find(s => sameWorkspace(s.cwd, input.cwd) && (s.messagesCount ?? 0) > 0)?.id
        ?? list.find(s => sameWorkspace(s.cwd, input.cwd))?.id;
    } else if (id && /^\d{1,3}$/.test(id)) {
      // List index (goon resume 3). Longer all-digit input is an ID prefix: ~2.3% of UUIDs start with 8 digits.
      const idx = Number(id) - 1;
      const list = await SessionStore.list(root);
      const wsNonEmpty = list.filter(s => sameWorkspace(s.cwd, input.cwd) && (s.messagesCount ?? 0) > 0 && s.id !== excludeId);
      const wsAll = list.filter(s => sameWorkspace(s.cwd, input.cwd) && s.id !== excludeId);
      const candidates = wsNonEmpty.length ? wsNonEmpty : wsAll;
      id = candidates[idx]?.id ?? id;
    } else if (id && !z.string().uuid().safeParse(id).success && /^[0-9a-f-]{4,35}$/i.test(id)) {
      const prefix = id.toLowerCase();
      const list = await SessionStore.list(root);
      id = list.find(s => sameWorkspace(s.cwd, input.cwd) && s.id.toLowerCase().startsWith(prefix))?.id
        ?? list.find(s => s.id.toLowerCase().startsWith(prefix))?.id
        ?? id;
    }
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
      if (session.id !== id || !sameWorkspace(await realpath(session.cwd), input.cwd)) throw new Error('Session belongs to a different workspace.');
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
        const preview = firstUserPreview(s.messages);
        results.push({
          id: s.id,
          cwd: s.cwd,
          updatedAt: s.updatedAt,
          model: s.model,
          messagesCount: s.messages.length,
          ...(s.title ? { title: s.title } : {}),
          ...(preview ? { preview } : {}),
        });
      } catch { /* Invalid/incomplete entries never become resumable sessions. */ }
    }
    return results.sort((a, b) => b.updatedAt.localeCompare(a.updatedAt));
  }
  static async deleteSession(root: string, idOrPrefix: string): Promise<string | undefined> {
    const trimmed = idOrPrefix.trim().toLowerCase();
    if (!trimmed) return undefined;
    const list = await SessionStore.list(root);
    const match = list.find(s => s.id.toLowerCase() === trimmed || s.id.toLowerCase().startsWith(trimmed));
    if (!match) return undefined;
    try {
      await unlink(join(root, `${match.id}.json`));
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code !== 'ENOENT') throw error;
    }
    try {
      await unlink(join(root, `${match.id}.lock`));
    } catch {
      // ignore
    }
    return match.id;
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
  async discardIfEmpty() {
    if (this.session.messages.length === 0 && !this.session.title) {
      try { await unlink(join(this.root, `${this.session.id}.json`)); }
      catch (error) { if ((error as NodeJS.ErrnoException).code !== 'ENOENT') throw error; }
    }
  }
  async close() {
    if (this.closed) return;
    this.closed = true;
    try { await unlink(this.lock); }
    catch (error) { if ((error as NodeJS.ErrnoException).code !== 'ENOENT') throw error; }
  }
}


