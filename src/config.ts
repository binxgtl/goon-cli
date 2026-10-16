import { homedir } from 'node:os';
import { join, resolve } from 'node:path';
import { readFile, mkdir, writeFile, rename, realpath } from 'node:fs/promises';
import { randomUUID } from 'node:crypto';
import { z } from 'zod';

export const configSchema = z.object({
  provider: z.enum(['openai-compatible', 'openai-responses', 'anthropic']).default('openai-compatible'),
  baseUrl: z.string().url().optional(),
  model: z.string().trim().min(1).optional(),
  keyEnv: z.string().regex(/^[A-Za-z_][A-Za-z0-9_]*$/).default('GOON_API_KEY'),
  contextWindow: z.number().int().min(8192).max(2_000_000).default(128_000),
  maxOutputTokens: z.number().int().min(128).max(128_000).default(8192),
  timeoutMs: z.number().int().min(1000).max(1_800_000).default(300_000),
  runTokenBudget: z.number().int().min(0).default(100_000),
}).strict();
export type Config = z.infer<typeof configSchema>;
export interface RuntimeConfig extends Config { cwd: string; apiKey: string; dataDir: string; baseUrl: string; model: string }
export const configDir = () => join(process.env.XDG_CONFIG_HOME || join(homedir(), '.config'), 'goon');
export const configPath = () => join(configDir(), 'config.json');
export const dataDir = () => join(process.env.XDG_DATA_HOME || join(homedir(), '.local', 'share'), 'goon');

export async function loadConfig(overrides: Record<string, unknown> = {}): Promise<Config> {
  let stored: unknown = {};
  try { stored = JSON.parse(await readFile(configPath(), 'utf8')); }
  catch (error) { if ((error as NodeJS.ErrnoException).code !== 'ENOENT') throw error; }
  const validated = configSchema.parse(stored);
  const env: Record<string, unknown> = {};
  for (const [key, name] of Object.entries({
    provider: 'GOON_PROVIDER', baseUrl: 'GOON_BASE_URL', model: 'GOON_MODEL', keyEnv: 'GOON_KEY_ENV',
    contextWindow: 'GOON_CONTEXT_WINDOW', maxOutputTokens: 'GOON_MAX_OUTPUT_TOKENS',
    timeoutMs: 'GOON_TIMEOUT_MS', runTokenBudget: 'GOON_RUN_TOKEN_BUDGET',
  })) {
    if (process.env[name] !== undefined) env[key] = ['contextWindow', 'maxOutputTokens', 'timeoutMs', 'runTokenBudget'].includes(key)
      ? Number(process.env[name]) : process.env[name];
  }
  const config = configSchema.parse({ ...validated, ...env, ...overrides });
  if (config.maxOutputTokens >= config.contextWindow / 2) throw new Error('maxOutputTokens must be less than half contextWindow.');
  if (config.baseUrl) {
    const url = new URL(config.baseUrl);
    if (!['http:', 'https:'].includes(url.protocol) || url.username || url.password || url.search || url.hash) {
      throw new Error('baseUrl must be HTTP(S), without credentials, query or fragment.');
    }
  }
  return config;
}

export async function resolveRuntime(config: Config, cwd: string): Promise<RuntimeConfig> {
  if (!config.model) throw new Error('Missing model. Run goon init --model MODEL --base-url URL.');
  const apiKey = process.env[config.keyEnv];
  if (!apiKey) throw new Error(`Missing ${config.keyEnv}. Set it in your shell; no account login is needed.`);
  const baseUrl = config.baseUrl ?? (config.provider === 'anthropic' ? 'https://api.anthropic.com/v1' : 'https://api.openai.com/v1');
  return { ...config, model: config.model, baseUrl, apiKey, cwd: await realpath(resolve(cwd)), dataDir: dataDir() };
}

export async function atomicJson(file: string, value: unknown): Promise<void> {
  const temporary = `${file}.${randomUUID()}.tmp`;
  await writeFile(temporary, `${JSON.stringify(value, null, 2)}\n`, { mode: 0o600, flag: 'wx' });
  try { await rename(temporary, file); }
  catch (error) { const { unlink } = await import('node:fs/promises'); await unlink(temporary).catch(() => {}); throw error; }
}

export async function saveConfig(config: Config): Promise<void> {
  await mkdir(configDir(), { recursive: true, mode: 0o700 });
  await atomicJson(configPath(), configSchema.parse(config));
}
