import { join } from 'node:path';
import { z } from 'zod';
import { Harness, type HarnessEvent } from './harness.js';
import { SessionStore } from './session.js';
import { ToolRegistry, PermissionDeniedError, type RunBudget } from './tools.js';
import type { RuntimeConfig } from './config.js';
import { limitText } from './files.js';
import { safeError } from './errors.js';
import { lastAssistantAnswer } from './reasoning.js';

export type SubagentProgress = Exclude<HarnessEvent, { type: 'text' | 'reasoning' | 'tool_stream' }>;
export type SubagentEvent =
  | { type: 'subagent'; id: string; state: 'start'; task: string }
  | { type: 'subagent'; id: string; state: 'progress'; event: SubagentProgress }
  | { type: 'subagent'; id: string; state: 'done' | 'error' };
const tasksSchema = z.object({ tasks: z.array(z.string().trim().min(1).max(16_000)).min(1).max(4) });
// A single manager bounds concurrency across multiple model tool calls.
export class Subagents {
  private active = 0;
  private waiters: (() => void)[] = [];
  constructor(readonly config: RuntimeConfig, readonly parent: SessionStore, readonly registry: ToolRegistry,
    readonly emit: (event: SubagentEvent) => void = () => {}) {}
  register() {
    const read = this.registry.register('delegate_task', {
      description: 'Delegate up to four independent research/review tasks to agents with separate context. Two run concurrently. They can only read this workspace. Include all needed context; they do not inherit this conversation. Returns reports and saved session IDs.',
      schema: tasksSchema, readOnly: true, execute: (input, signal) => this.run(input.tasks, true, signal),
    });
    const edit = this.registry.register('delegate_edit', {
      description: 'Delegate up to four coding tasks to separate agents in this workspace. Tasks run sequentially; child writes/shell require the same approval policy. Children cannot spawn agents or access MCP. Include context and precise file ownership.',
      schema: tasksSchema, readOnly: false, execute: (input, signal) => this.run(input.tasks, false, signal),
    });
    return () => { read(); edit(); };
  }
  private async acquire(signal: AbortSignal) {
    while (this.active >= 2) {
      signal.throwIfAborted();
      await new Promise<void>((resolve, reject) => {
        const cleanup = () => { this.waiters = this.waiters.filter(w => w !== ready); signal.removeEventListener('abort', abort); };
        const ready = () => { cleanup(); resolve(); };
        const abort = () => { cleanup(); reject(signal.reason ?? new Error('Cancelled')); };
        this.waiters.push(ready); signal.addEventListener('abort', abort, { once: true });
      });
    }
    signal.throwIfAborted(); this.active++;
  }
  private release() { this.active--; this.waiters.shift()?.(); }
  private async child(task: string, readOnly: boolean, signal: AbortSignal, budget: RunBudget): Promise<string> {
    await this.acquire(signal);
    let store: SessionStore | undefined, denied = false;
    let report = '';
    let failure: { error: unknown } | undefined;
    try {
      if (budget.limit && budget.spent >= budget.limit) throw new Error('Shared run token budget reached.');
      store = await SessionStore.open(join(this.config.dataDir, 'subagents', this.parent.session.id), this.config);
      const id = store.session.id;
      this.emit({ type: 'subagent', id, task, state: 'start' });
      const tools = new ToolRegistry(this.config.cwd, async request => {
        const allowed = await this.registry.approve(request);
        denied ||= !allowed;
        return allowed;
      }, readOnly || this.registry.readOnly);
      // No delegation tools are registered in children: depth is exactly one.
      const config = { ...this.config, runTokenBudget: Math.min(this.config.runTokenBudget || 30_000, 30_000) };
      await new Harness(config, store, tools, event => {
        // Parent progress is a control channel. The complete text/reasoning
        // belongs to the child transcript, never a per-token parent mirror.
        if (event.type !== 'text' && event.type !== 'reasoning' && event.type !== 'tool_stream') {
          this.emit({ type: 'subagent', id, state: 'progress', event });
        }
      }).run(
        `You are a delegated agent. Complete this task and return a concise report with evidence. Do not ask other agents.\n\n${task}`, signal, budget);
      if (denied) throw new PermissionDeniedError('Permission denied in a subagent. Stop this run.');
      report = `Session ${id}\nTask: ${task}\n${limitText(lastAssistantAnswer(store.session.messages, config.inlineReasoning), 12_000)}`;
    } catch (error) { failure = { error }; }
    try { await store?.close(); }
    // Preserve a primary run/denial error if cleanup also fails; a successful
    // run with failed cleanup must never announce done or return success.
    catch (error) { failure ??= { error }; }
    finally {
      this.release();
      if (store) this.emit({ type: 'subagent', id: store.session.id, state: failure ? 'error' : 'done' });
    }
    if (failure) throw failure.error;
    return report;
  }
  private async run(tasks: string[], readOnly: boolean, signal: AbortSignal) {
    if (!readOnly && this.registry.readOnly) throw new Error('Writable subagents unavailable in read-only mode.');
    const budget = this.registry.budget;
    if (!budget) throw new Error('Subagents require an active harness run.');
    const results: string[] = Array.from({ length: tasks.length }, () => 'Skipped.');
    let next = 0, denied = false;
    const worker = async () => {
      while (next < tasks.length && !signal.aborted && !denied) {
        const index = next++;
        try { results[index] = await this.child(tasks[index], readOnly, signal, budget); }
        catch (error) {
          if (error instanceof PermissionDeniedError) denied = true;
          results[index] = `Task failed: ${safeError(error, this.config.apiKey)}`;
        }
      }
    };
    await Promise.all(Array.from({ length: readOnly ? Math.min(2, tasks.length) : 1 }, worker));
    signal.throwIfAborted();
    if (denied) throw new PermissionDeniedError('Permission denied in a subagent. Stop this run.');
    return limitText(results.join('\n\n'), 32_000);
  }
}
