/**
 * Multi-agent DAG coordination (v0.6, P20/P21).
 *
 * Pure planning / scheduling / merge logic plus the `runDag` orchestrator. This module never
 * calls a model and never spawns a shell by itself: model access, workspace spawn and command
 * execution are hooks owned by the caller (the CLI wires them to `Subagents.runTasks`,
 * `runVerification` and the parent approval policy).
 *
 * Safety invariants:
 * - A plan that fails `validatePlan` never spawns anything (`dag_plan_invalid`).
 * - A todo whose dependency failed/blocked is `blocked`, never dispatched.
 * - Write roles (coder) run one at a time; every write todo is snapshotted with
 *   `WorkspaceFiles.beginTurn()`/`takeTurnBackups()` and rolled back on rejection, test
 *   failure, plan failure, cancellation and failed verification. Restores never clobber
 *   user edits (they are reported as conflicts and the todo fails instead).
 * - Every role gets a pinned toolset (`ROLE_TOOLSETS`); MCP tools are not exposed to DAG
 *   roles unless the caller adds them explicitly. Toolsets are enforced in the child
 *   `ToolRegistry`, so a hidden tool call is rejected by `execute()` itself.
 * - `runVerification` is fail-closed: a hook with commands but no approve callback throws
 *   `VerificationApprovalError` instead of running anything.
 */
import { z } from 'zod';
import { computeUnifiedDiff, type WorkspaceFiles } from './files.js';
import { CostLedger, createRunBudget, formatDurationMs, formatUsd } from './cost.js';
import { safeError } from './errors.js';
import type { RunBudget } from './tools.js';
import type { FileBackup } from './session.js';
import {
  createVerifyRunner,
  formatVerifyErrors,
  planVerification,
  redactText,
  redactVerdict,
  runVerification,
  type VerifyConfig,
  type VerifyPlan,
  type VerifyPolicy,
  type VerifyRunner,
  type VerifyVerdict,
} from './verify.js';

export type TodoStatus = 'pending' | 'in_progress' | 'blocked' | 'done' | 'failed';
export type AgentRole = 'planner' | 'coder' | 'reviewer' | 'tester' | 'researcher';

export const AGENT_ROLES = ['planner', 'coder', 'reviewer', 'tester', 'researcher'] as const;
export const agentRoleSchema = z.enum(AGENT_ROLES);

/** Roles that may modify the workspace. Everything else is read-only exploration. */
export const WRITE_ROLES: ReadonlySet<AgentRole> = new Set<AgentRole>(['coder']);

/**
 * Toolsets per role, enforced in the child `ToolRegistry` (not just the prompt). MCP tools are
 * deliberately absent: `multiAgentMcpTools`/`DagOptions.allowedMcpTools` must opt in explicitly.
 * Peer/collaboration tools are never exposed to DAG roles.
 */
export const ROLE_TOOLSETS: Record<AgentRole, string[]> = {
  planner: ['read_file', 'list_files', 'search'],
  reviewer: ['read_file', 'list_files', 'search'],
  researcher: ['read_file', 'list_files', 'search'],
  coder: ['read_file', 'list_files', 'search', 'write_file', 'edit_file', 'apply_patch', 'bash'],
  tester: ['read_file', 'list_files', 'search', 'bash'],
};

/** Role toolset plus explicit MCP opt-ins (`mcp__server__tool` or `mcp__*` wildcards). */
export function roleToolset(role: AgentRole, allowedMcpTools: readonly string[] = []): string[] {
  return [...ROLE_TOOLSETS[role], ...allowedMcpTools];
}

const todoInputSchema = z.object({
  id: z.string().trim().min(1).max(64),
  title: z.string().trim().min(1).max(400),
  acceptance: z.string().max(8000).default(''),
  dependsOn: z.array(z.string().trim().min(1).max(64)).max(16).default([]),
  role: agentRoleSchema,
  assignee: z.string().trim().max(80).optional(),
  artifacts: z.array(z.string().trim().min(1)).max(32).default([]),
});

/** Shape the planner model is asked to produce; statuses/rework are owned by the runtime. */
export const plannerPlanSchema = z.object({
  goal: z.string().trim().min(1).max(8000),
  todos: z.array(todoInputSchema).min(1).max(16),
});
export type PlannerPlan = z.infer<typeof plannerPlanSchema>;

export interface Todo {
  id: string;
  title: string;
  acceptance: string;
  dependsOn: string[];
  status: TodoStatus;
  role: AgentRole;
  /** Display label only; spawning always uses `role`, never the assignee string. */
  assignee?: string;
  artifacts: string[];
  reworkCycles: number;
}

export interface Plan {
  id: string;
  goal: string;
  todos: Todo[];
  createdAt: string;
}

/** Builds a runtime `Plan` from raw planner output (already JSON-parsed by the caller). */
export function planFromPlanner(raw: unknown, id: string, createdAt = new Date().toISOString()): Plan {
  const parsed = plannerPlanSchema.parse(raw);
  return {
    id,
    goal: parsed.goal,
    createdAt,
    todos: parsed.todos.map(todo => ({
      id: todo.id,
      title: todo.title,
      acceptance: todo.acceptance,
      dependsOn: [...new Set(todo.dependsOn)],
      status: 'pending' as const,
      role: todo.role,
      ...(todo.assignee !== undefined ? { assignee: todo.assignee } : {}),
      artifacts: [...todo.artifacts],
      reworkCycles: 0,
    })),
  };
}

export class PlanValidationError extends Error {
  constructor(message: string) {
    super(message);
    this.name = 'PlanValidationError';
  }
}
export class PlanDeadlockError extends Error {
  constructor(readonly todoIds: string[]) {
    super(`Plan deadlocked: no dispatchable todo but ${todoIds.length} still pending (${todoIds.join(', ')}).`);
    this.name = 'PlanDeadlockError';
  }
}
export class ErrNotVerified extends Error {
  constructor(reason: string, readonly conflicts: MergeConflict[] = []) {
    super(`Plan result not verified: ${reason}`);
    this.name = 'ErrNotVerified';
  }
}

