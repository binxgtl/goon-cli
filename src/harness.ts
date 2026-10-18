import { streamText, type LanguageModel, type ToolResultPart } from 'ai';
import type { RuntimeConfig } from './config.js';
import { SessionStore, repairInterrupted, type TodoItem, type TurnCheckpoint } from './session.js';
import { ToolRegistry, type ToolCall, type RunBudget } from './tools.js';
import { buildProviderOptions, createModel } from './provider.js';
import { safeError } from './errors.js';
import { compactIfNeeded, compactNow, estimatedTokens, expandFileMentions, systemPrompt } from './context.js';
import { AnswerProjection, type AnswerPart } from './reasoning.js';

export type HarnessEvent =
  | { type: 'text'; text: string }
  | { type: 'reasoning'; text: string; source: 'inline' | 'provider' }
  | { type: 'tool'; name: string; id: string; state: 'start' | 'done' | 'error' }
  | { type: 'usage'; input: number; output: number; total: number }
  | { type: 'compaction'; count: number }
  | { type: 'todo'; todos: TodoItem[] }
  | { type: 'plan_exit'; plan: string }
  | { type: 'stopped'; reason: string };

export interface RevertResult {
  turnsReverted: number;
  messagesRemoved: number;
  restoredFiles: string[];
  deletedFiles: string[];
  revertedPrompts: string[];
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
  private running = false;
  private readonly model: LanguageModel;
  private lastCallSig = '';
  private repeatCount = 0;
  constructor(readonly config: RuntimeConfig, readonly store: SessionStore, readonly tools: ToolRegistry,
    readonly emit: (event: HarnessEvent) => void = () => {}, model?: LanguageModel) {
    this.model = model ?? createModel(config);
    if (this.store.session.todos?.length) {
      this.tools.todos = [...this.store.session.todos];
    }
    this.tools.onTodoUpdate = todos => {
      this.store.session.todos = todos;
      this.emit({ type: 'todo', todos });
    };
    this.tools.onPlanExit = plan => {
      this.emit({ type: 'plan_exit', plan });
    };
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
    }
  }
  async revert(options?: { steps?: number; scope?: 'both' | 'code' | 'messages' }): Promise<RevertResult> {
    if (this.running) throw new Error('Cannot revert while a run is active.');
    const session = this.store.session;
    const checkpoints = session.checkpoints ?? [];
    if (!checkpoints.length) throw new Error('No turn checkpoints available to revert.');
    const count = Math.min(Math.max(1, options?.steps ?? 1), checkpoints.length);
    const scope = options?.scope ?? 'both';
    const target = checkpoints.slice(-count);
    const restoredSet = new Set<string>();
    const deletedSet = new Set<string>();

    if (scope === 'both' || scope === 'code') {
      for (const cp of [...target].reverse()) {
        const res = await this.tools.files.restoreBackups(cp.files);
        for (const r of res.restored) {
          deletedSet.delete(r);
          restoredSet.add(r);
        }
        for (const d of res.deleted) {
          restoredSet.delete(d);
          deletedSet.add(d);
        }
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
    this.tools.files.beginTurn();
    try {
      signal.throwIfAborted();
      const { expandedPrompt } = await expandFileMentions(prompt, this.tools.files);
      session.messages.push({ role: 'user', content: expandedPrompt });
      await this.store.save();
      const providerOptions = buildProviderOptions(this.config);
      for (;;) {
        signal.throwIfAborted();
        if (exhausted()) throw new Error('Run token budget reached. Resume with a new prompt or increase --run-token-budget.');
        const system = await systemPrompt(this.config, this.tools.readOnly, this.tools.planMode);
        const oldCount = session.compactions;
        account(await compactIfNeeded(this.store, this.config, this.model, system, signal, measuredInput));
        if (session.compactions !== oldCount) { measuredInput = 0; this.emit({ type: 'compaction', count: session.compactions }); }
        if (exhausted()) throw new Error('Run token budget reached during compaction. Original history is archived; resume to continue.');
        const result = streamText({
          model: this.model, system, messages: session.messages, tools: this.tools.schemas(),
          maxOutputTokens: this.config.maxOutputTokens, maxRetries: 2,
          ...(providerOptions ? { providerOptions } : {}),
          abortSignal: AbortSignal.any([signal, AbortSignal.timeout(this.config.timeoutMs)]),
          // The SDK's default handler logs the raw upstream error. CLI owns
          // error presentation so credentials can be redacted before output.
          onError: () => {},
        });
        let stepText = '';
        const projection = new AnswerProjection(this.config.inlineReasoning);
        const present = (parts: AnswerPart[]) => {
          for (const part of parts) {
            if (part.type === 'text') { stepText += part.text; this.emit({ type: 'text', text: part.text }); }
            else if (this.config.showReasoning) this.emit({ ...part, type: 'reasoning', source: 'inline' });
          }
        };
        for await (const event of result.fullStream) {
          if (event.type === 'error') throw event.error;
          if (event.type === 'abort') throw new Error('Model request cancelled or timed out.');
          if (event.type === 'text-delta') present(projection.push(event.text));
          if (event.type === 'reasoning-delta') {
            if (this.config.showReasoning) this.emit({ type: 'reasoning', text: event.text, source: 'provider' });
          }
        }
        signal.throwIfAborted();
        const finish = await result.finishReason;
        if (finish === 'length') throw new Error('Model response hit the output limit. No tools from this response were executed; increase --max-output-tokens.');
        if (['error', 'content-filter', 'other', 'unknown'].includes(finish)) throw new Error(`Model stopped unexpectedly (${finish}).`);
        present(projection.finish());
        const response = await result.response;
        const calls: ToolCall[] = (await result.toolCalls).map(c => ({ toolCallId: c.toolCallId, toolName: c.toolName, input: c.input }));
        if (calls.length > 32 || new Set(calls.map(c => c.toolCallId)).size !== calls.length) throw new Error('Invalid or excessive tool call batch. No tools executed.');
        const usage = await result.usage;
        const input = usage.inputTokens ?? estimatedTokens(session.messages, system);
        const output = usage.outputTokens ?? Math.ceil(JSON.stringify(response.messages).length / 2);
        const tokens = usage.totalTokens ?? input + output;
        account(tokens); session.tokens += tokens; measuredInput = input;
        this.emit({ type: 'usage', input, output, total: tokens });
        session.messages.push(...response.messages);
        text += stepText;
        await this.store.save(); // Persist tool declarations before any side effect.
        // A successful terminal step needs its own answer. Reasoning may be
        // opaque to the adapter, and earlier tool narration is not completion.
        if (!calls.length && !stepText.trim()) throw new Error('Model finished without a visible answer.');
        if (!calls.length) return text;
        const outcome = await this.executeBatch(calls, signal);
        if (outcome.denied) { this.emit({ type: 'stopped', reason: 'Permission denied.' }); return text; }
      }
    } catch (error) {
      // Failed streams execute no tools. If cancellation happens after a tool
      // declaration, pair every uncompleted call before the next resume.
      repairInterrupted(session.messages);
      await this.store.save();
      throw error;
    } finally {
      const files = this.tools.files.takeTurnBackups();
      if (session.messages.length > messageIndexBeforeTurn || files.length > 0) {
        session.checkpoints ??= [];
        const nextId = (session.checkpoints.at(-1)?.id ?? 0) + 1;
        const cp: TurnCheckpoint = {
          id: nextId,
          createdAt: turnStartedAt,
          prompt,
          messageIndex: messageIndexBeforeTurn,
          tokensBefore: tokensBeforeTurn,
          files,
        };
        session.checkpoints.push(cp);
        await this.store.save().catch(() => {});
      }
      this.running = false;
      this.tools.budget = undefined;
    }
  }

  private async executeBatch(calls: ToolCall[], signal: AbortSignal): Promise<{ denied: boolean }> {
    const content: ToolResultPart[] = [];
    let denied = false;
    const execute = async (call: ToolCall) => {
      let output: ToolResultPart['output'];
      try {
        if (denied || signal.aborted) throw new Error(denied ? 'Skipped after permission denial.' : 'Cancelled before dispatch.');
        const sig = callSignature(call);
        if (sig === this.lastCallSig) this.repeatCount++;
        else { this.lastCallSig = sig; this.repeatCount = 1; }
        this.emit({ type: 'tool', name: call.toolName, id: call.toolCallId, state: 'start' });
        const result = await this.tools.execute(call, signal);
        denied ||= result.denied === true;
        const loopNote = !result.denied && (this.repeatCount === 3 || this.repeatCount === 5 || this.repeatCount >= 8)
          ? `\n[Loop warning: ${call.toolName} was called ${this.repeatCount} times consecutively with identical arguments. Analyze the previous result and try a different approach.]`
          : '';
        output = { type: result.denied ? 'error-text' : 'text', value: result.text + loopNote };
        this.emit({ type: 'tool', name: call.toolName, id: call.toolCallId, state: result.denied ? 'error' : 'done' });
      } catch (error) {
        output = { type: 'error-text', value: safeError(error, this.config.apiKey) };
        this.emit({ type: 'tool', name: call.toolName, id: call.toolCallId, state: 'error' });
      }
      return { type: 'tool-result' as const, toolCallId: call.toolCallId, toolName: call.toolName, output };
    };
    let index = 0;
    while (index < calls.length) {
      // Consecutive reads form small parallel groups. Writes/shell are barriers.
      const group = [calls[index++]];
      if (this.tools.isReadOnly(group[0].toolName) && index < calls.length && this.tools.isReadOnly(calls[index].toolName)) group.push(calls[index++]);
      content.push(...await Promise.all(group.map(execute)));
      // Keep one tool result message per batch in model order, checkpointed at
      // each barrier so a crash never blindly repeats a completed write.
      const previous = this.store.session.messages.at(-1);
      if (previous?.role === 'tool') previous.content.push(...content.splice(0));
      else this.store.session.messages.push({ role: 'tool', content: content.splice(0) });
      await this.store.save();
    }
    signal.throwIfAborted();
    return { denied };
  }
}

