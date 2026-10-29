import { homedir } from 'node:os';
import { join, resolve } from 'node:path';
import { readFile, mkdir, writeFile, rename, realpath } from 'node:fs/promises';
import { randomUUID } from 'node:crypto';
import { z } from 'zod';
import { protectSecretEnv } from './process.js';
import type { StubCursor } from './stub-provider.js';

export const EFFORT_LEVELS = ['low', 'medium', 'high', 'xhigh', 'max'] as const;
export type EffortLevel = (typeof EFFORT_LEVELS)[number];

/** Per-model prices, in USD per one million tokens. */
export const modelPriceSchema = z.object({
  inputPer1M: z.number().min(0),
  outputPer1M: z.number().min(0),
  cachedInputPer1M: z.number().min(0).optional(),
  cacheWritePer1M: z.number().min(0).optional(),
}).strict();
export type ModelPrice = z.infer<typeof modelPriceSchema>;
/**
 * Flat (`{ "model": price, "default": price }`) and nested (`{ "models": { "model": price } }`)
 * shapes are accepted; cost.ts normalizes both.
 */
export const pricingConfigSchema = z.record(
  z.string(),
  z.union([modelPriceSchema, z.record(z.string(), modelPriceSchema)]),
);

/** Verification policy (v0.4). Defaults are permissive: only failures found by runnable stages fail. */
export const verifyPolicySchema = z.object({
  requireTests: z.boolean().default(false),
  requireLint: z.boolean().default(false),
  requireTypecheck: z.boolean().default(false),
  allowWarnings: z.boolean().default(true),
}).strict();
export type VerifyPolicyConfig = z.infer<typeof verifyPolicySchema>;

export const verifyConfigSchema = z.object({
  enabled: z.boolean().default(true),
  commands: z.array(z.string()).default([]),
  timeoutMs: z.number().int().min(1000).max(600_000).default(120_000),
  maxRepairRounds: z.number().int().min(0).max(10).default(2),
  policy: verifyPolicySchema.default({ requireTests: false, requireLint: false, requireTypecheck: false, allowWarnings: true }),
}).strict();
export type VerifyConfigSchema = z.infer<typeof verifyConfigSchema>;

export const configSchema = z.object({
  provider: z.enum(['openai-compatible', 'openai-responses', 'anthropic']).default('openai-compatible'),
  baseUrl: z.string().url().optional(),
  model: z.string().trim().min(1).optional(),
  effort: z.enum(EFFORT_LEVELS).optional(),
  keyEnv: z.string().regex(/^[A-Za-z_][A-Za-z0-9_]*$/).default('GOON_API_KEY'),
  contextWindow: z.number().int().min(8192).max(2_000_000).default(128_000),
  maxOutputTokens: z.number().int().min(128).max(131_072).default(16_384),
  timeoutMs: z.number().int().min(1000).max(1_800_000).default(300_000),
  runTokenBudget: z.number().int().min(0).default(0),
  maxCostUsd: z.number().min(0).default(0),
  maxTimeMs: z.number().int().min(0).default(0),
  pricing: pricingConfigSchema.optional(),
  inlineReasoning: z.enum(['leading-think', 'off']).default('leading-think'),
  showReasoning: z.boolean().default(false),
  approvalMode: z.enum(['ask', 'auto', 'plan', 'readonly']).optional(),
  // Execution policy (v0.5, P12). Network access is denied unless explicitly allowed.
  allowNet: z.boolean().default(false),
  allowNetHosts: z.array(z.string()).default([]),
  viewMode: z.enum(['normal', 'verbose', 'transcript']).optional(),
  verify: verifyConfigSchema.default({ enabled: true, commands: [], timeoutMs: 120_000, maxRepairRounds: 2,
    policy: { requireTests: false, requireLint: false, requireTypecheck: false, allowWarnings: true } }),
  eventLog: z.boolean().default(false),
  stub: z.boolean().default(false),
  stubFixture: z.string().optional(),
  // Memory & lexical RAG (v0.6). Both opt-in by default: indexing must not change startup
  // latency or behavior on Termux until it is benchmarked.
  memoryEnabled: z.boolean().default(false),
  ragEnabled: z.boolean().default(false),
  ragTopK: z.number().int().min(1).max(100).default(10),
  ragCapacity: z.number().int().min(10).max(100_000).default(5000),
  ragThreshold: z.number().min(0).max(1).default(0),
  ragIndexMaxFiles: z.number().int().min(1).max(50_000).default(2000),
  ragIndexMaxFileBytes: z.number().int().min(1024).max(10_485_760).default(1_048_576),
  // Context budget splits (v0.6, P18). Of the input budget (contextWindow - maxOutputTokens):
  // a reserve for provider overhead, a cap/floor pair for the system prompt and the remainder
  // split between conversation and injected files/RAG context.
  contextReserveRatio: z.number().min(0).max(0.5).default(0.15),
  contextConversationRatio: z.number().min(0).max(1).default(0.45),
  contextFilesRatio: z.number().min(0).max(1).default(0.25),
  // Multi-agent DAG (v0.6, P20/P21). "off" rejects /dag entirely; MCP tools are never exposed
  // to DAG roles unless listed here ("mcp__server__tool" or "mcp__*" wildcards).
  multiAgentMode: z.enum(['off', 'manual', 'auto']).default('manual'),
  multiAgentReworkCap: z.number().int().min(1).max(10).default(3),
  multiAgentMaxConcurrency: z.number().int().min(1).max(8).default(2),
  multiAgentMcpTools: z.array(z.string()).default([]),
  // Real embedder seam (v0.7, P24). "hash" keeps the deterministic offline embedder (default);
  // "openai"/"ollama" route embeddings over HTTP and require "embedderModel".
  embedderProvider: z.enum(['hash', 'openai', 'ollama']).default('hash'),
  embedderModel: z.string().trim().min(1).max(200).optional(),
  embedderBaseUrl: z.string().url().optional(),
}).strict();
export type Config = z.infer<typeof configSchema>;
export interface RuntimeConfig extends Config { cwd: string; apiKey: string; dataDir: string; baseUrl: string; model: string; stubCursor?: StubCursor }
export const configDir = () => join(process.env.XDG_CONFIG_HOME || join(homedir(), '.config'), 'goon');
export const configPath = () => join(configDir(), 'config.json');
export const credentialsPath = () => join(configDir(), 'credentials.json');
export const dataDir = () => join(process.env.XDG_DATA_HOME || join(homedir(), '.local', 'share'), 'goon');