/**
 * Structural plan validation: non-empty unique ids, existing deps, no self-deps, acyclic
 * (Kahn), and valid roles. Throws `PlanValidationError`; the caller emits `dag_plan_invalid`.
 */
export function validatePlan(plan: Plan): void {
  if (!plan.todos.length) throw new PlanValidationError('Plan has no todos.');
  const seen = new Set<string>();
  for (const todo of plan.todos) {
    if (!todo.id.trim()) throw new PlanValidationError('Todo id must not be empty.');
    if (seen.has(todo.id)) throw new PlanValidationError(`Duplicate todo id: ${todo.id}.`);
    seen.add(todo.id);
  }
  for (const todo of plan.todos) {
    if (!AGENT_ROLES.includes(todo.role)) {
      throw new PlanValidationError(`Todo ${todo.id} has invalid role: ${String(todo.role)}.`);
    }
    for (const dep of todo.dependsOn) {
      if (dep === todo.id) throw new PlanValidationError(`Todo ${todo.id} depends on itself.`);
      if (!seen.has(dep)) throw new PlanValidationError(`Todo ${todo.id} depends on unknown todo: ${dep}.`);
    }
  }
  const indegree = new Map<string, number>();
  const dependents = new Map<string, string[]>();
  for (const todo of plan.todos) {
    indegree.set(todo.id, todo.dependsOn.length);
    for (const dep of todo.dependsOn) {
      dependents.set(dep, [...(dependents.get(dep) ?? []), todo.id]);
    }
  }
  const queue = plan.todos.filter(todo => indegree.get(todo.id) === 0).map(todo => todo.id);
  const sorted = new Set<string>();
  for (let i = 0; i < queue.length; i++) {
    const id = queue[i]!;
    sorted.add(id);
    for (const next of dependents.get(id) ?? []) {
      const left = (indegree.get(next) ?? 0) - 1;
      indegree.set(next, left);
      if (left === 0) queue.push(next);
    }
  }
  if (sorted.size !== plan.todos.length) {
    const cyclic = plan.todos.filter(todo => !sorted.has(todo.id)).map(todo => todo.id);
    throw new PlanValidationError(`Plan contains a dependency cycle involving: ${cyclic.join(', ')}.`);
  }
}

/**
 * DAG dispatcher over a validated plan. `depsMet` is true only when every dependency is
 * `done`; a `failed`/`blocked` dependency blocks the dependent (propagating transitively).
 */
export class Scheduler {
  private readonly inflight = new Set<string>();

  constructor(readonly plan: Plan, readonly writeConcurrency = 1, readonly maxConcurrency = 2) {}

  todo(id: string): Todo | undefined {
    return this.plan.todos.find(todo => todo.id === id);
  }

  statusOf(id: string): TodoStatus {
    return this.plan.todos.find(todo => todo.id === id)?.status ?? 'pending';
  }

  /** True only when every dependency is `done`. */
  depsMet(todo: Todo): boolean {
    return todo.dependsOn.every(dep => this.statusOf(dep) === 'done');
  }

  /** First dependency that failed or blocked, if any. */
  blockedBy(todo: Todo): Todo | undefined {
    return todo.dependsOn
      .map(dep => this.todo(dep))
      .find(dep => dep !== undefined && (dep.status === 'failed' || dep.status === 'blocked'));
  }

  /** Pending todos whose dependencies are all done, in plan order. */
  readyTodos(): Todo[] {
    return this.plan.todos.filter(todo => todo.status === 'pending' && this.depsMet(todo));
  }

  inflightCount(): number {
    return this.inflight.size;
  }

  /**
   * Marks every ready todo `in_progress` (up to the write/read bounds) and returns them in
   * plan order. A todo already inflight is never re-dispatched.
   */
  dispatchReady(): Todo[] {
    let writers = [...this.inflight].map(id => this.todo(id)).filter(t => t && WRITE_ROLES.has(t.role)).length;
    let readers = this.inflight.size - writers;
    const dispatched: Todo[] = [];
    for (const todo of this.plan.todos) {
      if (todo.status !== 'pending' || this.inflight.has(todo.id) || !this.depsMet(todo)) continue;
      const isWrite = WRITE_ROLES.has(todo.role);
      if (isWrite && writers >= Math.max(1, this.writeConcurrency)) continue;
      if (!isWrite && readers >= Math.max(0, this.maxConcurrency)) continue;
      todo.status = 'in_progress';
      this.inflight.add(todo.id);
      dispatched.push(todo);
      if (isWrite) writers++;
      else readers++;
    }
    return dispatched;
  }

  markDispatched(id: string): void {
    this.inflight.add(id);
  }

  /** Releases the inflight slot; callers set the terminal status first. */
  release(id: string): void {
    this.inflight.delete(id);
  }

  /** Requeues a todo for another attempt (rework): back to pending, inflight released. */
  requeue(id: string): void {
    const todo = this.todo(id);
    if (!todo) return;
    todo.status = 'pending';
    this.inflight.delete(id);
  }

  markDone(id: string): void {
    const todo = this.todo(id);
    if (!todo) return;
    todo.status = 'done';
    this.inflight.delete(id);
  }

  /** Marks the todo `failed` and blocks every pending todo that (transitively) depends on it. */
  markFailed(id: string): string[] {
    const todo = this.todo(id);
    if (!todo) return [];
    todo.status = 'failed';
    this.inflight.delete(id);
    return this.propagateBlocks();
  }

  /** Blocks all pending dependents of failed/blocked todos. Fixpoint; returns newly blocked ids. */
  propagateBlocks(): string[] {
    const blocked: string[] = [];
    let changed = true;
    while (changed) {
      changed = false;
      for (const todo of this.plan.todos) {
        if (todo.status !== 'pending') continue;
        if (this.blockedBy(todo) !== undefined) {
          todo.status = 'blocked';
          blocked.push(todo.id);
          changed = true;
        }
      }
    }
    return blocked;
  }

