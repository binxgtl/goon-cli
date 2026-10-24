/**
 * Run cost accounting (v0.4, P05).
 *
 * Design:
 * - `CostLedger.total` is canonical for one ledger (a run, or a parent + its children sharing it).
 * - `session.costUsd` is canonical for a session and survives resume.
 * - `budget.costSpent` is a mirror of the shared ledger's total, never of a child session.
 * - The ledger fails closed: constructing it with `maxCostUsd > 0` and no price for the model
 *   throws `CostPricingError` before any provider request is made.
 *
 * This module type-imports `RunBudget` only, so the tool registry can import it for value types
 * without a runtime cycle.
 */
import type { Config, ModelPrice } from './config.js';
import type { RunBudget } from './tools.js';

export type PricingConfig = Config['pricing'];

export interface CostPricingTable {
  default?: ModelPrice;
  models: Record<string, ModelPrice>;
}

/** Token usage split used by the ledger and emitted in usage events. */
export interface UsageSplit {
  inputTokens: number;
  outputTokens: number;
  totalTokens: number;
  cachedInputTokens?: number;
  cacheWriteTokens?: number;
}

/** Shape accepted from the AI SDK and from context helpers (all fields optional). */
export interface RawUsage {
  inputTokens?: number | null;
  outputTokens?: number | null;
  totalTokens?: number | null;
  cachedInputTokens?: number | null;
  inputTokenDetails?: {
    cacheReadTokens?: number | null;
    cacheWriteTokens?: number | null;
  } | null;
}

export class CostPricingError extends Error {
  constructor(readonly model: string, readonly configuredModels: string[]) {
    super(
      `No price configured for model "${model}" while maxCostUsd > 0. ` +
      `Add pricing["${model}"] (or pricing.default) to the goon config. ` +
      `Configured models: ${configuredModels.length ? configuredModels.join(', ') : 'none'}.`,
    );
    this.name = 'CostPricingError';
  }
}

function isModelPrice(value: unknown): value is ModelPrice {
  if (!value || typeof value !== 'object' || Array.isArray(value)) return false;
  const candidate = value as Record<string, unknown>;
  return typeof candidate.inputPer1M === 'number' && typeof candidate.outputPer1M === 'number';
}

/**
 * Accepts both the flat shape (`{ "model": {...}, "default": {...} }`) and the nested shape
 * (`{ "models": { "model": {...} }, "default": {...} }`). The config schema validates both.
 */
export function normalizePricing(pricing: PricingConfig | undefined): CostPricingTable {
  const table: CostPricingTable = { models: {} };
  if (!pricing || typeof pricing !== 'object') return table;
  for (const [key, value] of Object.entries(pricing)) {
    if (key === 'models' && value && typeof value === 'object' && !isModelPrice(value)) {
      for (const [model, price] of Object.entries(value as Record<string, ModelPrice>)) {
        if (isModelPrice(price)) table.models[model] = price;
      }
      continue;
    }
    if (key === 'default' && isModelPrice(value)) {
      table.default = value;
      continue;
    }
    if (isModelPrice(value)) table.models[key] = value;
  }
  return table;
}

export function isFiniteTokenCount(value: unknown): value is number {
  return typeof value === 'number' && Number.isFinite(value) && value >= 0;
}

/** Normalizes an AI SDK usage object, falling back to local estimates when the provider omits usage. */
export function toUsageSplit(usage: RawUsage | undefined, fallbackInput = 0, fallbackOutput = 0): UsageSplit {
  const input = isFiniteTokenCount(usage?.inputTokens) ? usage.inputTokens : Math.max(0, fallbackInput);
  const output = isFiniteTokenCount(usage?.outputTokens) ? usage.outputTokens : Math.max(0, fallbackOutput);
  const total = isFiniteTokenCount(usage?.totalTokens) ? usage.totalTokens : input + output;
  const cached = isFiniteTokenCount(usage?.inputTokenDetails?.cacheReadTokens)
    ? usage.inputTokenDetails.cacheReadTokens
    : isFiniteTokenCount(usage?.cachedInputTokens)
      ? usage.cachedInputTokens
      : 0;
  const cacheWrite = isFiniteTokenCount(usage?.inputTokenDetails?.cacheWriteTokens)
    ? usage.inputTokenDetails.cacheWriteTokens
    : 0;
  return {
    inputTokens: input,
    outputTokens: output,
    totalTokens: total,
    ...(cached > 0 ? { cachedInputTokens: cached } : {}),
    ...(cacheWrite > 0 ? { cacheWriteTokens: cacheWrite } : {}),
  };
}

export function formatUsd(value: number): string {
  const safe = Number.isFinite(value) && value > 0 ? value : 0;
  return `$${safe.toFixed(4)}`;
}

export function formatDurationMs(ms: number): string {
  const safe = Number.isFinite(ms) && ms > 0 ? ms : 0;
  if (safe < 1000) return `${Math.round(safe)}ms`;
  if (safe < 60_000) return `${(safe / 1000).toFixed(1)}s`;
  const minutes = Math.floor(safe / 60_000);
  const seconds = Math.round((safe % 60_000) / 1000);
  return `${minutes}m ${String(seconds).padStart(2, '0')}s`;
}

export interface CapCheck {
  exceeded: boolean;
  reason: string;
}

/**
 * Prices are per one million tokens. Cached/cache-write rates fall back to the input rate when
 * omitted, so a missing optional rate can never make a cached token free by accident.
 */
export class CostLedger {
  /** Cost accumulated by this ledger. Canonical when no session store exists (unit tests). */
  total = 0;

  constructor(readonly pricing: CostPricingTable, readonly model?: string) {}

