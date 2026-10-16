import { streamText, type LanguageModel, type ToolResultPart } from 'ai';
import type { RuntimeConfig } from './config.js';
import { SessionStore, repairInterrupted } from './session.js';
import { ToolRegistry, type ToolCall, type RunBudget } from './tools.js';
import { createModel } from './provider.js';
import { safeError } from './errors.js';
import { compactIfNeeded, estimatedTokens, systemPrompt } from './context.js';
import { AnswerProjection, type AnswerPart } from './reasoning.js';

export type HarnessEvent =
  | { type: 'text'; text: string }
  | { type: 'reasoning'; text: string; source: 'inline' | 'provider' }
  | { type: 'tool'; name: string; id: string; state: 'start' | 'done' | 'error' }
  | { type: 'usage'; input: number; output: number; total: number }
  | { type: 'compaction'; count: number }
  | { type: 'stopped'; reason: string };

export class Harness {
  private running = false;
  private readonly model: LanguageModel;
  constructor(readonly config: RuntimeConfig, readonly store: SessionStore, readonly tools: ToolRegistry,
    readonly emit: (event: HarnessEvent) => void = () => {}, model?: LanguageModel) {
    this.model = model ?? createModel(config);
  }
  async run(prompt: string, signal: AbortSignal, sharedBudget?: RunBudget): Promise<string> {
    if (this.running) throw new Error('This harness already has an active run.');
    this.running = true;
    let text = '', spent = 0, measuredInput = 0;
    const budget = sharedBudget ?? { spent: 0, limit: this.config.runTokenBudget };
    this.tools.budget = budget;
    const account = (tokens: number) => { spent += tokens; budget.spent += tokens; };
    const exhausted = () => (this.config.runTokenBudget > 0 && spent >= this.config.runTokenBudget) || (budget.limit > 0 && budget.spent >= budget.limit);
    const session = this.store.session;
    try {
      signal.throwIfAborted();
      const system = await systemPrompt(this.config, this.tools.readOnly);
      session.messages.push({ role: 'user', content: prompt });
      await this.store.save();
      for (;;) {
        signal.throwIfAborted();
        if (exhausted()) throw new Error('Run token budget reached. Resume with a new prompt or increase --run-token-budget.');
        const oldCount = session.compactions;
        account(await compactIfNeeded(this.store, this.config, this.model, system, signal, measuredInput));
        if (session.compactions !== oldCount) { measuredInput = 0; this.emit({ type: 'compaction', count: session.compactions }); }
        if (exhausted()) throw new Error('Run token budget reached during compaction. Original history is archived; resume to continue.');
        const result = streamText({
          model: this.model, system, messages: session.messages, tools: this.tools.schemas(),
          maxOutputTokens: this.config.maxOutputTokens, maxRetries: 2,
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
    } finally { this.running = false; this.tools.budget = undefined; }
  }
  private async executeBatch(calls: ToolCall[], signal: AbortSignal): Promise<{ denied: boolean }> {
    const content: ToolResultPart[] = [];
    let denied = false;
    const execute = async (call: ToolCall) => {
      let output: ToolResultPart['output'];
      try {
        if (denied || signal.aborted) throw new Error(denied ? 'Skipped after permission denial.' : 'Cancelled before dispatch.');
        this.emit({ type: 'tool', name: call.toolName, id: call.toolCallId, state: 'start' });
        const result = await this.tools.execute(call, signal);
        denied ||= result.denied === true;
        output = { type: result.denied ? 'error-text' : 'text', value: result.text };
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