  /** Pending todo ids when nothing is inflight and nothing can be dispatched. */
  checkDeadlock(): string[] {
    if (this.inflight.size > 0 || this.readyTodos().length > 0) return [];
    return this.plan.todos.filter(todo => todo.status === 'pending').map(todo => todo.id);
  }
}

export const DEFAULT_MAX_REWORK_CYCLES = 3;
export const DEFAULT_DAG_MAX_CONCURRENCY = 2;
export const DEFAULT_DAG_WRITE_CONCURRENCY = 1;
/** Token budget for a `/dag` run when neither the caller nor `runTokenBudget` provides one. */
export const DEFAULT_DAG_TOKEN_BUDGET = 200_000;

export type ReviewOutcome = 'approved' | 'rework' | 'failed';

/**
 * Rework accounting for a rejected review or a failed test. `rejected` increments
 * `reworkCycles`; past the cap the todo is `failed`, otherwise it returns to the queue.
 */
export function applyReviewVerdict(todo: Todo, verdict: 'approved' | 'rejected', cap = DEFAULT_MAX_REWORK_CYCLES): ReviewOutcome {
  if (verdict === 'approved') return 'approved';
  todo.reworkCycles++;
  if (todo.reworkCycles > cap) {
    todo.status = 'failed';
    return 'failed';
  }
  todo.status = 'pending';
  return 'rework';
}

export interface MergeConflict {
  path: string;
  /** Done todos whose non-empty diffs both touch the file. */
  todos: string[];
}

export interface MergeResult {
  verified: boolean;
  combinedDiff: string;
  conflicts: MergeConflict[];
  artifacts: string[];
  reason: string;
}

const DIFF_HEADER_RE = /^--- a\/(.+?) \((?:new |deleted )?\+\d+ -\d+\)$/gm;

/** File paths with a non-empty diff inside one todo's unified diff text. */
export function diffPaths(diff: string): string[] {
  const paths = new Set<string>();
  for (const match of diff.matchAll(DIFF_HEADER_RE)) {
    const path = match[1]!.trim().replace(/\\/g, '/');
    if (path) paths.add(path);
  }
  return [...paths];
}

/**
 * Merge is validate/aggregate, never apply: the workspace already contains every done todo's
 * changes (children write directly). Conflicts arise when two done todos changed the same
 * file with non-empty diffs. An empty combined diff with no artifacts is `verified: false`
 * ("no changes") unless `allowEmpty`. `verify` runs on the combined diff/workspace.
 */
export async function merge(
  plan: Plan,
  diffs: Record<string, string>,
  opts: {
    allowEmpty?: boolean;
    verify?: (combinedDiff: string, artifacts: string[]) => Promise<boolean> | boolean;
  } = {},
): Promise<MergeResult> {
  const done = plan.todos.filter(todo => todo.status === 'done');
  const combinedParts: string[] = [];
  const filesByTodo = new Map<string, string[]>();
  const artifacts: string[] = [];
  for (const todo of done) {
    const diff = diffs[todo.id] ?? '';
    if (!diff.trim()) continue;
    combinedParts.push(diff);
    const paths = diffPaths(diff);
    filesByTodo.set(todo.id, paths);
    for (const path of paths) {
      if (!artifacts.includes(path)) artifacts.push(path);
    }
  }
  const combinedDiff = combinedParts.join('\n');
  const conflicts: MergeConflict[] = [];
  for (let i = 0; i < done.length; i++) {
    for (let j = i + 1; j < done.length; j++) {
      const a = filesByTodo.get(done[i]!.id) ?? [];
      const b = new Set(filesByTodo.get(done[j]!.id) ?? []);
      for (const path of a) {
        if (!b.has(path)) continue;
        conflicts.push({ path, todos: [done[i]!.id, done[j]!.id] });
      }
    }
  }
  if (conflicts.length) {
    return { verified: false, combinedDiff, conflicts, artifacts, reason: `merge conflict: ${conflicts[0]!.path}` };
  }
  if (!combinedDiff.trim() && artifacts.length === 0) {
    if (opts.allowEmpty) return { verified: true, combinedDiff, conflicts, artifacts, reason: 'empty (allowed)' };
    return { verified: false, combinedDiff, conflicts, artifacts, reason: 'no changes' };
  }
  if (opts.verify) {
    const ok = await opts.verify(combinedDiff, artifacts);
    if (!ok) return { verified: false, combinedDiff, conflicts, artifacts, reason: 'verification failed' };
  }
  return { verified: true, combinedDiff, conflicts, artifacts, reason: 'ok' };
}

/** v0.6 DAG events (additive members of `HarnessEvent`). */
export type DagEvent =
  | { type: 'dag_plan_invalid'; message: string }
  | { type: 'dag_plan_ready'; planId: string; todos: number }
  | { type: 'dag_todo_update'; todoId: string; status: TodoStatus; role: AgentRole; attempt?: number; reason?: string }
  | { type: 'dag_merge'; verified: boolean; conflicts: number; reason?: string };

export interface DagConfig {
  runTokenBudget?: number;
  maxCostUsd?: number;
  maxTimeMs?: number;
  multiAgentReworkCap?: number;
  multiAgentMaxConcurrency?: number;
}

export interface DagSpawnRequest {
  todo: Todo;
  readOnly: boolean;
  allowedTools: string[];
  budget: RunBudget;
  signal: AbortSignal;
}

export interface DagHooks {
  /** Returns raw planner output (parsed JSON); `runDag` validates shape + DAG structure. */
  planner: (goal: string, signal: AbortSignal) => Promise<unknown>;
  /** Runs one todo via the public spawn path (e.g. `Subagents.runTasks`). Must not shell out. */
  spawn: (request: DagSpawnRequest) => Promise<string>;
  /** Optional per-todo review after spawn. Non-'approved' verdicts trigger rework. */
  review?: (todo: Todo, report: string, signal: AbortSignal) => Promise<'approved' | 'rejected'> | 'approved' | 'rejected';
  /**
   * Optional per-todo test for todos that changed files. Prefer `runVerification` with an
   * approve callback; a hook that shells out must approval-gate every command itself.
   */
  test?: (todo: Todo, files: string[], signal: AbortSignal) => Promise<boolean> | boolean;
  /** Final verification over the merged workspace. Prefer `runVerification` + approve callback. */
  verify?: (files: string[], combinedDiff: string, signal: AbortSignal) => Promise<boolean> | boolean;
}

