import { streamText, type LanguageModel, type ModelMessage, type ToolResultPart } from 'ai';
import type { RuntimeConfig } from './config.js';
import { SessionStore, repairInterrupted, type SessionGoal, type TodoItem, type TurnCheckpoint } from './session.js';
import { ToolRegistry, normalizeToolInput, normalizeToolName, type ToolCall, type RunBudget } from './tools.js';
import { auxiliaryEffortHeaders, buildProviderOptions, createModel, resolveModelCapabilities } from './provider.js';
import { safeError } from './errors.js';
import { buildGoalBudgetLimitPrompt, compactIfNeeded, compactNow, estimatedTokens, expandFileMentions, recapSession, systemPrompt, type SystemPromptCache } from './context.js';
import { AnswerProjection, visibleAnswer, withReasoningReplay, type AnswerPart } from './reasoning.js';

export interface ToolEventMeta {
  kind: 'read' | 'list' | 'search' | 'exec' | 'edit' | 'other';
  summary: string;
  durationMs?: number;
  outputPreview?: string[];
  omittedLines?: number;
  diff?: string;
  added?: number;
  removed?: number;
  verb?: 'Added' | 'Edited' | 'Deleted';
}

export type HarnessEvent =
  | { type: 'text'; text: string }
  | { type: 'reasoning'; text: string; source: 'inline' | 'provider' }
  | { type: 'tool_stream'; name: string; id: string; state: 'start' | 'delta'; preview: string }
  | { type: 'tool'; name: string; id: string; state: 'start' | 'done' | 'error'; meta?: ToolEventMeta }
  | { type: 'usage'; input: number; output: number; total: number }
  | { type: 'compaction'; count: number }
  | { type: 'todo'; todos: TodoItem[] }
  | { type: 'goal'; goal?: SessionGoal }
  | { type: 'plan_exit'; plan: string }
  | { type: 'retry'; attempt: number; maxRetries: number; message: string }
  | { type: 'stopped'; reason: string };

export const MAX_API_RETRIES = 3;
/** Consecutive read-only tool calls from one model step that may run concurrently. */
export const MAX_PARALLEL_READS = 6;
/** Retry backoff: base * 2^attempt plus jitter, unless the provider sends Retry-After (capped). */
export const RETRY_POLICY = { baseMs: 250, maxMs: 8_000, maxRetryAfterMs: 20_000 };

function retryDelayMs(error: unknown, attempt: number): number {
  const headers = error && typeof error === 'object' ? (error as { responseHeaders?: Record<string, string> }).responseHeaders : undefined;
  const retryAfter = headers?.['retry-after'] ?? headers?.['Retry-After'];
  if (retryAfter) {
    const seconds = Number(retryAfter);
    const ms = Number.isFinite(seconds) ? seconds * 1000 : Date.parse(retryAfter) - Date.now();
    if (Number.isFinite(ms) && ms > 0) return Math.min(RETRY_POLICY.maxRetryAfterMs, ms);
  }
  const exponential = Math.min(RETRY_POLICY.maxMs, RETRY_POLICY.baseMs * 2 ** attempt);
  return Math.round(exponential + Math.random() * exponential * 0.25);
}

function isNonRetryableApiError(error: unknown, userSignal: AbortSignal): boolean {
  if (userSignal.aborted) return true;
  const errObj = error && typeof error === 'object' ? (error as Record<string, unknown>) : {};
  const status = typeof errObj.statusCode === 'number'
    ? errObj.statusCode
    : typeof errObj.status === 'number'
      ? errObj.status
      : undefined;
  if (status === 400 || status === 401 || status === 403 || status === 404 || status === 422) {
    return true;
  }
  const msg = error instanceof Error ? error.message : String(error);
  if (
    /\b(?:401|403|authentication_error|invalid_api_key|unauthorized)\b/i.test(msg) ||
    msg.startsWith('Model response hit the output limit') ||
    msg.startsWith('Invalid or excessive tool call batch') ||
    msg.startsWith('Model stopped unexpectedly (content-filter)')
  ) {
    return true;
  }
  return false;
}

async function sleepWithAbort(ms: number, signal: AbortSignal): Promise<void> {
  if (ms <= 0) return;
  signal.throwIfAborted();
  await new Promise<void>((resolve, reject) => {
    const timer = setTimeout(() => {
      signal.removeEventListener('abort', onAbort);
      resolve();
    }, ms);
    const onAbort = () => {
      clearTimeout(timer);
      reject(signal.reason ?? new Error('Cancelled by user.'));
    };
    signal.addEventListener('abort', onAbort, { once: true });
  });
}