const credentialsSchema = z.record(z.string(), z.string());

export async function loadCredentials(): Promise<Record<string, string>> {
  try {
    const raw = JSON.parse(await readFile(credentialsPath(), 'utf8'));
    return credentialsSchema.parse(raw);
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === 'ENOENT') return {};
    return {};
  }
}

export async function saveCredential(keyEnv: string, apiKey: string): Promise<void> {
  await mkdir(configDir(), { recursive: true, mode: 0o700 });
  const existing = await loadCredentials();
  const trimmed = apiKey.trim();
  if (trimmed) existing[keyEnv] = trimmed;
  await atomicJson(credentialsPath(), existing);
}

export async function resolveApiKey(config: Config): Promise<string | undefined> {
  protectSecretEnv(config.keyEnv);
  const fromEnv = process.env[config.keyEnv]?.trim();
  if (fromEnv) return fromEnv;
  const stored = await loadCredentials();
  const fromStore = stored[config.keyEnv]?.trim();
  return fromStore || undefined;
}

export const DEFAULT_GATEWAY_URL = 'https://api.yuristore.xyz/v1';

export function normalizeGatewayUrl(rawUrl: string): string {
  const trimmed = rawUrl.trim().replace(/\/+$/, '');
  try {
    const parsed = new URL(trimmed);
    const isKnownGatewayRoot =
      parsed.hostname === 'api.yuristore.xyz' ||
      parsed.host === '127.0.0.1:8000' ||
      parsed.host === 'localhost:8000';
    if ((parsed.pathname === '/' || parsed.pathname === '') && isKnownGatewayRoot) {
      return `${trimmed}/v1`;
    }
  } catch {
    // Keep original if URL parsing fails; loadConfig validates URL format
  }
  return trimmed;
}