export interface DagOptions {
  config: DagConfig;
  signal: AbortSignal;
  /** Parent workspace file registry: per-todo snapshots and rollbacks. */
  files: WorkspaceFiles;
  /** Shared run budget; every spawn receives the same object. */
  budget?: RunBudget;
  allowedMcpTools?: readonly string[];
  allowEmpty?: boolean;
  writeConcurrency?: number;
  emit?: (event: DagEvent) => void;
}

export interface DagRunResult {
  plan: Plan;
  merge: MergeResult;
  reports: Record<string, string>;
  summary: string;
}

/** Per-todo unified diff from the parent-side turn backups (before content vs current file). */
export async function diffFromBackups(files: WorkspaceFiles, backups: readonly FileBackup[]): Promise<string> {
  const parts: string[] = [];
  for (const backup of backups) {
    let after = '';
    try {
      after = await files.text(await files.path(backup.path));
    } catch {
      after = ''; // deleted or unreadable: diff as a deletion, matching restoreBackups semantics
    }
    const before = backup.existed ? backup.content ?? '' : '';
    const diff = computeUnifiedDiff(backup.path, before, after);
    if (!diff.startsWith('No changes in ')) parts.push(diff);
  }
  return parts.join('\n');
}

/**
 * Executes a validated plan as a DAG: planner → scheduler dispatch (write roles serial) →
 * spawn → review/test with rollback + rework cap → merge → verify. Verify failure, merge
 * conflict, "no changes", plan failure and cancellation all roll back every per-todo backup
 * (reverse order) before throwing.
 */
