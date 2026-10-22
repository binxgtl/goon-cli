import { readdir, readFile } from 'node:fs/promises';
import { join } from 'node:path';
import { randomUUID } from 'node:crypto';
import { z } from 'zod';
import { Harness, type HarnessEvent } from './harness.js';
import { SessionStore } from './session.js';
import { ToolRegistry, PermissionDeniedError, type RunBudget } from './tools.js';
import { type RuntimeConfig } from './config.js';
import { limitText } from './files.js';
import { safeError } from './errors.js';
import { lastAssistantAnswer } from './reasoning.js';

export type SubagentProgress = Exclude<HarnessEvent, { type: 'text' | 'reasoning' | 'tool_stream' }>;
export type SubagentEvent =
  | { type: 'subagent'; id: string; state: 'start'; task: string }
  | { type: 'subagent'; id: string; state: 'progress'; event: SubagentProgress }
  | { type: 'subagent'; id: string; state: 'done' | 'error' };

export const CODEX_AGENT_NICKNAMES = [
  'Euclid', 'Archimedes', 'Hypatia', 'Galileo', 'Descartes', 'Pascal', 'Fermat', 'Leibniz',
  'Newton', 'Euler', 'Gauss', 'Faraday', 'Lovelace', 'Boole', 'Maxwell', 'Curie',
  'Planck', 'Tesla', 'Noether', 'Hilbert', 'Einstein', 'Bohr', 'Turing', 'Feynman',
  'Sagan', 'Socrates', 'Plato', 'Aristotle', 'Kant', 'Godel', 'Nash', 'Ramanujan', 'Erdos',
] as const;

export type AgentRoleType = 'default' | 'explorer' | 'worker';

export type CodexAgentStatusPayload =
  | 'running'
  | 'shutdown'
  | 'not_found'
  | { completed: string }
  | { errored: string };

export interface ManagedSubagent {
  id: string;
  nickname: string;
  agentType: AgentRoleType;
  readOnly: boolean;
  model: string;
  effort?: RuntimeConfig['effort'];
  task: string;
  status: 'running' | 'completed' | 'errored' | 'shutdown';
  lastOutput?: string;
  lastError?: string;
  updatedAt: string;
  controller: AbortController;
  currentPromise?: Promise<void>;
  waiters: Set<() => void>;
}

const tasksSchema = z.object({
  tasks: z
    .array(z.string().trim().min(1).max(16_000))
    .min(1)
    .max(8)
    .describe('List of independent task prompts (1 to 4 tasks) to run in isolated sub-agents.'),
});

const spawnAgentSchema = z.object({
  message: z
    .string()
    .trim()
    .min(1)
    .max(32_000)
    .describe('Initial task instructions for the new sub-agent. Include purpose, constraints, and file paths.'),
  agent_type: z
    .enum(['default', 'explorer', 'worker', 'research', 'self'])
    .optional()
    .describe('Sub-agent role: "explorer" (fast read-only codebase exploration), "worker" (coding/execution with explicit file ownership), or "default".'),
  nickname: z
    .string()
    .trim()
    .min(1)
    .max(64)
    .optional()
    .describe('Optional human-readable nickname for the sub-agent.'),
  fork_context: z
    .boolean()
    .optional()
    .describe('When true, fork the current conversation history into the child sub-agent before sending the initial message.'),
  model: z
    .string()
    .trim()
    .min(1)
    .max(128)
    .optional()
    .describe('Optional model override for this sub-agent.'),
  reasoning_effort: z
    .enum(['low', 'medium', 'high', 'xhigh', 'max'])
    .optional()
    .describe('Optional reasoning effort override for this sub-agent.'),
  wait: z
    .boolean()
    .optional()
    .describe('When true, wait for the sub-agent to complete and return its report immediately instead of running in the background.'),
});

const waitAgentSchema = z.object({
  targets: z
    .array(z.string().trim().min(1))
    .optional()
    .describe('Agent IDs or nicknames to wait for. Omit to wait for all currently running sub-agents.'),
  timeout_ms: z
    .number()
    .int()
    .min(100)
    .max(600_000)
    .default(120_000)
    .describe('Maximum wait time in milliseconds (default 120000).'),
});

