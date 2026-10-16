import { createOpenAICompatible } from '@ai-sdk/openai-compatible';
import { createOpenAI } from '@ai-sdk/openai';
import { createAnthropic } from '@ai-sdk/anthropic';
import type { LanguageModel } from 'ai';
import type { RuntimeConfig } from './config.js';

export function createModel(config: RuntimeConfig): LanguageModel {
  const options = { apiKey: config.apiKey, baseURL: config.baseUrl };
  if (config.provider === 'anthropic') return createAnthropic(options)(config.model!);
  if (config.provider === 'openai-responses') return createOpenAI(options).responses(config.model!);
  return createOpenAICompatible({ ...options, name: 'goon' }).chatModel(config.model!);
}