export async function runDag(goal: string, hooks: DagHooks, opts: DagOptions): Promise<DagRunResult> {
  const emit = (event: DagEvent) => opts.emit?.(event);
  // Internal cancellation channel: when one todo fails hard (or the caller's signal aborts),
  // remaining spawns are aborted through their signal, so no child keeps writing into the
  // workspace after the rollback started. Sibling promises are always drained (allSettled)
  // before the rollback runs.
  const internal = new AbortController();
  const onOuterAbort = () => internal.abort(opts.signal.reason ?? new Error('DAG cancelled.'));
  if (opts.signal.aborted) onOuterAbort();
  else opts.signal.addEventListener('abort', onOuterAbort, { once: true });
  const signal = internal.signal;
  const active = new Set<Promise<void>>();
  // Bound later, once the plan and its backup ledger exist; the catch path uses the same entry.
  let rollbackAllImpl: () => Promise<Array<{ path: string; conflicts: string[] }>> = async () => [];
  const rollbackAll = () => rollbackAllImpl();
  try {
    const cap = opts.config.multiAgentReworkCap ?? DEFAULT_MAX_REWORK_CYCLES;
    const maxConcurrency = opts.config.multiAgentMaxConcurrency ?? DEFAULT_DAG_MAX_CONCURRENCY;
    const writeConcurrency = opts.writeConcurrency ?? DEFAULT_DAG_WRITE_CONCURRENCY;

    // Shared budget: reuse the caller's object, otherwise create one seeded from config. The
    // config cost/time caps apply to both paths (`??=` is a no-op on values already present);
    // costSpent/ledger stay untouched — the caller (CLI) seeds them from the parent session and
    // ledger so a child harness can never clobber the mirror with its own session counter.
    const caps = {
      runTokenBudget: opts.config.runTokenBudget ?? 0,
      maxCostUsd: opts.config.maxCostUsd ?? 0,
      maxTimeMs: opts.config.maxTimeMs ?? 0,
    };
    const budget = opts.budget
      ?? createRunBudget(caps, { spent: 0, limit: caps.runTokenBudget > 0 ? caps.runTokenBudget : DEFAULT_DAG_TOKEN_BUDGET });
    budget.costLimit ??= caps.maxCostUsd;
    budget.timeLimitMs ??= caps.maxTimeMs;
    budget.startedAt ??= Date.now();

    const plan = await (async () => {
      try {
        const raw = await hooks.planner(goal, signal);
        return planFromPlanner(raw, `dag-${Date.now().toString(36)}`);
      } catch (error) {
        const message = error instanceof Error ? error.message : String(error);
        emit({ type: 'dag_plan_invalid', message });
        throw error;
      }
    })();
    try {
      validatePlan(plan);
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      emit({ type: 'dag_plan_invalid', message });
      throw error;
    }
    emit({ type: 'dag_plan_ready', planId: plan.id, todos: plan.todos.length });

    const scheduler = new Scheduler(plan, writeConcurrency, maxConcurrency);
    const reports: Record<string, string> = {};
    const diffs: Record<string, string> = {};
    const failures: Record<string, string> = {};
    const todoBackups: Array<{ todo: Todo; backups: FileBackup[] }> = [];
    let rolledBack = false;

    rollbackAllImpl = async (): Promise<Array<{ path: string; conflicts: string[] }>> => {
      if (rolledBack) return [];
      rolledBack = true;
      const results: Array<{ path: string; conflicts: string[] }> = [];
      for (let i = todoBackups.length - 1; i >= 0; i--) {
        const entry = todoBackups[i]!;
        if (!entry.backups.length) continue;
        const res = await opts.files.restoreBackups(entry.backups, { force: false });
        results.push({ path: entry.todo.id, conflicts: res.conflicts });
      }
      return results;
    };

    const failTodo = async (todo: Todo, backups: FileBackup[], reason?: string): Promise<void> => {
      if (reason) {
        failures[todo.id] = reason;
        reports[todo.id] = `Task failed: ${reason}`;
      }
      const markFailed = () => {
        // markFailed already propagates blocks to dependents; surface them immediately.
        for (const id of scheduler.markFailed(todo.id)) {
          const blocked = scheduler.todo(id);
          if (blocked) emit({ type: 'dag_todo_update', todoId: id, status: 'blocked', role: blocked.role });
        }
        emit({ type: 'dag_todo_update', todoId: todo.id, status: 'failed', role: todo.role, ...(reason ? { reason: reason.slice(0, 200) } : {}) });
      };
      if (backups.length) {
        const res = await opts.files.restoreBackups(backups, { force: false });
        // The entry is consumed: rollbackAll must not restore the same backups twice (the second
        // pass would only produce phantom afterHash conflicts).
        const index = todoBackups.findIndex(entry => entry.backups === backups);
        if (index >= 0) todoBackups.splice(index, 1);
        if (res.conflicts.length) {
          // A user changed these files meanwhile: never clobber, the todo just fails.
          markFailed();
          return;
        }
      }
      markFailed();
    };

    const runTodo = async (todo: Todo): Promise<void> => {
      emit({ type: 'dag_todo_update', todoId: todo.id, status: 'in_progress', role: todo.role, attempt: todo.reworkCycles + 1 });
      const isWrite = WRITE_ROLES.has(todo.role);
      if (isWrite) opts.files.beginTurn();
      let backups: FileBackup[] = [];
      try {
        const report = await hooks.spawn({
          todo,
          readOnly: !isWrite,
          allowedTools: roleToolset(todo.role, opts.allowedMcpTools),
          budget,
          signal,
        });
        // Only write todos touch the shared turn state: read-only todos run in parallel with a
        // write spawn, and draining the shared buffer here would steal the write todo's snapshot.
        backups = isWrite ? opts.files.takeTurnBackups() : [];
        if (backups.length) todoBackups.push({ todo, backups });
        diffs[todo.id] = await diffFromBackups(opts.files, backups);
        reports[todo.id] = report;
        // A sibling may have failed while this spawn ran: discard this attempt and roll it back.
        signal.throwIfAborted();

        const reworkOrFails: Array<'rejected' | 'failed'> = [];
        if (hooks.review) {
          const verdict = await hooks.review(todo, report, signal);
          if (verdict !== 'approved') reworkOrFails.push('rejected');
        }
        if (!reworkOrFails.length && hooks.test && backups.length > 0) {
          const passed = await hooks.test(todo, backups.map(backup => backup.path), signal);
          if (!passed) reworkOrFails.push('failed');
        }
        if (reworkOrFails.length) {
          const outcome = applyReviewVerdict(todo, 'rejected', cap);
          if (outcome === 'failed') {
            await failTodo(todo, backups, `rework cap (${cap}) exceeded`);
            return;
          }
          const res = await opts.files.restoreBackups(backups, { force: false });
          if (res.conflicts.length) {
            await failTodo(todo, backups, 'rollback conflicted with newer user edits');
            return;
          }
          delete diffs[todo.id];
          delete reports[todo.id];
          scheduler.requeue(todo.id);
          return;
        }
        todo.artifacts = [...new Set([...todo.artifacts, ...backups.map(backup => backup.path)])];
        scheduler.markDone(todo.id);
        emit({ type: 'dag_todo_update', todoId: todo.id, status: 'done', role: todo.role, attempt: todo.reworkCycles + 1 });
      } catch (error) {
        // Spawn, review or test hook failure: consume the snapshot, fail the todo and surface
        // the cause (redacted, bounded) in the plan-failure detail instead of swallowing it.
        await failTodo(todo, backups, safeError(error));
      }
    };

    for (;;) {
      signal.throwIfAborted();
      for (const id of scheduler.propagateBlocks()) {
        const todo = scheduler.todo(id)!;
        emit({ type: 'dag_todo_update', todoId: id, status: 'blocked', role: todo.role });
      }
      const dispatched = scheduler.dispatchReady();
      for (const todo of dispatched) {
        const p = runTodo(todo).finally(() => active.delete(p));
        active.add(p);
      }
      if (!active.size) {
        const deadlockIds = scheduler.checkDeadlock();
        if (deadlockIds.length) {
          for (const id of deadlockIds) {
            const todo = scheduler.todo(id)!;
            todo.status = 'blocked';
            emit({ type: 'dag_todo_update', todoId: id, status: 'blocked', role: todo.role });
          }
          throw new PlanDeadlockError(deadlockIds);
        }
        break;
      }
      await Promise.all(active);
    }

    const unfinished = plan.todos.filter(todo => todo.status !== 'done');
    if (unfinished.length) {
      await rollbackAll();
      const detail = unfinished
        .map(todo => `${todo.id}(${todo.status}${failures[todo.id] ? `: ${failures[todo.id]}` : ''})`)
        .join(', ');
      throw new Error(`Plan failed: ${detail}. All todo changes were rolled back.`);
    }

    const mergeResult = await merge(plan, diffs, {
      allowEmpty: opts.allowEmpty,
      ...(hooks.verify ? { verify: (combined: string, artifacts: string[]) => hooks.verify!(artifacts, combined, signal) } : {}),
    });
    emit({ type: 'dag_merge', verified: mergeResult.verified, conflicts: mergeResult.conflicts.length, ...(mergeResult.reason ? { reason: mergeResult.reason } : {}) });
    if (!mergeResult.verified) {
      await rollbackAll();
      throw new ErrNotVerified(mergeResult.reason, mergeResult.conflicts);
    }
    const done = plan.todos.filter(todo => todo.status === 'done');
    const summary = [
      `DAG ${plan.id}: ${done.length}/${plan.todos.length} todo(s) done.`,
      ...done.map(todo => `  [${todo.role}] ${todo.id}: ${todo.title} — artifacts: ${todo.artifacts.join(', ') || '(none)'}`),
      `Merge: ${mergeResult.reason}${mergeResult.artifacts.length ? ` (${mergeResult.artifacts.length} file(s))` : ''}`,
    ].join('\n');
    return { plan, merge: mergeResult, reports, summary };
  } catch (error) {
    // Stop remaining spawns/children, drain every sibling (their failTodo paths restore their
    // own snapshots and their rejections are consumed here), then roll back what is left.
    internal.abort(new Error('DAG failed; cancelling remaining todos.'));
    await Promise.allSettled(active);
    await rollbackAll();
    throw error;
  } finally {
    opts.signal.removeEventListener('abort', onOuterAbort);
  }
}