const sendInputSchema = z.object({
  target: z
    .string()
    .trim()
    .min(1)
    .describe('Agent ID or nickname to message.'),
  message: z
    .string()
    .trim()
    .min(1)
    .max(32_000)
    .describe('Follow-up message or task instructions to send to the sub-agent.'),
  interrupt: z
    .boolean()
    .default(false)
    .describe('When true, interrupt the sub-agent current turn and start the new instruction immediately.'),
  wait: z
    .boolean()
    .default(false)
    .describe('When true, wait for the sub-agent turn to finish and return its output immediately.'),
});

const targetAgentSchema = z.object({
  target: z
    .string()
    .trim()
    .min(1)
    .describe('Agent ID or nickname to close or resume.'),
});

function formatStatusPayload(agent: ManagedSubagent | undefined): CodexAgentStatusPayload {
  if (!agent) return 'not_found';
  if (agent.status === 'running') return 'running';
  if (agent.status === 'shutdown') return 'shutdown';
  if (agent.status === 'completed') return { completed: agent.lastOutput ?? '' };
  return { errored: agent.lastError ?? 'Subagent failed.' };
}

function buildRolePrompt(role: AgentRoleType, task: string): string {
  if (role === 'explorer') {
    return (
      `You are a delegated agent. Role: explorer. Inspect the codebase using read-only tools (read_file, list_files, search) to answer the question below quickly and authoritatively with exact file and line evidence. Do not ask other agents.\n\n${task}`
    );
  }
  if (role === 'worker') {
    return (
      `You are a delegated agent. Role: worker. Complete the assigned implementation or verification task below. Respect file ownership boundaries, do not revert unrelated changes made by other agents, and return a concise report listing changed files and verification evidence. Do not ask other agents.\n\n${task}`
    );
  }
  return `You are a delegated agent. Complete this task and return a concise report with evidence. Do not ask other agents.\n\n${task}`;
}

// A single manager bounds concurrency across multiple model tool calls and manages Codex-style background agents.
export class Subagents {
  private active = 0;
  private waiters: (() => void)[] = [];
  private nicknameIndex = 0;
  private readonly usedNicknames = new Set<string>();
  readonly spawned = new Map<string, ManagedSubagent>();
  readonly metadataById = new Map<string, { nickname: string; role: AgentRoleType; task: string }>();

  constructor(
    readonly config: RuntimeConfig,
    readonly parent: SessionStore,
    readonly registry: ToolRegistry,
    readonly emit: (event: SubagentEvent) => void = () => {},
  ) {}

  get subagentsRoot(): string {
    return join(this.config.dataDir, 'subagents', this.parent.session.id).replace(/\\/g, '/');
  }

  private allocateNickname(preferred?: string): string {
    for (const a of this.spawned.values()) this.usedNicknames.add(a.nickname.toLowerCase());
    for (const m of this.metadataById.values()) this.usedNicknames.add(m.nickname.toLowerCase());
    if (preferred && !this.usedNicknames.has(preferred.toLowerCase())) {
      this.usedNicknames.add(preferred.toLowerCase());
      return preferred;
    }
    for (let i = 0; i < CODEX_AGENT_NICKNAMES.length; i++) {
      const candidate = CODEX_AGENT_NICKNAMES[(this.nicknameIndex + i) % CODEX_AGENT_NICKNAMES.length];
      if (!this.usedNicknames.has(candidate.toLowerCase())) {
        this.nicknameIndex = (this.nicknameIndex + i + 1) % CODEX_AGENT_NICKNAMES.length;
        this.usedNicknames.add(candidate.toLowerCase());
        return candidate;
      }
    }
    const fallback = `Agent-${this.usedNicknames.size + 1}`;
    this.usedNicknames.add(fallback.toLowerCase());
    return fallback;
  }

