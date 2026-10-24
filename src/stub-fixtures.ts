import { readFileSync, statSync } from 'node:fs';
import { resolve } from 'node:path';
import { z } from 'zod';

export const stubUsageSchema = z.object({
  inputTokens: z.number().int().nonnegative().optional(),
  outputTokens: z.number().int().nonnegative().optional(),
  totalTokens: z.number().int().nonnegative().optional(),
  cacheReadTokens: z.number().int().nonnegative().optional(),
  cacheWriteTokens: z.number().int().nonnegative().optional(),
}).strict();

export const stubToolCallSchema = z.object({
  id: z.string().min(1),
  name: z.string().min(1),
  input: z.unknown().optional(),
}).strict();

export const stubTurnSchema = z.object({
  match: z.string().optional(),
  reasoning: z.array(z.string()).optional(),
  text: z.array(z.string()).optional(),
  toolCalls: z.array(stubToolCallSchema).optional(),
  usage: stubUsageSchema.optional(),
  error: z.object({
    message: z.string().min(1),
    statusCode: z.number().int().min(100).max(599).optional(),
    retryable: z.boolean().optional(),
  }).optional(),
}).strict();

export const stubFixtureSchema = z.object({
  v: z.literal(1),
  model: z.string().min(1).optional(),
  input: z.string().optional(),
  turns: z.array(stubTurnSchema),
  auxiliary: stubTurnSchema.optional(),
}).strict();

export type StubUsage = z.infer<typeof stubUsageSchema>;
export type StubToolCall = z.infer<typeof stubToolCallSchema>;
export type StubTurn = z.infer<typeof stubTurnSchema>;
export type StubFixture = z.infer<typeof stubFixtureSchema>;

export const DEFAULT_STUB_FIXTURE: StubFixture = {
  v: 1,
  model: 'stub-model',
  turns: [
    {
      text: ['[stub] deterministic reply'],
      usage: { inputTokens: 10, outputTokens: 5, totalTokens: 15 },
    },
  ],
};

export function loadStubFixture(path?: string): StubFixture {
  if (!path) return DEFAULT_STUB_FIXTURE;
  const absolute = resolve(path);
  let info;
  try { info = statSync(absolute); } catch (error) {
    throw new Error(`Stub fixture not found: ${absolute} (${error instanceof Error ? error.message : String(error)}).`);
  }
  if (!info.isFile()) throw new Error(`Stub fixture is not a file: ${absolute}.`);
  if (info.size > 1024 * 1024) {
    throw new Error(`Stub fixture exceeds 1 MiB: ${absolute} (${info.size} bytes).`);
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(readFileSync(absolute, 'utf8'));
  } catch (error) {
    throw new Error(`Stub fixture is not valid JSON: ${absolute} (${error instanceof Error ? error.message : String(error)}).`);
  }
  return stubFixtureSchema.parse(parsed);
}