// ---------------------------------------------------------------------------
// Scope loop (v0.7, P22)
// ---------------------------------------------------------------------------

/** Hard cap for scope-loop rounds: `min(maxRepairRounds + 1, 4)` (§8.2). */
export const SCOPE_ROUND_CAP = 4;

export function scopeRoundCap(maxRepairRounds: number): number {
  const rounds = Number.isFinite(maxRepairRounds) ? Math.trunc(maxRepairRounds) : 0;
  return Math.max(1, Math.min(rounds + 1, SCOPE_ROUND_CAP));
}

/** v0.7 P22 scope-loop events (additive members of `HarnessEvent`). */
export type ScopeEvent = { type: 'scope_round'; round: number; action: 'expand' | 'contract' | 'stop'; reason: string };

/** Shared-budget stop check: token limit first, then the ledger's cost/time caps. */
export function budgetStopReason(budget: RunBudget | undefined): string | undefined {
  if (!budget) return undefined;
  if (budget.limit > 0 && budget.spent >= budget.limit) {
    return `Run token budget reached (${budget.spent} of ${budget.limit} tokens).`;
  }
  const ledger = budget.ledger instanceof CostLedger ? budget.ledger : undefined;
  if (!ledger) return undefined;
  const cap = ledger.hardCapExceeded(budget);
  return cap.exceeded ? cap.reason : undefined;
}

/** Bounded context handed to each scope-loop attempt. */
export interface ScopeLoopAttemptContext {
  round: number;
  goal: string;
  /** Redacted failure output of the previous verification (empty on round 1). */
  verifyOutput: string;
  /** Files the loop has seen change or flagged so far. */
  relatedFiles: string[];
  /** Display form of the commands the failed verification ran. */
  commands: string[];
}

export interface ScopeLoopHooks {
  /**
   * One repair attempt. All writes must go through the `WorkspaceFiles` passed in options so the
   * loop can snapshot and roll them back; the hook must not shell out. A model attempt still
   * accounts its usage into the shared budget so the next round's cap check sees it.
   */
  attempt: (context: ScopeLoopAttemptContext, signal: AbortSignal) => Promise<string>;
}

export interface ScopeLoopOptions {
  cwd: string;
  verifyConfig: VerifyConfig;
  verifyPolicy?: VerifyPolicy;
  runner?: VerifyRunner;
  /** Workspace file registry: per-attempt snapshots and rollback. */
  files: WorkspaceFiles;
  signal: AbortSignal;
  /** Shared run budget; checked before every attempt so the loop never exceeds it. */
  budget?: RunBudget;
  /**
   * Approves the exact verification commands (`tools.approve`). Fail-closed: a plan with
   * commands and no callback throws `VerificationApprovalError`; AST-only plans still run.
   */
  approve?: (plan: VerifyPlan) => Promise<boolean> | boolean;
  /** Literal secrets redacted from the expanded verify output before it reaches the attempt. */
  secrets?: Iterable<string>;
  emit?: (event: ScopeEvent) => void;
  /** Test override; defaults to `scopeRoundCap(verifyConfig.maxRepairRounds)`. */
  maxRounds?: number;
}

export interface ScopeLoopResult {
  ok: boolean;
  rounds: number;
  reason: string;
  /** Final verification verdict — redacted with `opts.secrets` plus the bearer/key-value patterns. */
  verdict?: VerifyVerdict;
  reports: string[];
  changedFiles: string[];
  rollback?: { restored: string[]; deleted: string[]; conflicts: string[]; pruned: string[] };
}

function pathsFromBackups(backups: readonly FileBackup[]): string[] {
  return [...new Set(backups.map(backup => backup.path))];
}

/**
 * Verify-driven repair loop (§8.2): run one attempt, verify the changed files, expand the
 * attempt context on failure and contract (release the expanded context) on pass. The loop owns
 * the workspace turn window (`beginTurn` per attempt) — run it while the harness is idle, like
 * `/dag`. Stops on pass, no tracked changes, a budget/cost/time cap, exhausted rounds, or
 * cancellation; a terminal failure rolls every accumulated snapshot back per §5.1 (restore
 * without force, conflicting user edits are kept and reported).
 *
 * The loop runs verification fail-closed via `runVerification`: a plan with commands and no
 * `approve` callback throws `VerificationApprovalError` before anything executes. The returned
 * verdict (and the expanded attempt context) are redacted with `opts.secrets` plus the shared
 * bearer/key-value patterns before they cross the module boundary. The caller remains
 * responsible for the per-request context budget (`prepareContextBudget`) — the loop only
 * bounds its own additions (verify output text).
 */