  private resolveManagedAgent(target: string): ManagedSubagent | undefined {
    const clean = target.trim();
    const direct = this.spawned.get(clean);
    if (direct) return direct;
    const lower = clean.toLowerCase().replace(/^\/root\//, '');
    for (const agent of this.spawned.values()) {
      if (
        agent.id.toLowerCase() === lower ||
        agent.id.toLowerCase().startsWith(lower) ||
        agent.nickname.toLowerCase() === lower
      ) {
        return agent;
      }
    }
    return undefined;
  }

  register() {
    const unregisters: Array<() => void> = [];

    unregisters.push(
      this.registry.register('delegate_task', {
        description:
          'Delegate up to four independent research/review tasks to agents with separate context. Two run concurrently. They can only read this workspace. Include all needed context; they do not inherit this conversation. Returns reports and saved session IDs.',
        schema: tasksSchema,
        readOnly: true,
        execute: (input, signal) => this.run(input.tasks, true, signal),
      }),
    );

    unregisters.push(
      this.registry.register('delegate_edit', {
        description:
          'Delegate up to four coding tasks to separate agents in this workspace. Tasks run sequentially; child writes/shell require the same approval policy. Children cannot spawn agents or access MCP. Include context and precise file ownership.',
        schema: tasksSchema,
        readOnly: false,
        execute: (input, signal) => this.run(input.tasks, false, signal),
      }),
    );

    unregisters.push(
      this.registry.register('spawn_agent', {
        description:
          'Spawn a sub-agent (Codex multi-agent workflow) for a well-scoped task. Returns `{ agent_id, nickname, agent_type, status }` immediately so you can spawn multiple agents in parallel and then call `wait_agent({ targets: [agent_id] })` to collect their final reports. Available roles (`agent_type`): `explorer` (fast read-only codebase research), `worker` (coding/execution with explicit file ownership), `default`.',
        schema: spawnAgentSchema,
        readOnly: true,
        execute: (input, signal) => this.spawnAgentTool(input, signal),
      }),
    );

    unregisters.push(
      this.registry.register('wait_agent', {
        description:
          'Wait for spawned sub-agents to reach a final status (`completed` or `errored`) and return their reports. Pass `targets: [agent_id, ...]` (or omit `targets` to wait for all active spawned agents).',
        schema: waitAgentSchema,
        readOnly: true,
        execute: (input, signal) => this.waitAgentTool(input, signal),
      }),
    );

    unregisters.push(
      this.registry.register('send_input', {
        description:
          'Send a follow-up message or task to an existing spawned sub-agent by `agent_id` or `nickname`, preserving its conversation history. Use `interrupt: true` to redirect work immediately, and then call `wait_agent` to collect its reply.',
        schema: sendInputSchema,
        readOnly: true,
        execute: (input, signal) => this.sendInputTool(input, signal),
      }),
    );

    unregisters.push(
      this.registry.register('close_agent', {
        description:
          'Close a spawned sub-agent by `target` (`agent_id` or `nickname`) when it is no longer needed and return its previous status.',
        schema: targetAgentSchema,
        readOnly: true,
        execute: (input) => this.closeAgentTool(input.target),
      }),
    );

    unregisters.push(
      this.registry.register('resume_agent', {
        description:
          'Resume a previously closed sub-agent by `id` (`agent_id` or `nickname`) so it can receive `send_input` and `wait_agent` calls.',
        schema: targetAgentSchema,
        readOnly: true,
        execute: (input) => this.resumeAgentTool(input.target),
      }),
    );

    unregisters.push(
      this.registry.register('list_agents', {
        description: 'List all spawned sub-agents in the current session with their IDs, nicknames, roles, and statuses.',
        schema: z.object({}),
        readOnly: true,
        execute: async () =>
          JSON.stringify({
            agents: [...this.spawned.values()].map(a => ({
              agent_id: a.id,
              agent_name: a.nickname,
              agent_type: a.agentType,
              agent_status: formatStatusPayload(a),
            })),
          }),
      }),
    );

    return () => {
      for (const fn of unregisters) fn();
    };
  }

  private async acquire(signal: AbortSignal, maxConcurrency = 2) {
    while (this.active >= maxConcurrency) {
      signal.throwIfAborted();
      await new Promise<void>((resolve, reject) => {
        const cleanup = () => {
          this.waiters = this.waiters.filter(w => w !== ready);
          signal.removeEventListener('abort', abort);
        };
        const ready = () => {
          cleanup();
          resolve();
        };
        const abort = () => {
          cleanup();
          reject(signal.reason ?? new Error('Cancelled'));
        };
        this.waiters.push(ready);
        signal.addEventListener('abort', abort, { once: true });
      });
    }
    signal.throwIfAborted();
    this.active++;
  }

  private release() {
    this.active--;
    this.waiters.shift()?.();
  }

  private notifyAgentWaiters(agent: ManagedSubagent) {
    for (const notify of agent.waiters) {
      notify();
    }
  }

  private async spawnAgentTool(
    input: z.infer<typeof spawnAgentSchema>,
    parentSignal: AbortSignal,
  ): Promise<string> {
    const rawRole = input.agent_type ?? (this.registry.effectiveReadOnly ? 'explorer' : 'default');
    const role: AgentRoleType =
      rawRole === 'explorer' || rawRole === 'research'
        ? 'explorer'
        : rawRole === 'worker'
          ? 'worker'
          : 'default';
    const childReadOnly = role === 'explorer' || this.registry.effectiveReadOnly;
    if (!childReadOnly && this.registry.readOnly) {
      throw new Error('Writable subagents unavailable in read-only mode. Use agent_type="explorer".');
    }

    const childConfig: RuntimeConfig = {
      ...this.config,
      ...(input.model ? { model: input.model } : {}),
      ...(input.reasoning_effort ? { effort: input.reasoning_effort } : {}),
    };
    const nickname = this.allocateNickname(input.nickname);
    const store = await SessionStore.open(this.subagentsRoot, childConfig);
    const id = store.session.id;
    store.session.title = `${nickname} [${role}]`;

    if (input.fork_context) {
      for (const msg of this.parent.session.messages) {
        if (msg.role === 'user' || (msg.role === 'assistant' && typeof msg.content === 'string')) {
          store.session.messages.push(structuredClone(msg));
        }
      }
      await store.save();
    }
    await store.close();

    const controller = new AbortController();
    const onParentAbort = () => controller.abort(parentSignal.reason ?? new Error('Parent turn cancelled'));
    parentSignal.addEventListener('abort', onParentAbort, { once: true });

    const managed: ManagedSubagent = {
      id,
      nickname,
      agentType: role,
      readOnly: childReadOnly,
      model: childConfig.model,
      effort: childConfig.effort,
      task: input.message,
      status: 'running',
      updatedAt: new Date().toISOString(),
      controller,
      waiters: new Set(),
    };
    this.spawned.set(id, managed);
    this.metadataById.set(id, { nickname, role, task: input.message });

    const budget = this.registry.budget ?? { spent: 0, limit: this.config.runTokenBudget };
    const formattedPrompt = buildRolePrompt(role, input.message);

    const runPromise = this.executeManagedTurn(managed, childConfig, formattedPrompt, budget).finally(() => {
      parentSignal.removeEventListener('abort', onParentAbort);
    });
    managed.currentPromise = runPromise;

    if (input.wait) {
      await runPromise;
      return JSON.stringify({
        agent_id: id,
        nickname,
        agent_type: role,
        status: formatStatusPayload(managed),
      });
    }

    return JSON.stringify({
      agent_id: id,
      nickname,
      agent_type: role,
      status: 'running',
    });
  }

  private async executeManagedTurn(
    managed: ManagedSubagent,
    childConfig: RuntimeConfig,
    prompt: string,
    budget: RunBudget,
  ): Promise<void> {
    const signal = managed.controller.signal;
    let store: SessionStore | undefined;
    let denied = false;
    let failure: { error: unknown } | undefined;
    let acquired = false;
    try {
      await this.acquire(signal, managed.readOnly ? 4 : 2);
      acquired = true;
      if (budget.limit > 0 && budget.spent >= budget.limit) {
        throw new Error('Shared run token budget reached.');
      }
      store = await SessionStore.open(this.subagentsRoot, childConfig, managed.id);
      this.emit({ type: 'subagent', id: managed.id, task: managed.task, state: 'start' });
      const tools = new ToolRegistry(
        childConfig.cwd,
        async request => {
          const allowed = await this.registry.approve(request);
          denied ||= !allowed;
          return allowed;
        },
        managed.readOnly || this.registry.readOnly,
      );
      const childHarness = new Harness(childConfig, store, tools, event => {
        if (event.type !== 'text' && event.type !== 'reasoning' && event.type !== 'tool_stream') {
          this.emit({ type: 'subagent', id: managed.id, state: 'progress', event });
        }
      });
      // The task text was written by the parent model, not typed by the user.
      childHarness.untrustedPrompts = true;
      await childHarness.run(prompt, signal, budget);
      if (denied) throw new PermissionDeniedError('Permission denied in a subagent. Stop this run.');
      const answer = lastAssistantAnswer(store.session.messages, childConfig.inlineReasoning);
      managed.lastOutput = limitText(answer, 12_000);
      managed.lastError = undefined;
      if ((managed.status as string) !== 'shutdown') {
        managed.status = 'completed';
      }
    } catch (error) {
      failure = { error };
      managed.lastError = safeError(error, childConfig.apiKey);
      if ((managed.status as string) !== 'shutdown') {
        managed.status = 'errored';
      }
    }
    try {
      const lastCp = store?.session.checkpoints?.at(-1);
      if (lastCp) {
        this.registry.files.mergeBackups(lastCp.files);
      }
      await store?.close();
    } catch (error) {
      failure ??= { error };
      managed.lastError = safeError(error, childConfig.apiKey);
      if ((managed.status as string) !== 'shutdown') {
        managed.status = 'errored';
      }
    } finally {
      if (acquired) this.release();
      managed.updatedAt = new Date().toISOString();
      if (store) {
        this.emit({ type: 'subagent', id: managed.id, state: failure ? 'error' : 'done' });
      }
      this.notifyAgentWaiters(managed);
    }
  }

  private async waitAgentTool(
    input: z.infer<typeof waitAgentSchema>,
    signal: AbortSignal,
  ): Promise<string> {
    const requestedTargets =
      input.targets && input.targets.length > 0
        ? input.targets
        : [...this.spawned.values()]
            .filter(a => a.status !== 'shutdown')
            .map(a => a.id);

    if (!requestedTargets.length) {
      return JSON.stringify({ status: {}, timed_out: false });
    }

    const resolved: Array<{ key: string; agent?: ManagedSubagent }> = requestedTargets.map(t => ({
      key: t,
      agent: this.resolveManagedAgent(t),
    }));

    const isDone = (a: ManagedSubagent | undefined) => !a || a.status !== 'running';

    if (!resolved.every(r => isDone(r.agent))) {
      const timeoutMs = Math.max(100, Math.min(600_000, input.timeout_ms ?? 120_000));
      await new Promise<void>((resolve, reject) => {
        const timer = setTimeout(() => {
          cleanup();
          resolve();
        }, timeoutMs);
        const onAbort = () => {
          cleanup();
          reject(signal.reason ?? new Error('Wait cancelled'));
        };
        const check = () => {
          if (resolved.every(r => isDone(r.agent))) {
            cleanup();
            resolve();
          }
        };
        const cleanup = () => {
          clearTimeout(timer);
          signal.removeEventListener('abort', onAbort);
          for (const r of resolved) {
            r.agent?.waiters.delete(check);
          }
        };
        signal.addEventListener('abort', onAbort, { once: true });
        for (const r of resolved) {
          r.agent?.waiters.add(check);
        }
        check();
      });
    }

    const statusMap: Record<string, CodexAgentStatusPayload> = {};
    let anyCompletedOrFinal = false;
    for (const { key, agent } of resolved) {
      const payload = formatStatusPayload(agent);
      const outKey = agent ? agent.id : key;
      statusMap[outKey] = payload;
      if (payload !== 'running') anyCompletedOrFinal = true;
    }

    return JSON.stringify({
      status: statusMap,
      timed_out: !anyCompletedOrFinal,
    });
  }

  private async sendInputTool(
    input: z.infer<typeof sendInputSchema>,
    parentSignal: AbortSignal,
  ): Promise<string> {
    const agent = this.resolveManagedAgent(input.target);
    if (!agent) {
      throw new Error(`Agent with target "${input.target}" not found.`);
    }
    if (agent.status === 'shutdown') {
      throw new Error(`Agent "${agent.nickname}" (${agent.id}) is closed. Call resume_agent first.`);
    }
    if (agent.status === 'running') {
      if (input.interrupt) {
        agent.controller.abort(new Error('Interrupted by send_input'));
        await agent.currentPromise?.catch(() => {});
      } else {
        await agent.currentPromise?.catch(() => {});
      }
    }

    agent.controller = new AbortController();
    agent.status = 'running';
    agent.task = input.message;
    const onParentAbort = () => agent.controller.abort(parentSignal.reason ?? new Error('Parent turn cancelled'));
    parentSignal.addEventListener('abort', onParentAbort, { once: true });

    const childConfig: RuntimeConfig = {
      ...this.config,
      model: agent.model,
      ...(agent.effort ? { effort: agent.effort } : {}),
    };
    const budget = this.registry.budget ?? { spent: 0, limit: this.config.runTokenBudget };
    const runPromise = this.executeManagedTurn(agent, childConfig, input.message, budget).finally(() => {
      parentSignal.removeEventListener('abort', onParentAbort);
    });
    agent.currentPromise = runPromise;

    const submissionId = randomUUID();
    if (input.wait) {
      await runPromise;
      return JSON.stringify({
        submission_id: submissionId,
        agent_id: agent.id,
        nickname: agent.nickname,
        status: formatStatusPayload(agent),
      });
    }
    return JSON.stringify({
      submission_id: submissionId,
      agent_id: agent.id,
      nickname: agent.nickname,
      status: 'running',
    });
  }

  private async closeAgentTool(target: string): Promise<string> {
    const agent = this.resolveManagedAgent(target);
    if (!agent) {
      return JSON.stringify({ previous_status: 'not_found' });
    }
    const previousStatus = formatStatusPayload(agent);
    agent.status = 'shutdown';
    agent.controller.abort(new Error('Agent closed by parent'));
    await agent.currentPromise?.catch(() => {});
    this.notifyAgentWaiters(agent);
    return JSON.stringify({ previous_status: previousStatus });
  }

  private async resumeAgentTool(target: string): Promise<string> {
    const agent = this.resolveManagedAgent(target);
    if (!agent) {
      throw new Error(`Agent with target "${target}" not found.`);
    }
    if (agent.status === 'shutdown') {
      agent.status = agent.lastError ? 'errored' : 'completed';
    }
    return JSON.stringify({ status: formatStatusPayload(agent) });
  }

  async closeAll(): Promise<void> {
    const runningPromises: Promise<void>[] = [];
    for (const agent of this.spawned.values()) {
      if (agent.status === 'running') {
        agent.status = 'shutdown';
        agent.controller.abort(new Error('Parent session closing'));
        if (agent.currentPromise) runningPromises.push(agent.currentPromise.catch(() => {}));
      }
      this.notifyAgentWaiters(agent);
    }
    await Promise.all(runningPromises);
  }

  async listSubagentSummaries(): Promise<
    Array<{
      id: string;
      nickname: string;
      role: string;
      status: string;
      task: string;
      summary?: string;
      updatedAt: string;
      messagesCount: number;
    }>
  > {
    const summaries: Array<{
      id: string;
      nickname: string;
      role: string;
      status: string;
      task: string;
      summary?: string;
      updatedAt: string;
      messagesCount: number;
    }> = [];
    const seen = new Set<string>();

    for (const agent of this.spawned.values()) {
      seen.add(agent.id);
      const out = agent.lastOutput ?? agent.lastError;
      summaries.push({
        id: agent.id,
        nickname: agent.nickname,
        role: agent.agentType,
        status: agent.status,
        task: agent.task,
        ...(out ? { summary: out.replace(/\s+/g, ' ').slice(0, 140) } : {}),
        updatedAt: agent.updatedAt,
        messagesCount: 0,
      });
    }

    try {
      const diskList = await SessionStore.list(this.subagentsRoot);
      for (const item of diskList) {
        if (seen.has(item.id)) {
          const existing = summaries.find(s => s.id === item.id);
          if (existing) existing.messagesCount = item.messagesCount ?? 0;
          continue;
        }
        const meta = this.metadataById.get(item.id);
        summaries.push({
          id: item.id,
          nickname: meta?.nickname ?? item.title ?? item.id.slice(0, 8),
          role: meta?.role ?? 'default',
          status: 'completed',
          task: meta?.task ?? item.preview ?? '',
          updatedAt: item.updatedAt,
          messagesCount: item.messagesCount ?? 0,
        });
      }
    } catch {
      // Directory may not exist yet if no subagents were spawned
    }
    return summaries;
  }

  async readSubagentTranscript(target: string): Promise<string | undefined> {
    const managed = this.resolveManagedAgent(target);
    const root = this.subagentsRoot;
    let targetId = managed?.id;
    if (!targetId) {
      try {
        const files = (await readdir(root)).filter(f => f.endsWith('.json'));
        const match = files.find(f => f.slice(0, -5).toLowerCase().startsWith(target.trim().toLowerCase()));
        if (match) targetId = match.slice(0, -5);
      } catch {
        return undefined;
      }
    }
    if (!targetId) return undefined;
    try {
      const raw = JSON.parse(await readFile(join(root, `${targetId}.json`), 'utf8')) as {
        id: string;
        title?: string;
        messages?: Array<{ role: string; content: unknown }>;
      };
      const answer = lastAssistantAnswer((raw.messages ?? []) as Parameters<typeof lastAssistantAnswer>[0], this.config.inlineReasoning);
      const nickname = managed?.nickname ?? this.metadataById.get(targetId)?.nickname ?? raw.title ?? targetId.slice(0, 8);
      const report = answer || managed?.lastOutput || managed?.lastError || '(no output yet)';
      return `Subagent ${nickname} (${targetId}):\n${report}`;
    } catch {
      if (managed) {
        const report = managed.lastOutput || managed.lastError || `Status: ${managed.status}`;
        return `Subagent ${managed.nickname} (${managed.id}):\n${report}`;
      }
      return undefined;
    }
  }

  private async child(task: string, readOnly: boolean, signal: AbortSignal, budget: RunBudget): Promise<string> {
    await this.acquire(signal);
    let store: SessionStore | undefined;
    let denied = false;
    let report = '';
    let failure: { error: unknown } | undefined;
    try {
      if (budget.limit && budget.spent >= budget.limit) throw new Error('Shared run token budget reached.');
      store = await SessionStore.open(this.subagentsRoot, this.config);
      const id = store.session.id;
      const nickname = this.allocateNickname();
      const role: AgentRoleType = readOnly ? 'explorer' : 'worker';
      this.metadataById.set(id, { nickname, role, task });
      this.emit({ type: 'subagent', id, task, state: 'start' });
      const tools = new ToolRegistry(
        this.config.cwd,
        async request => {
          const allowed = await this.registry.approve(request);
          denied ||= !allowed;
          return allowed;
        },
        readOnly || this.registry.readOnly,
      );
      // No delegation tools are registered in children: depth is exactly one.
      const config = { ...this.config };
      const childHarness = new Harness(config, store, tools, event => {
        // Parent progress is a control channel. The complete text/reasoning
        // belongs to the child transcript, never a per-token parent mirror.
        if (event.type !== 'text' && event.type !== 'reasoning' && event.type !== 'tool_stream') {
          this.emit({ type: 'subagent', id, state: 'progress', event });
        }
      });
      childHarness.untrustedPrompts = true;
      await childHarness.run(
        `You are a delegated agent. Complete this task and return a concise report with evidence. Do not ask other agents.\n\n${task}`,
        signal,
        budget,
      );
      if (denied) throw new PermissionDeniedError('Permission denied in a subagent. Stop this run.');
      report = `Session ${id}\nTask: ${task}\n${limitText(lastAssistantAnswer(store.session.messages, config.inlineReasoning), 12_000)}`;
    } catch (error) {
      failure = { error };
    }
    try {
      if (store?.session.checkpoints?.length) {
        for (const cp of store.session.checkpoints) {
          this.registry.files.mergeBackups(cp.files);
        }
      }
      await store?.close();
    } catch (error) {
      // Preserve a primary run/denial error if cleanup also fails; a successful
      // run with failed cleanup must never announce done or return success.
      failure ??= { error };
    } finally {
      this.release();
      if (store) this.emit({ type: 'subagent', id: store.session.id, state: failure ? 'error' : 'done' });
    }
    if (failure) throw failure.error;
    return report;
  }

  private async run(tasks: string[], readOnly: boolean, signal: AbortSignal) {
    if (!readOnly && this.registry.readOnly) throw new Error('Writable subagents unavailable in read-only mode.');
    const budget = this.registry.budget ?? { spent: 0, limit: this.config.runTokenBudget };
    const results: string[] = Array.from({ length: tasks.length }, () => 'Skipped.');
    let next = 0;
    let denied = false;
    const worker = async () => {
      while (next < tasks.length && !signal.aborted && !denied) {
        const index = next++;
        try {
          results[index] = await this.child(tasks[index], readOnly, signal, budget);
        } catch (error) {
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
