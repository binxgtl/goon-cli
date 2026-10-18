import { createOpenAICompatible } from '@ai-sdk/openai-compatible';
import { createOpenAI } from '@ai-sdk/openai';
import { createAnthropic } from '@ai-sdk/anthropic';
import type { LanguageModel } from 'ai';
import type { Config, EffortLevel, RuntimeConfig } from './config.js';

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

export function createEffortFetch(provider: Config['provider'], effort?: EffortLevel): typeof fetch | undefined {
  if (!effort) return undefined;
  return async (input, init) => {
    if (init?.body && typeof init.body === 'string') {
      try {
        const parsed = JSON.parse(init.body) as Record<string, unknown>;
        if (provider === 'anthropic') {
          const existing = parsed.output_config && typeof parsed.output_config === 'object'
            ? (parsed.output_config as Record<string, unknown>)
            : {};
          parsed.output_config = { ...existing, effort };
        } else if (provider === 'openai-responses') {
          const existing = parsed.reasoning && typeof parsed.reasoning === 'object'
            ? (parsed.reasoning as Record<string, unknown>)
            : {};
          parsed.reasoning = { ...existing, effort };
        } else {
          parsed.reasoning_effort = effort;
        }
        return fetch(input, { ...init, body: JSON.stringify(parsed) });
      } catch {
        // Not a JSON request body; pass through unchanged
      }
    }
    return fetch(input, init);
  };
}

export function createModel(config: RuntimeConfig): LanguageModel {
  const customFetch = createEffortFetch(config.provider, config.effort);
  const options = {
    apiKey: config.apiKey,
    baseURL: config.baseUrl,
    ...(customFetch ? { fetch: customFetch } : {}),
  };
  if (config.provider === 'anthropic') return createAnthropic(options)(config.model!);
  if (config.provider === 'openai-responses') return createOpenAI(options).responses(config.model!);
  return createOpenAICompatible({ ...options, name: 'goon' }).chatModel(config.model!);
}

export interface DiscoveredModel {
  id: string;
  contextWindow?: number;
  maxOutputTokens?: number;
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
    const cleanUrl = baseUrl.replace(/\/+$/, '');
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
      const contextWindow = positiveInt(
        rec.context_window,
        rec.context_length,
        rec.max_context_length,
        rec.max_input_tokens,
        rec.inputTokenLimit,
      );
      const maxOutputTokens = positiveInt(
        rec.max_output_tokens,
        rec.max_completion_tokens,
        rec.max_tokens,
        rec.outputTokenLimit,
      );
      results.push({
        id,
        ...(contextWindow ? { contextWindow } : {}),
        ...(maxOutputTokens ? { maxOutputTokens } : {}),
      });
    }
    return results;
  } catch {
    return [];
  }
}