export async function runScopeLoop(goal: string, hooks: ScopeLoopHooks, opts: ScopeLoopOptions): Promise<ScopeLoopResult> {
  const emit = (event: ScopeEvent) => opts.emit?.(event);
  const maxRounds = opts.maxRounds ?? scopeRoundCap(opts.verifyConfig.maxRepairRounds);
  const policy = opts.verifyPolicy ?? opts.verifyConfig.policy;
  const runner = opts.runner ?? createVerifyRunner();
  const secrets = [...(opts.secrets ?? [])];
  const reports: string[] = [];
  const accumulated: FileBackup[] = [];
  let lastVerdict: VerifyVerdict | undefined;
  let lastPlan: VerifyPlan | undefined;
  let changedFiles: string[] = [];

  const rollbackAccumulated = async (): Promise<ScopeLoopResult['rollback']> => {
    if (!accumulated.length) return { restored: [], deleted: [], conflicts: [], pruned: [] };
    // Reverse order: a file touched by several attempts must be restored last-attempt-first, or
    // the earliest snapshot would see a mismatched afterHash and report a phantom conflict while
    // the file ends up in a later attempt's state instead of the pre-loop state.
    const res = await opts.files.restoreBackups([...accumulated].reverse(), { force: false });
    // §5.1: retain conflicting backups in the active window so a later checkpoint can revert them.
    const conflicts = new Set(res.conflicts.map(path => path.replace(/\\/g, '/')));
    opts.files.replaceTurnBackups(accumulated.filter(backup => conflicts.has(backup.path)));
    return res;
  };

  try {
    for (let round = 1; round <= maxRounds; round++) {
      const cap = budgetStopReason(opts.budget);
      if (cap) {
        emit({ type: 'scope_round', round, action: 'stop', reason: cap });
        return {
          ok: false, rounds: round - 1, reason: cap,
          ...(lastVerdict ? { verdict: lastVerdict } : {}),
          reports, changedFiles, rollback: await rollbackAccumulated(),
        };
      }
      opts.files.beginTurn();
      const report = await hooks.attempt({
        round,
        goal,
        verifyOutput: lastVerdict ? redactText(formatVerifyErrors(lastVerdict), secrets) : '',
        relatedFiles: changedFiles,
        commands: lastPlan?.commands.map(command => command.display) ?? [],
      }, opts.signal);
      reports.push(report);
      const backups = opts.files.takeTurnBackups();
      accumulated.push(...backups);
      changedFiles = pathsFromBackups(accumulated);
      if (!changedFiles.length) {
        const reason = 'no tracked file changes to verify';
        emit({ type: 'scope_round', round, action: 'stop', reason });
        return { ok: false, rounds: round, reason, reports, changedFiles, rollback: await rollbackAccumulated() };
      }
      const plan = planVerification(opts.cwd, changedFiles, opts.verifyConfig, policy);
      lastPlan = plan;
      const startedMs = Date.now();
      // Redact before the verdict is stored or returned: VerifyIssue.message carries raw command
      // output, and ScopeLoopResult.verdict crosses the module boundary to the caller — the same
      // rule the harness verify hook applies (harness.ts redacts before emitting/persisting).
      const verdict = redactVerdict(
        await runVerification(opts.cwd, changedFiles, opts.verifyConfig, policy, runner, opts.signal, plan, opts.approve),
        secrets,
      );
      lastVerdict = verdict;
      if (verdict.severity !== 'fail') {
        // Pass: contract — the expanded context is no longer needed.
        emit({ type: 'scope_round', round, action: 'contract', reason: `verification ${verdict.severity} in ${Date.now() - startedMs}ms: ${verdict.reason}` });
        return { ok: true, rounds: round, reason: verdict.reason, verdict, reports, changedFiles };
      }
      if (round === maxRounds) break;
      emit({ type: 'scope_round', round, action: 'expand', reason: `verification failed: ${verdict.reason}` });
    }
    const reason = lastVerdict
      ? `verification still failing after ${maxRounds} round(s): ${lastVerdict.reason}`
      : `scope loop exhausted ${maxRounds} round(s)`;
    emit({ type: 'scope_round', round: maxRounds, action: 'stop', reason });
    return {
      ok: false, rounds: maxRounds, reason,
      ...(lastVerdict ? { verdict: lastVerdict } : {}),
      reports, changedFiles, rollback: await rollbackAccumulated(),
    };
  } catch (error) {
    // Attempt hook failure, cancellation or the fail-closed VerificationApprovalError: undo the
    // loop's own writes (best effort) before surfacing the cause. A failure mid-attempt leaves
    // its snapshot in the active window, so drain it into the accumulated set first.
    accumulated.push(...opts.files.takeTurnBackups());
    await rollbackAccumulated().catch(() => {});
    throw error;
  }
}

// ---------------------------------------------------------------------------
// Multi-candidate generation (v0.7, P23)
// ---------------------------------------------------------------------------

/** A task is attempted at most twice; more candidates only burn shared budget. */
export const MAX_CANDIDATES = 2;
/** Degrade to a single candidate when less than this fraction of the cost/time budget remains. */
export const CANDIDATE_DEGRADE_RATIO = 0.2;

export function candidateCap(requested?: number): number {
  if (requested === undefined || !Number.isFinite(requested)) return MAX_CANDIDATES;
  return Math.max(1, Math.min(Math.trunc(requested), MAX_CANDIDATES));
}

/** v0.7 P23 multi-candidate events (additive members of `HarnessEvent`). */
export type CandidateEvent = { type: 'cost_degrade'; reason: string; candidates: number };

export interface Candidate {
  /** Generation order (0-based); the deterministic rerank tie-break. */
  index: number;
  task: string;
  report: string;
  /** Cost of this attempt in USD (shared-budget mirror delta; 0 when unaccounted). */
  costUsd: number;
  passed: boolean;
  warnings: number;
  reason: string;
  /** Snapshot of the workspace before this candidate wrote (empty when nothing changed). */
  backups: FileBackup[];
}

/**
 * Reflection (§8.3): folds a verification verdict into a rankable candidate. A candidate passes
 * whenever the verdict does not `fail` (warn and skip count as pass, matching §5.1 semantics).
 */
export function reflect(
  candidate: Pick<Candidate, 'index' | 'task' | 'report' | 'costUsd' | 'backups'>,
  verdict: VerifyVerdict,
): Candidate {
  return {
    ...candidate,
    passed: verdict.severity !== 'fail',
    warnings: verdict.warnings.length,
    reason: verdict.reason,
  };
}

/** Verify pass first, then fewer warnings, then lower cost; ties keep generation order. */
export function rerank(candidates: readonly Candidate[]): Candidate[] {
  return [...candidates].sort((a, b) =>
    Number(b.passed) - Number(a.passed) ||
    a.warnings - b.warnings ||
    a.costUsd - b.costUsd ||
    a.index - b.index,
  );
}

