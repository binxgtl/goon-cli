import { setDefaultResultOrder } from 'node:dns';
import { randomUUID } from 'node:crypto';
import { createOpenAICompatible } from '@ai-sdk/openai-compatible';
import { createOpenAI } from '@ai-sdk/openai';
import { createAnthropic } from '@ai-sdk/anthropic';
import type { LanguageModel } from 'ai';
import { EFFORT_LEVELS, normalizeGatewayUrl, type Config, type EffortLevel, type RuntimeConfig } from './config.js';

try {
  setDefaultResultOrder('ipv4first');
} catch {
  // Ignore if unsupported in restricted runtime
}

export type ProviderOptionsMap = Record<string, Record<string, string>>;

export function buildProviderOptions(config: Pick<Config, 'provider' | 'effort'>): ProviderOptionsMap | undefined {
  if (!config.effort) return undefined;
  const effort: EffortLevel = config.effort;
  if (config.provider === 'anthropic') {
    return { anthropic: { effort } };
  }
  if (config.provider === 'openai-responses') {
    // @ai-sdk/openai schema validates up to 'xhigh'; normalize 'max' -> 'xhigh'
    const normalized = effort === 'max' ? 'xhigh' : effort;
    return { openai: { reasoningEffort: normalized } };
  }
  return {
    goon: { reasoningEffort: effort },
    openaiCompatible: { reasoningEffort: effort },
  };
}

function isCompactionPayload(parsed: Record<string, unknown>): boolean {
  if (parsed.tools) return false;
  const raw = JSON.stringify(parsed.messages ?? parsed.system ?? parsed.input ?? '');
  return raw.includes('Summarize coding session history');
}

function mergeSessionHeaders(initHeaders: RequestInit['headers'], sessionId?: string): Record<string, string> {
  const merged: Record<string, string> = {};
  if (initHeaders instanceof Headers) {
    initHeaders.forEach((value, key) => { merged[key] = value; });
  } else if (Array.isArray(initHeaders)) {
    for (const [key, value] of initHeaders) merged[key] = value;
  } else if (initHeaders && typeof initHeaders === 'object') {
    Object.assign(merged, initHeaders as Record<string, string>);
  }
  merged['x-request-id'] = randomUUID();
  if (sessionId) {
    merged['x-session-id'] = sessionId;
    merged['x-opencode-session'] = sessionId;
  }
  return merged;
}

export function createEffortFetch(
  provider: Config['provider'],
  effort?: EffortLevel,
  sessionId?: string,
): typeof fetch | undefined {
  if (!effort && !sessionId) return undefined;
  return async (input, init) => {
    const headers = mergeSessionHeaders(init?.headers, sessionId);
    if (effort && init?.body && typeof init.body === 'string') {
      try {
        const parsed = JSON.parse(init.body) as Record<string, unknown>;
        const effectiveEffort: EffortLevel = isCompactionPayload(parsed) ? 'low' : effort;
        if (provider === 'anthropic') {
          const existing = parsed.output_config && typeof parsed.output_config === 'object'
            ? (parsed.output_config as Record<string, unknown>)
            : {};
          parsed.output_config = { ...existing, effort: effectiveEffort };
        } else if (provider === 'openai-responses') {
          const existing = parsed.reasoning && typeof parsed.reasoning === 'object'
            ? (parsed.reasoning as Record<string, unknown>)
            : {};
          parsed.reasoning = { ...existing, effort: effectiveEffort };
        } else {
          parsed.reasoning_effort = effectiveEffort;
        }
        return fetch(input, { ...init, headers, body: JSON.stringify(parsed) });
      } catch {
        // Not a JSON request body; pass through with session headers
      }
    }
    return fetch(input, { ...init, headers });
  };
}

export function createModel(config: RuntimeConfig, sessionId?: string): LanguageModel {
  const customFetch = createEffortFetch(config.provider, config.effort, sessionId);
  const baseHeaders: Record<string, string> = {
    ...(sessionId ? { 'x-session-id': sessionId, 'x-opencode-session': sessionId } : {}),
    ...(config.provider === 'anthropic' && config.apiKey ? { Authorization: `Bearer ${config.apiKey}` } : {}),
  };
  const options = {
    apiKey: config.apiKey,
    baseURL: config.baseUrl,
    ...(Object.keys(baseHeaders).length > 0 ? { headers: baseHeaders } : {}),
    ...(customFetch ? { fetch: customFetch } : {}),
  };
  if (config.provider === 'anthropic') return createAnthropic(options)(config.model!);
  if (config.provider === 'openai-responses') return createOpenAI(options).responses(config.model!);
  return createOpenAICompatible({ ...options, name: 'goon', includeUsage: true }).chatModel(config.model!);
}

export interface DiscoveredModel {
  id: string;
  contextWindow?: number;
  maxOutputTokens?: number;
  supportsVision?: boolean;
  defaultEffort?: EffortLevel;
}

export const KNOWN_GATEWAY_MODELS: Record<string, Omit<DiscoveredModel, 'id'>> = {
  'glm-5.3-flash': {
    contextWindow: 200_000,
    maxOutputTokens: 32_000,
    supportsVision: true,
    defaultEffort: 'max',
  },
  'deepseek-v4.1-flash': {
    contextWindow: 200_000,
    maxOutputTokens: 32_000,
    supportsVision: true,
    defaultEffort: 'max',
  },
  'space-bunny': {
    contextWindow: 1_048_576,
    maxOutputTokens: 65_536,
    supportsVision: true,
    defaultEffort: 'max',
  },
  beam: {
    contextWindow: 1_048_576,
    maxOutputTokens: 65_536,
    supportsVision: false,
    defaultEffort: 'max',
  },
};