export interface RevertResult {
  turnsReverted: number;
  messagesRemoved: number;
  restoredFiles: string[];
  deletedFiles: string[];
  revertedPrompts: string[];
  /** Files changed after the agent's turn; left untouched unless the revert was forced. */
  conflictFiles: string[];
  /** Files whose backup content was pruned to bound the session size. */
  unrecoverableFiles: string[];
}

function extractStreamPreview(rawJson: string): string {
  const pathMatch = /"(?:path|filePath|file_path|directory)"\s*:\s*"([^"\\]*(?:\\.[^"\\]*)*)"/.exec(rawJson);
  const cmdMatch = /"(?:command|cmd)"\s*:\s*"([^"\\]*(?:\\.[^"\\]*)*)"/.exec(rawJson);
  const queryMatch = /"(?:query|pattern)"\s*:\s*"([^"\\]*(?:\\.[^"\\]*)*)"/.exec(rawJson);
  const target = (pathMatch?.[1] ?? cmdMatch?.[1] ?? queryMatch?.[1] ?? '').replace(/\\"/g, '"').replace(/\\\\/g, '\\');
  const lines = rawJson.split(/\\n|\n/).length;
  const kb = (Buffer.byteLength(rawJson) / 1024).toFixed(1);
  if (target && rawJson.length > 120) return `${target} (${lines} lines · ${kb} KB)`;
  if (target) return target;
  if (rawJson.length > 80) return `streaming ${lines} lines (${kb} KB)…`;
  return '';
}

function buildToolMeta(toolName: string, rawInput: unknown): ToolEventMeta {
  const canonical = normalizeToolName(toolName);
  const norm = normalizeToolInput(canonical, rawInput);
  const rec = norm && typeof norm === 'object' ? (norm as Record<string, unknown>) : {};
  const pathStr = typeof rec.path === 'string' && rec.path.trim() ? rec.path.trim() : '.';

  if (canonical === 'read_file' || canonical === 'view_image') {
    const range = typeof rec.startLine === 'number'
      ? `:${rec.startLine}${typeof rec.endLine === 'number' ? `-${rec.endLine}` : ''}`
      : '';
    return { kind: 'read', summary: `${pathStr}${range}` };
  }
  if (canonical === 'list_files') {
    return { kind: 'list', summary: pathStr };
  }
  if (canonical === 'search') {
    const q = typeof rec.query === 'string' ? rec.query.trim() : '';
    return { kind: 'search', summary: pathStr !== '.' ? `${q} in ${pathStr}` : q };
  }
  if (canonical === 'bash') {
    const cmd = typeof rec.command === 'string' ? rec.command.trim().replace(/\s+/g, ' ') : '';
    return { kind: 'exec', summary: cmd.slice(0, 140) };
  }
  if (canonical === 'write_file' || canonical === 'edit_file' || canonical === 'apply_patch') {
    if (canonical === 'apply_patch' && typeof rec.patch === 'string') {
      const files = [...rec.patch.matchAll(/^\*\*\* (?:Update|Add|Delete) File:\s*(.+)$/gm)].map(m => m[1].trim());
      return { kind: 'edit', summary: files.join(', ') || 'patch' };
    }
    return { kind: 'edit', summary: pathStr };
  }
  if (canonical === 'update_plan') {
    const count = Array.isArray(rec.plan) ? rec.plan.length : 0;
    return { kind: 'other', summary: `${count} step(s)` };
  }
  if (canonical === 'get_goal') {
    return { kind: 'other', summary: 'inspect active goal' };
  }
  if (canonical === 'create_goal') {
    const obj = typeof rec.objective === 'string' ? rec.objective.trim().slice(0, 80) : 'new goal';
    return { kind: 'other', summary: obj };
  }
  if (canonical === 'update_goal') {
    const st = typeof rec.status === 'string' ? rec.status : 'status';
    return { kind: 'other', summary: `status -> ${st}` };
  }
  if (canonical === 'request_user_input') {
    const count = Array.isArray(rec.questions) ? rec.questions.length : 1;
    return { kind: 'other', summary: `${count} question(s)` };
  }
  if (canonical === 'list_mcp_resources' || canonical === 'list_mcp_resource_templates') {
    return { kind: 'list', summary: typeof rec.server === 'string' ? rec.server : 'all servers' };
  }
  if (canonical === 'read_mcp_resource') {
    return { kind: 'read', summary: `${typeof rec.server === 'string' ? rec.server : ''}:${typeof rec.uri === 'string' ? rec.uri : ''}` };
  }
  if (canonical === 'spawn_agent') {
    const role = typeof rec.agent_type === 'string' ? rec.agent_type : typeof rec.role === 'string' ? rec.role : 'default';
    const msg = typeof rec.message === 'string' ? rec.message : typeof rec.prompt === 'string' ? rec.prompt : typeof rec.task === 'string' ? rec.task : '';
    return { kind: 'other', summary: `[${role}] ${msg.trim().replace(/\s+/g, ' ').slice(0, 100)}`.trim() };
  }
  if (canonical === 'wait_agent') {
    const targets = Array.isArray(rec.targets) ? rec.targets.join(', ') : Array.isArray(rec.ids) ? rec.ids.join(', ') : typeof rec.target === 'string' ? rec.target : 'agents';
    return { kind: 'other', summary: `waiting for ${targets}` };
  }
  if (canonical === 'send_input') {
    const target = typeof rec.target === 'string' ? rec.target : typeof rec.id === 'string' ? rec.id : 'agent';
    return { kind: 'other', summary: `to ${target}` };
  }
  if (canonical === 'close_agent') {
    const target = typeof rec.target === 'string' ? rec.target : typeof rec.id === 'string' ? rec.id : 'agent';
    return { kind: 'other', summary: target };
  }
  if (canonical === 'delegate_task' || canonical === 'delegate_edit') {
    const count = Array.isArray(rec.tasks) ? rec.tasks.length : 1;
    return { kind: 'other', summary: `${count} subagent task(s)` };
  }
  if (canonical === 'agent_network') {
    const topic = typeof rec.topic === 'string' ? rec.topic.trim().replace(/\s+/g, ' ').slice(0, 80) : 'brainstorm';
    const rounds = typeof rec.rounds === 'number' ? `${rec.rounds}R` : '2R';
    return { kind: 'other', summary: `[${rounds}] ${topic}` };
  }
  if (canonical === 'peer_send') {
    const to = typeof rec.to === 'string' ? rec.to : 'peer';
    const kind = typeof rec.kind === 'string' ? rec.kind : 'message';
    return { kind: 'other', summary: `→ ${to} (${kind})` };
  }
  if (canonical === 'peer_broadcast') {
    const topic = typeof rec.topic === 'string' ? rec.topic.slice(0, 60) : 'blackboard';
    return { kind: 'other', summary: `broadcast: ${topic}` };
  }
  if (canonical === 'peer_read') {
    return { kind: 'read', summary: 'agent network inbox & blackboard' };
  }
  if (canonical === 'run_workflow') {
    const wf = typeof rec.name === 'string' ? rec.name : 'workflow';
    return { kind: 'other', summary: wf };
  }
  return { kind: 'other', summary: canonical };
}

function summarizeDiffCounts(diffText: string | undefined): { added: number; removed: number; verb: 'Added' | 'Edited' | 'Deleted' } {
  if (!diffText) return { added: 0, removed: 0, verb: 'Edited' };
  let added = 0;
  let removed = 0;
  let isNew = false;
  let isDeleted = false;
  for (const line of diffText.split('\n')) {
    if (line.startsWith('--- /dev/null') || /^--- a\/.+ \(new \+\d+ -\d+\)$/.test(line)) isNew = true;
    else if (line.startsWith('+++ /dev/null') || /^--- a\/.+ \(deleted \+\d+ -\d+\)$/.test(line)) isDeleted = true;
    else if (line.startsWith('+') && !line.startsWith('+++')) added++;
    else if (line.startsWith('-') && !line.startsWith('---')) removed++;
    else {
      const moreAdded = /^\.\.\. \(\+(\d+) more added lines\)$/.exec(line);
      if (moreAdded) added += Number(moreAdded[1]);
      const moreRemoved = /^\.\.\. \(-(\d+) more removed lines\)$/.exec(line);
      if (moreRemoved) removed += Number(moreRemoved[1]);
    }
  }
  const verb = isDeleted ? 'Deleted' : isNew ? 'Added' : 'Edited';
  return { added, removed, verb };
}

function canonicalize(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(canonicalize);
  if (value && typeof value === 'object') {
    return Object.fromEntries(
      Object.entries(value as Record<string, unknown>)
        .sort(([a], [b]) => a.localeCompare(b))
        .map(([k, v]) => [k, canonicalize(v)])
    );
  }
  return value;
}

function callSignature(call: ToolCall): string {
  return `${call.toolName}:${JSON.stringify(canonicalize(call.input))}`;
}

export class Harness {
  /**
   * Set for prompts written by a model (subagent tasks). Such prompts may only attach images from
   * the workspace; a user-typed prompt may attach any image the user points at.
   */
  untrustedPrompts = false;
  private running = false;
  private pendingContext: ModelMessage[] = [];
  private readonly model: LanguageModel;
  private lastCallSig = '';
  private repeatCount = 0;
  private consecutiveGoalExecFailureTurns = 0;
  private turnHadSuccessfulTool = false;
  private turnHadFailedExec = false;
  private flushGoalProgressHook?: (allowStopped?: boolean) => void;
  constructor(readonly config: RuntimeConfig, readonly store: SessionStore, readonly tools: ToolRegistry,
    readonly emit: (event: HarnessEvent) => void = () => {}, model?: LanguageModel) {
    this.model = model ?? createModel(config, this.store.session.id);
    if (this.store.session.todos?.length) {
      this.tools.todos = [...this.store.session.todos];
    }
    this.tools.onTodoUpdate = todos => {
      this.store.session.todos = todos;
      this.emit({ type: 'todo', todos });
    };
    if (!this.tools.readOnly) {
      this.tools.getGoal = () => this.store.session.goal;
      this.tools.onGoalUpdate = async nextGoal => {
        this.flushGoalProgressHook?.(true);
        if (nextGoal && nextGoal.tokensAtStart === 0 && nextGoal.tokensUsed === 0) {
          nextGoal.tokensAtStart = this.store.session.tokens;
        }
        if (nextGoal && this.store.session.goal) {
          nextGoal.tokensUsed = this.store.session.goal.tokensUsed ?? nextGoal.tokensUsed ?? 0;
          nextGoal.timeUsedSeconds = this.store.session.goal.timeUsedSeconds ?? nextGoal.timeUsedSeconds ?? 0;
        }
        this.store.session.goal = nextGoal;
        await this.store.save();
        this.emit({ type: 'goal', goal: nextGoal });
      };
    }
    this.tools.onPlanExit = plan => {
      this.emit({ type: 'plan_exit', plan });
    };
  }
  /** True while a run, compaction or other exclusive operation owns the session history. */
  get busy(): boolean {
    return this.running;
  }

  /**
   * Adds a user-role context message (shell output from `!cmd`, goal updates, notices). While a run
   * is active the message waits for the next safe point — after the current tool results and
   * before the next model request — because inserting it between an assistant tool call and its
   * results produces a history every provider rejects.
   */
  addContextMessage(message: ModelMessage) {
    if (this.running) this.pendingContext.push(message);
    else this.store.session.messages.push(message);
  }

  private drainPendingContext(): boolean {
    if (!this.pendingContext.length) return false;
    this.store.session.messages.push(...this.pendingContext.splice(0));
    return true;
  }

  async compact(signal: AbortSignal, focus?: string): Promise<{ tokens: number; compactions: number }> {
    if (this.running) throw new Error('Cannot compact while a run is active.');
    this.running = true;
    try {
      const res = await compactNow(this.store, this.config, this.model, signal, focus);
      this.emit({ type: 'compaction', count: res.compactions });
      if (res.tokens > 0) this.emit({ type: 'usage', input: 0, output: 0, total: res.tokens });
      return res;
    } finally {
      this.running = false;
      if (this.drainPendingContext()) await this.store.save().catch(() => {});
    }
  }
  async recap(signal: AbortSignal): Promise<{ text: string; tokens: number }> {
    const res = await recapSession(this.store, this.config, this.model, signal);
    if (res.tokens > 0) this.emit({ type: 'usage', input: 0, output: 0, total: res.tokens });
    return res;
  }
  async askSideQuestion(question: string, signal: AbortSignal): Promise<{ text: string; tokens: number }> {
    const clonedHistory: ModelMessage[] = structuredClone(this.store.session.messages);
    repairInterrupted(clonedHistory);
    const { expandedPrompt } = await expandFileMentions(question, this.tools.files, { model: this.config.model });
    clonedHistory.push({
      role: 'user',
      content: `[Ephemeral Side Question (/btw) — answer concisely without modifying session state]:\n${expandedPrompt}`,
    });
    const providerOptions = buildProviderOptions({
      provider: this.config.provider,
      effort: this.config.effort ? 'low' : undefined,
    });
    const sys = await systemPrompt(this.config, true, false, this.store.session.goal);
    const result = streamText({
      model: this.model,
      system: `${sys}\n\nYou are answering an ephemeral side question (/btw). Do not call tools; answer directly and concisely from the conversation context and your knowledge.`,
      messages: withReasoningReplay(clonedHistory, this.config.provider, this.config.effort),
      maxOutputTokens: Math.min(8_192, this.config.maxOutputTokens),
      maxRetries: 2,
      ...(providerOptions ? { providerOptions } : {}),
      headers: auxiliaryEffortHeaders(this.config.effort),
      abortSignal: AbortSignal.any([signal, AbortSignal.timeout(this.config.timeoutMs)]),
      onError: () => {},
    });
    let raw = '';
    for await (const event of result.fullStream) {
      if (event.type === 'error') throw event.error;
      if (event.type === 'text-delta') raw += event.text;
    }
    const usage = await result.usage;
    const text = visibleAnswer(raw, this.config.inlineReasoning).trim() || raw.trim();
    const tokens = usage.totalTokens ?? 0;
    return { text, tokens };
  }
  async revert(options?: { steps?: number; scope?: 'both' | 'code' | 'messages'; force?: boolean }): Promise<RevertResult> {
    if (this.running) throw new Error('Cannot revert while a run is active.');
    const session = this.store.session;
    const checkpoints = session.checkpoints ?? [];
    if (!checkpoints.length) throw new Error('No turn checkpoints available to revert.');
    const count = Math.min(Math.max(1, options?.steps ?? 1), checkpoints.length);
    const scope = options?.scope ?? 'both';
    const target = checkpoints.slice(-count);
    const restoredSet = new Set<string>();
    const deletedSet = new Set<string>();
    const conflictSet = new Set<string>();
    const prunedSet = new Set<string>();

    if (scope === 'both' || scope === 'code') {
      for (const cp of [...target].reverse()) {
        // Once a file conflicts, older backups of it must not be applied either.
        const files = cp.files.filter(f => !conflictSet.has(f.path));
        const res = await this.tools.files.restoreBackups(files, { force: options?.force });
        for (const r of res.restored) {
          deletedSet.delete(r);
          restoredSet.add(r);
        }
        for (const d of res.deleted) {
          restoredSet.delete(d);
          deletedSet.add(d);
        }
        for (const c of res.conflicts) conflictSet.add(c);
        for (const p of res.pruned) prunedSet.add(p);
      }
    }

    let messagesRemoved = 0;
    const first = target[0];
    if (scope === 'both' || scope === 'messages') {
      const prevLen = session.messages.length;
      if (first.messageIndex <= prevLen) {
        messagesRemoved = prevLen - first.messageIndex;
        session.messages.length = first.messageIndex;
      }
      session.tokens = first.tokensBefore;
      checkpoints.splice(checkpoints.length - count, count);
      session.checkpoints = checkpoints;
    } else if (scope === 'code') {
      for (const cp of target) cp.files = [];
      const changedList = [...restoredSet, ...deletedSet];
      if (changedList.length > 0) {
        session.messages.push({
          role: 'user',
          content: `[System Notice: User reverted workspace file changes from the last ${count} turn(s). Restored/deleted files: ${changedList.join(', ')}]`,
        });
      }
    }

    await this.store.save();
    return {
      turnsReverted: count,
      messagesRemoved,
      restoredFiles: [...restoredSet],
      deletedFiles: [...deletedSet],
      revertedPrompts: target.map(c => c.prompt),
      conflictFiles: [...conflictSet],
      unrecoverableFiles: [...prunedSet],
    };
  }
  async run(prompt: string, signal: AbortSignal, sharedBudget?: RunBudget): Promise<string> {
    if (this.running) throw new Error('This harness already has an active run.');
    this.running = true;
    this.lastCallSig = '';
    this.repeatCount = 0;
    let text = '', spent = 0, measuredInput = 0;
    const budget = sharedBudget ?? { spent: 0, limit: this.config.runTokenBudget };
    this.tools.budget = budget;
    const account = (tokens: number) => { spent += tokens; budget.spent += tokens; };
    const exhausted = () => (this.config.runTokenBudget > 0 && spent >= this.config.runTokenBudget) || (budget.limit > 0 && budget.spent >= budget.limit);
    const session = this.store.session;
    const messageIndexBeforeTurn = session.messages.length;
    const tokensBeforeTurn = session.tokens;
    const turnStartedAt = new Date().toISOString();
    let lastGoalAccountMs = Date.now();
    let lastGoalAccountTokens = session.tokens;
    let budgetLimitSteered = false;
    this.turnHadSuccessfulTool = false;
    this.turnHadFailedExec = false;
    const flushGoalTurnProgress = (allowStopped = false) => {
      const goal = session.goal;
      if (!goal || this.tools.planMode) return;
      if (goal.status !== 'active' && !allowStopped) return;
      const nowMs = Date.now();
      const deltaSec = Math.max(0, Math.floor((nowMs - lastGoalAccountMs) / 1000));
      const deltaTokens = Math.max(0, session.tokens - lastGoalAccountTokens);
      lastGoalAccountMs = nowMs;
      lastGoalAccountTokens = session.tokens;
      if (deltaSec === 0 && deltaTokens === 0) return;
      goal.timeUsedSeconds = (goal.timeUsedSeconds ?? 0) + deltaSec;
      goal.tokensUsed = (goal.tokensUsed ?? 0) + deltaTokens;
      goal.updatedAt = new Date(nowMs).toISOString();
      if (goal.status === 'active' && goal.tokenBudget !== undefined && goal.tokensUsed >= goal.tokenBudget) {
        goal.status = 'budget_limited';
      }
      this.emit({ type: 'goal', goal });
    };
    this.flushGoalProgressHook = flushGoalTurnProgress;
    this.tools.files.beginTurn();
    try {
      signal.throwIfAborted();
      this.drainPendingContext();
      const { expandedPrompt, imageParts } = await expandFileMentions(prompt, this.tools.files, {
        model: this.config.model,
        allowOutsideWorkspace: !this.untrustedPrompts,
      });
      if (imageParts.length > 0) {
        session.messages.push({
          role: 'user',
          content: [{ type: 'text', text: expandedPrompt }, ...imageParts],
        });
      } else {
        session.messages.push({ role: 'user', content: expandedPrompt });
      }
      await this.store.save();
      const providerOptions = buildProviderOptions(this.config);
      const promptCache: SystemPromptCache = {};
      for (;;) {
        signal.throwIfAborted();
        // Safe point: the previous step's tool results are recorded, the next request not yet sent.
        if (this.drainPendingContext()) await this.store.save();
        if (exhausted()) throw new Error('Run token budget reached. Resume with a new prompt or increase --run-token-budget.');
        const system = await systemPrompt(this.config, this.tools.readOnly, this.tools.planMode, session.goal, promptCache);
        const oldCount = session.compactions;
        account(await compactIfNeeded(this.store, this.config, this.model, system, signal, measuredInput));
        if (session.compactions !== oldCount) { measuredInput = 0; this.emit({ type: 'compaction', count: session.compactions }); }
        if (exhausted()) throw new Error('Run token budget reached during compaction. Original history is archived; resume to continue.');
        let stepText = '';
        let calls: ToolCall[] = [];
        for (let attempt = 0; ; attempt++) {
          stepText = '';
          calls = [];
          try {
            const result = streamText({
              model: this.model, system, messages: withReasoningReplay(session.messages, this.config.provider, this.config.effort), tools: this.tools.schemas(),
              maxOutputTokens: this.config.maxOutputTokens, maxRetries: 0,
              ...(providerOptions ? { providerOptions } : {}),
              abortSignal: AbortSignal.any([signal, AbortSignal.timeout(this.config.timeoutMs)]),
              // The SDK's default handler logs the raw upstream error. CLI owns
              // error presentation so credentials can be redacted before output.
              onError: () => {},
            });
            const projection = new AnswerProjection(this.config.inlineReasoning);
            const present = (parts: AnswerPart[]) => {
              for (const part of parts) {
                if (part.type === 'text') { stepText += part.text; this.emit({ type: 'text', text: part.text }); }
                else if (this.config.showReasoning) this.emit({ ...part, type: 'reasoning', source: 'inline' });
              }
            };
            const streamingTools = new Map<string, { name: string; raw: string; lastEmitLen: number }>();
            for await (const event of result.fullStream) {
              if (event.type === 'error') throw event.error;
              if (event.type === 'abort') throw new Error('Model request cancelled or timed out.');
              if (event.type === 'text-delta') present(projection.push(event.text));
              if (event.type === 'reasoning-delta') {
                if (this.config.showReasoning) this.emit({ type: 'reasoning', text: event.text, source: 'provider' });
              }
              if (event.type === 'tool-input-start') {
                const name = normalizeToolName(event.toolName);
                streamingTools.set(event.id, { name, raw: '', lastEmitLen: 0 });
                this.emit({ type: 'tool_stream', name, id: event.id, state: 'start', preview: '' });
              }
              if (event.type === 'tool-input-delta') {
                const entry = streamingTools.get(event.id);
                if (entry) {
                  entry.raw += event.delta;
                  if (entry.raw.length - entry.lastEmitLen >= 48 || event.delta.includes('\n') || entry.lastEmitLen === 0) {
                    entry.lastEmitLen = entry.raw.length;
                    this.emit({
                      type: 'tool_stream',
                      name: entry.name,
                      id: event.id,
                      state: 'delta',
                      preview: extractStreamPreview(entry.raw),
                    });
                  }
                }
              }
            }
            signal.throwIfAborted();
            const finish = await result.finishReason;
            if (finish === 'length') throw new Error('Model response hit the output limit. No tools from this response were executed; increase --max-output-tokens.');
            if (['error', 'content-filter', 'other', 'unknown'].includes(finish)) throw new Error(`Model stopped unexpectedly (${finish}).`);
            present(projection.finish());
            const response = await result.response;
            const assistantMessages = response.messages.filter(m => m.role === 'assistant');
            calls = (await result.toolCalls).map(c => ({ toolCallId: c.toolCallId, toolName: c.toolName, input: c.input }));
            for (const m of assistantMessages) {
              if (!Array.isArray(m.content)) continue;
              for (const part of m.content) {
                if (part.type === 'tool-call' && !calls.some(c => c.toolCallId === part.toolCallId)) {
                  calls.push({ toolCallId: part.toolCallId, toolName: part.toolName, input: part.input });
                }
              }
            }
            if (calls.length > 32 || new Set(calls.map(c => c.toolCallId)).size !== calls.length) throw new Error('Invalid or excessive tool call batch. No tools executed.');
            const usage = await result.usage;
            const input = usage.inputTokens ?? estimatedTokens(session.messages, system);
            const output = usage.outputTokens ?? Math.ceil(JSON.stringify(assistantMessages).length / 2);
            const tokens = usage.totalTokens ?? input + output;
            account(tokens); session.tokens += tokens; measuredInput = input;
            flushGoalTurnProgress();
            this.emit({ type: 'usage', input, output, total: tokens });
            session.messages.push(...assistantMessages);
            text += stepText;
            await this.store.save(); // Persist tool declarations before any side effect.
            break;
          } catch (stepErr) {
            if (
              attempt >= MAX_API_RETRIES ||
              isNonRetryableApiError(stepErr, signal) ||
              /output limit|Model stopped unexpectedly|Invalid or excessive tool call batch/i.test(
                stepErr instanceof Error ? stepErr.message : String(stepErr),
              )
            ) {
              throw stepErr;
            }
            const nextRetry = attempt + 1;
            this.emit({
              type: 'retry',
              attempt: nextRetry,
              maxRetries: MAX_API_RETRIES,
              message: safeError(stepErr, this.config.apiKey),
            });
            await sleepWithAbort(retryDelayMs(stepErr, attempt), signal);
          }
        }
        // A successful terminal step needs its own answer. Reasoning may be
        // opaque to the adapter, and earlier tool narration is not completion.
        if (!calls.length && !stepText.trim()) throw new Error('Model finished without a visible answer.');
        if (!calls.length) return text;
        const outcome = await this.executeBatch(calls, signal);
        if (outcome.denied) { this.emit({ type: 'stopped', reason: 'Permission denied.' }); return text; }
        flushGoalTurnProgress();
        if (session.goal?.status === 'budget_limited' && !budgetLimitSteered) {
          budgetLimitSteered = true;
          session.messages.push({
            role: 'user',
            content: buildGoalBudgetLimitPrompt(session.goal),
          });
          await this.store.save();
        }
      }
    } catch (error) {
      flushGoalTurnProgress();
      if (!signal.aborted && session.goal && (session.goal.status === 'active' || session.goal.status === 'budget_limited')) {
        const errMsg = error instanceof Error ? error.message : String(error);
        const isUsageLimit = /\b(?:429|rate[_\s-]?limit|usage[_\s-]?limit|insufficient_quota|quota|token budget reached)\b/i.test(errMsg);
        if (session.goal.status === 'active' || isUsageLimit) {
          session.goal.status = isUsageLimit ? 'usage_limited' : 'blocked';
          session.goal.updatedAt = new Date().toISOString();
          this.emit({ type: 'goal', goal: session.goal });
        }
      }
      // Failed streams execute no tools. If cancellation happens after a tool
      // declaration, pair every uncompleted call before the next resume.
      repairInterrupted(session.messages);
      this.drainPendingContext();
      await this.store.save();
      throw error;
    } finally {
      this.flushGoalProgressHook = undefined;
      flushGoalTurnProgress();
      if (session.goal?.status === 'active' && !signal.aborted) {
        if (this.turnHadFailedExec && !this.turnHadSuccessfulTool) {
          this.consecutiveGoalExecFailureTurns++;
          if (this.consecutiveGoalExecFailureTurns >= 3) {
            session.goal.status = 'blocked';
            session.goal.updatedAt = new Date().toISOString();
            this.emit({ type: 'goal', goal: session.goal });
          }
        }
      }
      const files = this.tools.files.takeTurnBackups();
      session.checkpoints ??= [];
      const nextId = (session.checkpoints.at(-1)?.id ?? 0) + 1;
      const cp: TurnCheckpoint = {
        id: nextId,
        createdAt: turnStartedAt,
        prompt: prompt.startsWith('<codex_internal_context source="goal">') ? (session.goal ? `[goal] ${session.goal.objective}` : '[goal continuation]') : prompt,
        messageIndex: messageIndexBeforeTurn,
        tokensBefore: tokensBeforeTurn,
        files,
      };
      session.checkpoints.push(cp);
      this.drainPendingContext();
      await this.store.save().catch(() => {});
      this.running = false;
      this.tools.budget = undefined;
    }
  }

  private async executeBatch(calls: ToolCall[], signal: AbortSignal): Promise<{ denied: boolean }> {
    const content: ToolResultPart[] = [];
    let denied = false;
    const execute = async (call: ToolCall) => {
      let output: ToolResultPart['output'];
      const meta = buildToolMeta(call.toolName, call.input);
      const startedMs = Date.now();
      try {
        if (denied || signal.aborted) throw new Error(denied ? 'Skipped after permission denial.' : 'Cancelled before dispatch.');
        const sig = callSignature(call);
        if (sig === this.lastCallSig) this.repeatCount++;
        else { this.lastCallSig = sig; this.repeatCount = 1; }
        if (meta.kind === 'edit') {
          const preDiff = await this.tools.files.previewDiff(call.toolName, call.input).catch(() => undefined);
          if (preDiff) {
            const counts = summarizeDiffCounts(preDiff);
            meta.diff = preDiff;
            meta.added = counts.added;
            meta.removed = counts.removed;
            meta.verb = counts.verb;
          }
        }
        this.emit({ type: 'tool', name: call.toolName, id: call.toolCallId, state: 'start', meta });
        const result = await this.tools.execute(call, signal);
        denied ||= result.denied === true;
        meta.durationMs = Date.now() - startedMs;
        if (!result.denied) {
          const isFailedBash = meta.kind === 'exec' && /^Exit code:\s*(?!0\b)\d+/m.test(result.text);
          if (isFailedBash) {
            this.turnHadFailedExec = true;
          } else {
            this.turnHadSuccessfulTool = true;
            this.consecutiveGoalExecFailureTurns = 0;
          }
        }
        if (meta.kind === 'exec' && result.text) {
          const allLines = result.text.split(/\r?\n/).map(l => l.trimEnd()).filter(Boolean);
          meta.outputPreview = allLines.slice(0, 80);
          if (allLines.length > 80) meta.omittedLines = allLines.length - 80;
        }
        const loopNote = !result.denied && (this.repeatCount === 3 || this.repeatCount === 5 || this.repeatCount >= 8)
          ? `\n[Loop warning: ${call.toolName} was called ${this.repeatCount} times consecutively with identical arguments. Analyze the previous result and try a different approach.]`
          : '';
        output = { type: result.denied ? 'error-text' : 'text', value: result.text + loopNote };
        this.emit({ type: 'tool', name: call.toolName, id: call.toolCallId, state: result.denied ? 'error' : 'done', meta });
      } catch (error) {
        meta.durationMs = Date.now() - startedMs;
        output = { type: 'error-text', value: safeError(error, this.config.apiKey) };
        this.emit({ type: 'tool', name: call.toolName, id: call.toolCallId, state: 'error', meta });
      }
      return { type: 'tool-result' as const, toolCallId: call.toolCallId, toolName: call.toolName, output };
    };
    let index = 0;
    while (index < calls.length) {
      // Consecutive reads form small parallel groups. Writes/shell are barriers.
      const group = [calls[index++]];
      if (this.tools.isReadOnly(group[0].toolName)) {
        while (group.length < MAX_PARALLEL_READS && index < calls.length && this.tools.isReadOnly(calls[index].toolName)) group.push(calls[index++]);
      }
      content.push(...await Promise.all(group.map(execute)));
      // Keep one tool result message per batch in model order, checkpointed at
      // each barrier so a crash never blindly repeats a completed write.
      const previous = this.store.session.messages.at(-1);
      if (previous?.role === 'tool') previous.content.push(...content.splice(0));
      else this.store.session.messages.push({ role: 'tool', content: content.splice(0) });
      await this.store.save();
    }
    const pendingImgs = this.tools.takePendingImages();
    if (pendingImgs.length > 0) {
      const supportsVision = resolveModelCapabilities(this.config.model).supportsVision ?? true;
      if (supportsVision) {
        this.store.session.messages.push({
          role: 'user',
          content: [
            { type: 'text', text: `[Attached image(s) from view_image: ${pendingImgs.map(i => i.path).join(', ')}]` },
            // Base64 keeps the session file small, valid JSON and resumable.
            ...pendingImgs.map(img => ({
              type: 'image' as const,
              image: Buffer.from(img.bytes).toString('base64'),
              mediaType: img.mimeType,
            })),
          ],
        });
        await this.store.save();
      }
    }
    signal.throwIfAborted();
    return { denied };
  }
}