/**
 * Cost-degrade check (§8.3): when less than 20% of the cost or time budget remains, run a single
 * candidate at reduced effort instead of the full set. Token budgets are enforced by
 * `budgetStopReason`, not here.
 */
export function costDegrade(budget: RunBudget | undefined, now = Date.now()): { degrade: boolean; reason: string } {
  if (!budget) return { degrade: false, reason: '' };
  const costLimit = budget.costLimit ?? 0;
  if (costLimit > 0) {
    const remaining = costLimit - (budget.costSpent ?? 0);
    if (remaining / costLimit < CANDIDATE_DEGRADE_RATIO) {
      return {
        degrade: true,
        reason: `cost budget nearly exhausted (${formatUsd(Math.max(0, budget.costSpent ?? 0))} of ${formatUsd(costLimit)})`,
      };
    }
  }
  const timeLimit = budget.timeLimitMs ?? 0;
  if (timeLimit > 0 && budget.startedAt) {
    const remainingMs = budget.startedAt + timeLimit - now;
    if (remainingMs / timeLimit < CANDIDATE_DEGRADE_RATIO) {
      return {
        degrade: true,
        reason: `time budget nearly exhausted (${formatDurationMs(Math.max(0, remainingMs))} of ${formatDurationMs(timeLimit)} left)`,
      };
    }
  }
  return { degrade: false, reason: '' };
}

export interface CandidateHooks {
  /**
   * Produces one candidate. Writes must go through the `WorkspaceFiles` passed in options so
   * the runner can isolate and roll back each attempt; `degraded` asks the hook to lower the
   * effort (or switch to a cheaper model) because the budget is nearly exhausted.
   */
  generate: (task: string, index: number, opts: { degraded: boolean; signal: AbortSignal }) => Promise<string>;
  /** Verifies the candidate's changed files; prefer `runVerification` with an approve callback. */
  verify: (files: string[], signal: AbortSignal) => Promise<VerifyVerdict>;
}

export interface CandidateOptions {
  files: WorkspaceFiles;
  signal: AbortSignal;
  /** Shared run budget; checked before every attempt so the runner never exceeds it. */
  budget?: RunBudget;
  /** Requested candidate count; clamped to `MAX_CANDIDATES` (2). */
  candidates?: number;
  emit?: (event: CandidateEvent) => void;
}

export interface CandidateRunResult {
  ok: boolean;
  winner?: Candidate;
  /** All generated candidates in generation order. */
  candidates: Candidate[];
  /** `rerank` order (pass > warnings > cost > generation order). */
  ranked: Candidate[];
  degraded: boolean;
  reason: string;
}

/**
 * Multi-candidate runner (§8.3). Candidates run sequentially on the shared workspace: each one
 * is isolated with `beginTurn`/`takeTurnBackups`, verified, and rolled back when it fails, so a
 * losing attempt can never leak into the next attempt or the final result. The first candidate
 * whose verdict does not fail is kept on disk (generating further candidates on top would break
 * isolation and only burn budget); `rerank` still orders every generated attempt in the result.
 * All candidates fail (or the budget runs out) → every snapshot is restored, `ok: false`.
 */
export async function runCandidates(task: string, hooks: CandidateHooks, opts: CandidateOptions): Promise<CandidateRunResult> {
  const emit = (event: CandidateEvent) => opts.emit?.(event);
  const degrade = costDegrade(opts.budget);
  const count = degrade.degrade ? 1 : candidateCap(opts.candidates);
  if (degrade.degrade) emit({ type: 'cost_degrade', reason: degrade.reason, candidates: 1 });

  const generated: Candidate[] = [];
  let stopReason = '';
  for (let index = 0; index < count; index++) {
    const cap = budgetStopReason(opts.budget);
    if (cap) {
      stopReason = cap;
      break;
    }
    const costBefore = opts.budget?.costSpent ?? 0;
    // Isolation: this candidate is the only writer inside the turn window.
    opts.files.beginTurn();
    let backups: FileBackup[] = [];
    let report = '';
    try {
      report = await hooks.generate(task, index, { degraded: degrade.degrade, signal: opts.signal });
      backups = opts.files.takeTurnBackups();
      const files = pathsFromBackups(backups);
      const verdict = await hooks.verify(files, opts.signal);
      const candidate = reflect(
        { index, task, report, costUsd: Math.max(0, (opts.budget?.costSpent ?? 0) - costBefore), backups },
        verdict,
      );
      generated.push(candidate);
      if (candidate.passed) break; // winner stays on disk; the next attempt would layer on top
      await opts.files.restoreBackups(backups, { force: false });
    } catch (error) {
      // Generation or verification hook failure: undo this attempt's partial writes and record
      // it as a failed candidate; the next one still starts from the same base state. A failure
      // inside `generate` leaves the snapshot in the active window — drain it first.
      if (!backups.length) backups = opts.files.takeTurnBackups();
      if (backups.length) await opts.files.restoreBackups(backups, { force: false }).catch(() => {});
      generated.push({
        index, task, report,
        costUsd: Math.max(0, (opts.budget?.costSpent ?? 0) - costBefore),
        passed: false, warnings: 0, reason: safeError(error), backups,
      });
    }
  }

  const ranked = rerank(generated);
  const winner = generated.find(candidate => candidate.passed);
  if (winner) {
    // Surface the winner's snapshot to the caller's turn window so a later checkpoint can revert it.
    opts.files.mergeBackups(winner.backups);
    return { ok: true, winner, candidates: generated, ranked, degraded: degrade.degrade, reason: winner.reason };
  }
  return {
    ok: false,
    candidates: generated,
    ranked,
    degraded: degrade.degrade,
    reason: stopReason
      || (generated.length
        ? `all ${generated.length} candidate(s) failed verification: ${generated.at(-1)?.reason ?? ''}`
        : 'no candidate generated'),
  };
}