  static fromConfig(
    config: Pick<Config, 'model' | 'maxCostUsd' | 'pricing'>,
    model: string | undefined = config.model,
  ): CostLedger {
    const ledger = new CostLedger(normalizePricing(config.pricing), model);
    const cap = config.maxCostUsd ?? 0;
    if (cap > 0 && model && !ledger.priceFor(model)) {
      throw new CostPricingError(model, ledger.configuredModels());
    }
    return ledger;
  }

  configuredModels(): string[] {
    return Object.keys(this.pricing.models);
  }

  priceFor(model: string | undefined = this.model): ModelPrice | undefined {
    if (!model) return this.pricing.default;
    return this.pricing.models[model] ?? this.pricing.default;
  }

  computeCost(usage: UsageSplit, model: string | undefined = this.model): number {
    const price = this.priceFor(model);
    if (!price) return 0;
    const input = Math.max(0, usage.inputTokens);
    const output = Math.max(0, usage.outputTokens);
    const cached = Math.min(input, Math.max(0, usage.cachedInputTokens ?? 0));
    const cacheWrite = Math.min(Math.max(0, input - cached), Math.max(0, usage.cacheWriteTokens ?? 0));
    const uncached = Math.max(0, input - cached - cacheWrite);
    const inputRate = price.inputPer1M;
    const cachedRate = price.cachedInputPer1M ?? inputRate;
    const cacheWriteRate = price.cacheWritePer1M ?? inputRate;
    const micro = uncached * inputRate + cached * cachedRate + cacheWrite * cacheWriteRate + output * price.outputPer1M;
    return Math.max(0, micro) / 1_000_000;
  }

  /**
   * Initializes a run budget and links it to this ledger unless it already shares another ledger
   * (child harnesses must never rebind the parent's ledger). Seeds the ledger total from the
   * budget mirror so resumed sessions keep counting from the persisted session cost.
   */
  registerRun(budget: RunBudget, startedAt = Date.now()): CostLedger {
    budget.costSpent ??= 0;
    budget.costLimit ??= 0;
    budget.startedAt ??= startedAt;
    budget.timeLimitMs ??= 0;
    const ledger = budget.ledger instanceof CostLedger ? budget.ledger : this;
    budget.ledger = ledger;
    if (ledger.total < budget.costSpent) ledger.total = budget.costSpent;
    return ledger;
  }

  /**
   * Adds one request's usage to the ledger returned by `registerRun` (or this ledger) and mirrors
   * the shared total into `budget.costSpent`. Returns the cost of this request (the delta).
   */
  account(
    budget: RunBudget | undefined,
    usage: UsageSplit,
    model: string | undefined = this.model,
    onCost?: (cost: number) => void,
  ): number {
    const ledger = budget?.ledger instanceof CostLedger ? budget.ledger : this;
    const cost = ledger.computeCost(usage, model);
    ledger.total += cost;
    if (budget) budget.costSpent = Math.max(budget.costSpent ?? 0, ledger.total);
    onCost?.(cost);
    return cost;
  }

  hardCapExceeded(budget: RunBudget): CapCheck {
    const costLimit = budget.costLimit ?? 0;
    const costSpent = budget.costSpent ?? 0;
    if (costLimit > 0 && costSpent >= costLimit) {
      return { exceeded: true, reason: `Run cost budget reached (${formatUsd(costSpent)} of ${formatUsd(costLimit)}).` };
    }
    const timeLimit = budget.timeLimitMs ?? 0;
    if (timeLimit > 0 && budget.startedAt) {
      const elapsed = Date.now() - budget.startedAt;
      if (elapsed >= timeLimit) {
        return {
          exceeded: true,
          reason: `Run time budget reached (${formatDurationMs(elapsed)} of ${formatDurationMs(timeLimit)}).`,
        };
      }
    }
    return { exceeded: false, reason: '' };
  }

  describe(budget?: RunBudget, sessionCostUsd = 0): string {
    const parts = [`Session cost ${formatUsd(sessionCostUsd)}`];
    if (budget) {
      const runCost = budget.costSpent ?? 0;
      const costLimit = budget.costLimit ?? 0;
      parts.push(`run ${formatUsd(runCost)}${costLimit > 0 ? ` / ${formatUsd(costLimit)}` : ''}`);
      if ((budget.timeLimitMs ?? 0) > 0 && budget.startedAt) {
        parts.push(`time ${formatDurationMs(Date.now() - budget.startedAt)} / ${formatDurationMs(budget.timeLimitMs ?? 0)}`);
      }
    }
    return parts.join(' · ');
  }

  /** Returns true when cost enforcement cannot work (missing or zero price). */
  warnIfInert(model: string | undefined = this.model, write: (text: string) => void = text => process.stderr.write(`${text}\n`)): boolean {
    const price = this.priceFor(model);
    if (!price) {
      write(`goon: no price configured for model "${model ?? '(unknown)'}"; token caps still apply but cost tracking is inactive.`);
      return true;
    }
    if (price.inputPer1M === 0 && price.outputPer1M === 0) {
      write(`goon: pricing for model "${model}" is zero; maxCostUsd cannot stop a run.`);
      return true;
    }
    return false;
  }
}

/** Creates or reuses a run budget, filling cost/time caps from config without resetting a shared one. */
export function createRunBudget(
  config: Pick<Config, 'runTokenBudget' | 'maxCostUsd' | 'maxTimeMs'>,
  shared?: RunBudget,
): RunBudget {
  const budget = shared ?? { spent: 0, limit: config.runTokenBudget };
  budget.costLimit ??= config.maxCostUsd ?? 0;
  budget.timeLimitMs ??= config.maxTimeMs ?? 0;
  budget.startedAt ??= Date.now();
  return budget;
}