export async function loadConfig(overrides: Record<string, unknown> = {}): Promise<Config> {
  let stored: unknown = {};
  try { stored = JSON.parse(await readFile(configPath(), 'utf8')); }
  catch (error) { if ((error as NodeJS.ErrnoException).code !== 'ENOENT') throw error; }
  const validated = configSchema.parse(stored);
  const env: Record<string, unknown> = {};
  for (const [key, name] of Object.entries({
    provider: 'GOON_PROVIDER', baseUrl: 'GOON_BASE_URL', model: 'GOON_MODEL', effort: 'GOON_EFFORT',
    keyEnv: 'GOON_KEY_ENV', contextWindow: 'GOON_CONTEXT_WINDOW', maxOutputTokens: 'GOON_MAX_OUTPUT_TOKENS',
    timeoutMs: 'GOON_TIMEOUT_MS', runTokenBudget: 'GOON_RUN_TOKEN_BUDGET',
    maxCostUsd: 'GOON_MAX_COST_USD', maxTimeMs: 'GOON_MAX_TIME_MS',
    inlineReasoning: 'GOON_INLINE_REASONING',
    embedderProvider: 'GOON_EMBEDDER_PROVIDER',
    embedderModel: 'GOON_EMBEDDER_MODEL',
    embedderBaseUrl: 'GOON_EMBEDDER_BASE_URL',
  })) {
    if (process.env[name] !== undefined) env[key] = ['contextWindow', 'maxOutputTokens', 'timeoutMs', 'runTokenBudget', 'maxCostUsd', 'maxTimeMs'].includes(key)
      ? Number(process.env[name]) : process.env[name];
  }
  if (process.env.GOON_PRICING_JSON !== undefined) {
    try { env.pricing = JSON.parse(process.env.GOON_PRICING_JSON); }
    catch { throw new Error('GOON_PRICING_JSON is not valid JSON.'); }
  }
  const envVerify: Record<string, unknown> = {};
  const boolEnv = (name: string): boolean | undefined => {
    const raw = process.env[name];
    if (raw === undefined) return undefined;
    const value = raw.trim().toLowerCase();
    if (['1', 'true', 'yes', 'on'].includes(value)) return true;
    if (['0', 'false', 'no', 'off', ''].includes(value)) return false;
    throw new Error(`${name} must be a boolean (true/false).`);
  };
  const enabledEnv = boolEnv('GOON_VERIFY_ENABLED');
  if (enabledEnv !== undefined) envVerify.enabled = enabledEnv;
  if (process.env.GOON_VERIFY_TIMEOUT_MS !== undefined) {
    const value = Number(process.env.GOON_VERIFY_TIMEOUT_MS);
    if (!Number.isFinite(value)) throw new Error('GOON_VERIFY_TIMEOUT_MS must be a number.');
    envVerify.timeoutMs = value;
  }
  if (process.env.GOON_VERIFY_MAX_REPAIR_ROUNDS !== undefined) {
    const value = Number(process.env.GOON_VERIFY_MAX_REPAIR_ROUNDS);
    if (!Number.isFinite(value)) throw new Error('GOON_VERIFY_MAX_REPAIR_ROUNDS must be a number.');
    envVerify.maxRepairRounds = value;
  }
  if (process.env.GOON_VERIFY_CMDS !== undefined) {
    const raw = process.env.GOON_VERIFY_CMDS.trim();
    if (!raw) envVerify.commands = [];
    else if (raw.startsWith('[')) {
      try {
        const parsed = JSON.parse(raw);
        if (!Array.isArray(parsed) || parsed.some(item => typeof item !== 'string')) {
          throw new Error('not a string array');
        }
        envVerify.commands = parsed;
      } catch { throw new Error('GOON_VERIFY_CMDS must be a JSON array of strings or a comma-separated list.'); }
    } else {
      envVerify.commands = raw.split(',').map(item => item.trim()).filter(Boolean);
    }
  }
  const verifyPolicyEnv: Record<string, unknown> = {};
  for (const [key, name] of Object.entries({
    requireTests: 'GOON_VERIFY_REQUIRE_TESTS',
    requireLint: 'GOON_VERIFY_REQUIRE_LINT',
    requireTypecheck: 'GOON_VERIFY_REQUIRE_TYPECHECK',
    allowWarnings: 'GOON_VERIFY_ALLOW_WARNINGS',
  })) {
    const value = boolEnv(name);
    if (value !== undefined) verifyPolicyEnv[key] = value;
  }
  if (Object.keys(verifyPolicyEnv).length) envVerify.policy = verifyPolicyEnv;
  const allowNetEnv = boolEnv('GOON_ALLOW_NET');
  if (allowNetEnv !== undefined) env.allowNet = allowNetEnv;
  if (process.env.GOON_ALLOW_NET_HOSTS !== undefined) {
    env.allowNetHosts = process.env.GOON_ALLOW_NET_HOSTS.split(',').map(item => item.trim()).filter(Boolean);
  }
  const eventLogEnv = boolEnv('GOON_EVENT_LOG');
  if (eventLogEnv !== undefined) env.eventLog = eventLogEnv;
  const stubEnv = boolEnv('GOON_STUB');
  if (stubEnv !== undefined) env.stub = stubEnv;
  if (process.env.GOON_STUB_FIXTURE !== undefined) env.stubFixture = process.env.GOON_STUB_FIXTURE;
  const memoryEnabledEnv = boolEnv('GOON_MEMORY_ENABLED');
  if (memoryEnabledEnv !== undefined) env.memoryEnabled = memoryEnabledEnv;
  const ragEnabledEnv = boolEnv('GOON_RAG_ENABLED');
  if (ragEnabledEnv !== undefined) env.ragEnabled = ragEnabledEnv;
  for (const [key, name] of Object.entries({ ragTopK: 'GOON_RAG_TOP_K', ragCapacity: 'GOON_RAG_CAPACITY' })) {
    if (process.env[name] !== undefined) {
      const value = Number(process.env[name]);
      if (!Number.isInteger(value)) throw new Error(`${name} must be an integer.`);
      env[key] = value;
    }
  }
  const merged = { ...validated, ...env, ...overrides };
  // `verify` is nested: merge per key so one env var or one flag never drops the other fields.
  const overrideVerify = overrides.verify && typeof overrides.verify === 'object' ? overrides.verify as Record<string, unknown> : undefined;
  const storedVerify = (validated as { verify?: Record<string, unknown> }).verify ?? {};
  const overridePolicy = overrideVerify?.policy && typeof overrideVerify.policy === 'object'
    ? overrideVerify.policy as Record<string, unknown> : undefined;
  merged.verify = {
    ...storedVerify,
    ...envVerify,
    ...(overrideVerify),
    policy: {
      ...(storedVerify.policy as Record<string, unknown> | undefined),
      ...(envVerify.policy as Record<string, unknown> | undefined),
      ...overridePolicy,
    },
  } as Config['verify'];
  if (env.runTokenBudget === undefined && overrides.runTokenBudget === undefined) {
    merged.runTokenBudget = 0;
  }
  const explicitMaxOut =
    (stored && typeof stored === 'object' && 'maxOutputTokens' in stored) ||
    env.maxOutputTokens !== undefined ||
    overrides.maxOutputTokens !== undefined;
  if (!explicitMaxOut && typeof merged.contextWindow === 'number' && merged.contextWindow > 0 && merged.maxOutputTokens >= merged.contextWindow / 2) {
    merged.maxOutputTokens = Math.max(128, Math.floor(merged.contextWindow / 4));
  }
  const config = configSchema.parse(merged);
  if (config.maxOutputTokens >= config.contextWindow / 2) throw new Error('maxOutputTokens must be less than half contextWindow.');
  if (config.baseUrl) {
    const url = new URL(config.baseUrl);
    if (!['http:', 'https:'].includes(url.protocol) || url.username || url.password || url.search || url.hash) {
      throw new Error('baseUrl must be HTTP(S), without credentials, query or fragment.');
    }
    config.baseUrl = normalizeGatewayUrl(config.baseUrl);
  }
  return config;
}