export function resolveModelCapabilities(modelId: string, discovered?: DiscoveredModel): DiscoveredModel {
  const cleanId = modelId.trim();
  const known = KNOWN_GATEWAY_MODELS[cleanId];
  const contextWindow = discovered?.contextWindow && known?.contextWindow
    ? Math.max(discovered.contextWindow, known.contextWindow)
    : (discovered?.contextWindow ?? known?.contextWindow);
  const maxOutputTokens = discovered?.maxOutputTokens && known?.maxOutputTokens
    ? Math.max(discovered.maxOutputTokens, known.maxOutputTokens)
    : (discovered?.maxOutputTokens ?? known?.maxOutputTokens);
  const supportsVision = discovered?.supportsVision ?? known?.supportsVision;
  const defaultEffort = discovered?.defaultEffort ?? known?.defaultEffort;
  return {
    id: cleanId,
    ...(contextWindow !== undefined ? { contextWindow } : {}),
    ...(maxOutputTokens !== undefined ? { maxOutputTokens } : {}),
    ...(supportsVision !== undefined ? { supportsVision } : {}),
    ...(defaultEffort !== undefined ? { defaultEffort } : {}),
  };
}

function formatTokenCount(n: number): string {
  if (n % 1024 === 0) return `${n / 1024}k`;
  if (n % 1000 === 0) return `${n / 1000}k`;
  return `${Math.round(n / 1024)}k`;
}

export function formatModelBadge(m: DiscoveredModel): string {
  const parts: string[] = [];
  if (m.contextWindow) parts.push(`ctx: ${formatTokenCount(m.contextWindow)}`);
  if (m.maxOutputTokens) parts.push(`out: ${formatTokenCount(m.maxOutputTokens)}`);
  if (m.supportsVision === true) parts.push('vision');
  else if (m.supportsVision === false) parts.push('text-only');
  if (m.defaultEffort) parts.push(`effort: ${m.defaultEffort}`);
  return parts.length ? ` (${parts.join(' · ')})` : '';
}

function positiveInt(...candidates: unknown[]): number | undefined {
  for (const val of candidates) {
    if (typeof val === 'number' && Number.isInteger(val) && val > 0) return val;
  }
  return undefined;
}

export async function discoverModels(
  baseUrl: string,
  apiKey?: string,
  provider: Config['provider'] = 'openai-compatible',
  signal?: AbortSignal,
): Promise<DiscoveredModel[]> {
  try {
    const cleanUrl = normalizeGatewayUrl(baseUrl);
    const headers: Record<string, string> = { Accept: 'application/json' };
    if (apiKey) {
      headers.Authorization = `Bearer ${apiKey}`;
      if (provider === 'anthropic') {
        headers['x-api-key'] = apiKey;
        headers['anthropic-version'] = '2023-06-01';
      }
    }
    const response = await fetch(`${cleanUrl}/models`, {
      method: 'GET',
      headers,
      redirect: 'error',
      signal: AbortSignal.any([AbortSignal.timeout(4000), ...(signal ? [signal] : [])]),
    });
    if (!response.ok) return [];
    const text = await response.text();
    if (text.length > 4 * 1024 * 1024) return [];
    const parsed = JSON.parse(text) as unknown;
    const items: unknown[] = Array.isArray(parsed)
      ? parsed
      : parsed && typeof parsed === 'object' && Array.isArray((parsed as { data?: unknown[] }).data)
        ? (parsed as { data: unknown[] }).data
        : parsed && typeof parsed === 'object' && Array.isArray((parsed as { models?: unknown[] }).models)
          ? (parsed as { models: unknown[] }).models
          : [];
    const seen = new Set<string>();
    const results: DiscoveredModel[] = [];
    for (const raw of items) {
      if (!raw || typeof raw !== 'object') continue;
      const rec = raw as Record<string, unknown>;
      const id = typeof rec.id === 'string' ? rec.id.trim() : typeof rec.name === 'string' ? rec.name.trim() : '';
      if (!id || seen.has(id)) continue;
      seen.add(id);
      const known = KNOWN_GATEWAY_MODELS[id];
      const rawCtx = positiveInt(
        rec.supports_1m_context === true ? 1_048_576 : undefined,
        rec.context_window,
        rec.context_length,
        rec.max_context_length,
        rec.max_input_tokens,
        rec.inputTokenLimit,
        known?.contextWindow,
      );
      const contextWindow = rawCtx && known?.contextWindow ? Math.max(rawCtx, known.contextWindow) : rawCtx;
      const rawOut = positiveInt(
        rec.max_output_tokens,
        rec.max_completion_tokens,
        rec.max_tokens,
        rec.outputTokenLimit,
        known?.maxOutputTokens,
      );
      const maxOutputTokens = rawOut && known?.maxOutputTokens ? Math.max(rawOut, known.maxOutputTokens) : rawOut;
      const supportsVision = typeof rec.supports_vision === 'boolean'
        ? rec.supports_vision
        : known?.supportsVision;
      const rawEffort = typeof rec.reasoning_effort === 'string' ? rec.reasoning_effort.trim().toLowerCase() : undefined;
      const defaultEffort = rawEffort && (EFFORT_LEVELS as readonly string[]).includes(rawEffort)
        ? (rawEffort as EffortLevel)
        : known?.defaultEffort;
      results.push({
        id,
        ...(contextWindow ? { contextWindow } : {}),
        ...(maxOutputTokens ? { maxOutputTokens } : {}),
        ...(supportsVision !== undefined ? { supportsVision } : {}),
        ...(defaultEffort !== undefined ? { defaultEffort } : {}),
      });
    }
    return results;
  } catch {
    return [];
  }
}