// One deterministic script cursor per process/workspace/fixture. Keying by a stable string (not
// the Config object identity) keeps turn order across /model, /effort and /setup config reloads.
const stubCursors = new Map<string, StubCursor>();

export async function resolveRuntime(config: Config, cwd: string): Promise<RuntimeConfig> {
  protectSecretEnv(config.keyEnv);
  if (config.stub) {
    const resolvedCwd = await realpath(resolve(cwd));
    const key = `${process.platform === 'win32' ? resolvedCwd.toLowerCase() : resolvedCwd}\0${config.stubFixture ?? ''}`;
    let cursor = stubCursors.get(key);
    if (!cursor) {
      cursor = { index: 0 };
      stubCursors.set(key, cursor);
    }
    return {
      ...config,
      model: config.model ?? 'stub-model',
      baseUrl: 'stub://local',
      // The stub provider never performs network I/O, so no credential is read or required.
      apiKey: '',
      cwd: resolvedCwd,
      dataDir: dataDir(),
      stubCursor: cursor,
    };
  }
  if (!config.model) throw new Error('Missing model. Run "goon setup" or "goon init --model MODEL --base-url URL".');
  const apiKey = await resolveApiKey(config);
  if (!apiKey) throw new Error(`Missing ${config.keyEnv}. Run "goon setup" or set ${config.keyEnv} in your shell.`);
  const baseUrl = config.baseUrl ? normalizeGatewayUrl(config.baseUrl) : DEFAULT_GATEWAY_URL;
  return { ...config, model: config.model, baseUrl, apiKey, cwd: await realpath(resolve(cwd)), dataDir: dataDir() };
}

export async function atomicJson(file: string, value: unknown, options: { compact?: boolean } = {}): Promise<void> {
  const temporary = `${file}.${randomUUID()}.tmp`;
  // Session files are machine-written and rewritten on every tool step: compact JSON is ~30%
  // smaller and faster to serialize. Human-edited config files stay pretty-printed.
  const body = options.compact ? JSON.stringify(value) : JSON.stringify(value, null, 2);
  await writeFile(temporary, `${body}\n`, { mode: 0o600, flag: 'wx' });
  try { await rename(temporary, file); }
  catch (error) { const { unlink } = await import('node:fs/promises'); await unlink(temporary).catch(() => {}); throw error; }
}

export async function saveConfig(config: Config): Promise<void> {
  await mkdir(configDir(), { recursive: true, mode: 0o700 });
  await atomicJson(configPath(), configSchema.parse(config));
}
