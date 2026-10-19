import { createInterface } from 'node:readline/promises';
import { stdin, stderr } from 'node:process';
import {
  type Config,
  type EffortLevel,
  DEFAULT_GATEWAY_URL,
  EFFORT_LEVELS,
  configPath,
  credentialsPath,
  loadConfig,
  normalizeGatewayUrl,
  resolveApiKey,
  saveConfig,
  saveCredential,
} from './config.js';
import {
  discoverModels,
  formatModelBadge,
  resolveModelCapabilities,
  type DiscoveredModel,
} from './provider.js';
import { VERSION } from './version.js';
import { padEndVisible, visibleLength } from './tui.js';

export interface ProviderPreset {
  id: string;
  label: string;
  provider: Config['provider'];
  baseUrl: string;
}

export const PROVIDER_PRESETS: ProviderPreset[] = [
  {
    id: 'anthropic',
    label: 'Anthropic Messages API (YuriStore / Claude / Anthropic-compatible)',
    provider: 'anthropic',
    baseUrl: DEFAULT_GATEWAY_URL,
  },
  {
    id: 'openai-responses',
    label: 'OpenAI Responses API (YuriStore / Codex / Responses endpoint)',
    provider: 'openai-responses',
    baseUrl: DEFAULT_GATEWAY_URL,
  },
  {
    id: 'openai-compatible',
    label: 'YuriStore AI Gateway (https://api.yuristore.xyz/v1 · glm-5.3 / deepseek-v4.1 / space-bunny / beam)',
    provider: 'openai-compatible',
    baseUrl: DEFAULT_GATEWAY_URL,
  },
  {
    id: 'openrouter',
    label: 'OpenRouter Gateway',
    provider: 'openai-compatible',
    baseUrl: 'https://openrouter.ai/api/v1',
  },
  {
    id: 'deepseek',
    label: 'DeepSeek Official API',
    provider: 'openai-compatible',
    baseUrl: 'https://api.deepseek.com/v1',
  },
  {
    id: 'groq',
    label: 'Groq Cloud API',
    provider: 'openai-compatible',
    baseUrl: 'https://api.groq.com/openai/v1',
  },
  {
    id: 'local-proxy',
    label: 'Local AI Gateway (http://127.0.0.1:8000/v1)',
    provider: 'openai-compatible',
    baseUrl: 'http://127.0.0.1:8000/v1',
  },
];

export const EFFORT_DESCRIPTIONS: Record<EffortLevel, string> = {
  low: 'Fast responses with lighter reasoning',
  medium: 'Balanced speed and reasoning depth for everyday tasks',
  high: 'Deep reasoning for complex engineering and debugging',
  xhigh: 'Extra-high reasoning depth for hard architectural problems',
  max: 'Maximum reasoning compute budget and exhaustive verification',
};

export interface SetupIO {
  write?: (text: string) => void;
  ask?: (prompt: string) => Promise<string>;
  discover?: (baseUrl: string, apiKey?: string, provider?: Config['provider']) => Promise<DiscoveredModel[]>;
}

export function parseEffortInput(raw: string, fallback?: EffortLevel): EffortLevel | undefined {
  const clean = raw.trim().toLowerCase();
  if (!clean) return fallback;
  if (clean === 'default' || clean === 'off' || clean === 'none' || clean === '0' || clean === '1') return undefined;
  const idx = Number(clean) - 2;
  if (Number.isInteger(idx) && idx >= 0 && idx < EFFORT_LEVELS.length) {
    return EFFORT_LEVELS[idx];
  }
  if ((EFFORT_LEVELS as readonly string[]).includes(clean)) {
    return clean as EffortLevel;
  }
  return fallback;
}

export async function runSetupWizard(initialConfig?: Config, io?: SetupIO): Promise<Config> {
  const current = initialConfig ?? await loadConfig();
  const existingKey = await resolveApiKey(current);
  let ownRl: ReturnType<typeof createInterface> | undefined;
  const write = io?.write ?? ((text: string) => stderr.write(text));
  const ask = io?.ask ?? (async (prompt: string) => {
    if (!ownRl) ownRl = createInterface({ input: stdin, output: stderr, terminal: Boolean(stdin.isTTY) });
    return (await ownRl.question(prompt)).trim();
  });
  const discover = io?.discover ?? ((url, key, prov) => discoverModels(url, key, prov));

  try {
    const setupTitle = `╭─── ✻ goon cli v${VERSION} · Interactive Setup `;
    const setupTopDashes = '─'.repeat(Math.max(1, 70 - visibleLength(setupTitle)));
    write(
      `\n${setupTitle}${setupTopDashes}╮\n` +
      `│  Configure provider, endpoint, API key, model & reasoning effort    │\n` +
      `╰─────────────────────────────────────────────────────────────────────╯\n\n` +
      `Step 1/5 · Choose AI Provider Dialect / Endpoint Preset:\n`
    );

    const defaultIndex = Math.max(
      0,
      PROVIDER_PRESETS.findIndex(p => p.provider === current.provider && (!current.baseUrl || p.baseUrl === current.baseUrl))
    );
    for (const [idx, preset] of PROVIDER_PRESETS.entries()) {
      const marker = idx === defaultIndex ? '●' : ' ';
      write(`  ${marker} [${idx + 1}] ${preset.label}\n        (${preset.provider} · ${preset.baseUrl})\n`);
    }

    const choiceRaw = (await ask(`\nSelect provider [1-${PROVIDER_PRESETS.length}] (default ${defaultIndex + 1}): `)).trim();
    const chosenIdx = choiceRaw ? Number(choiceRaw) - 1 : defaultIndex;
    const preset = PROVIDER_PRESETS[chosenIdx] ?? PROVIDER_PRESETS[defaultIndex];

    const defaultUrl = current.baseUrl && chosenIdx === defaultIndex ? current.baseUrl : preset.baseUrl;
    write(`\nStep 2/5 · API Base URL (${preset.provider}):\n`);
    const urlInput = (await ask(`Base URL [${defaultUrl}]: `)).trim();
    const baseUrl = normalizeGatewayUrl(urlInput || defaultUrl);

    const keyHint = existingKey
      ? `saved (${existingKey.slice(0, 4)}...${existingKey.slice(-2)}) — press Enter to keep`
      : `stored in ${credentialsPath()}`;
    write(`\nStep 3/5 · API Key (${current.keyEnv}, ${keyHint}):\n`);
    const keyInput = (await ask(`API Key: `)).trim();
    // Persisted only after the final step so cancelling the wizard leaves credentials untouched.
    const activeKey = keyInput || existingKey;

    write(`\nStep 4/5 · Model ID (probing ${baseUrl}/models ...):\n`);
    const discovered = await discover(baseUrl, activeKey, preset.provider);
    if (discovered.length > 0) {
      const shown = discovered.slice(0, 30);
      for (const [idx, m] of shown.entries()) {
        write(`    [${idx + 1}] ${m.id}${formatModelBadge(m)}\n`);
      }
      if (discovered.length > shown.length) {
        write(`    ... and ${discovered.length - shown.length} more models available on endpoint\n`);
      }
    } else {
      write(`  (Endpoint did not return a /models list — enter your exact model ID below)\n`);
    }

    let model = current.model ?? '';
    while (!model) {
      const promptHint = discovered.length > 0
        ? `Enter model ID or number [1-${Math.min(30, discovered.length)}]${current.model ? ` (default: ${current.model})` : ''}: `
        : `Enter exact model ID${current.model ? ` (default: ${current.model})` : ''}: `;
      const modelInput = (await ask(promptHint)).trim();
      const modelIdx = Number(modelInput) - 1;
      if (modelInput && Number.isInteger(modelIdx) && modelIdx >= 0 && modelIdx < Math.min(30, discovered.length)) {
        model = discovered[modelIdx].id;
      } else {
        model = modelInput || current.model || '';
      }
      if (!model) write(`  ! Model ID is required (no models are hardcoded).\n`);
    }

    const meta = resolveModelCapabilities(model, discovered.find(d => d.id === model));
    const nextContextWindow = meta.contextWindow ?? current.contextWindow;
    const rawMaxOut = meta.maxOutputTokens ?? current.maxOutputTokens;
    const nextMaxOutputTokens = Math.min(131_072, Math.min(rawMaxOut, Math.floor(nextContextWindow / 2) - 1));

    write(`\nStep 5/5 · Reasoning Effort:\n`);
    const currentEffort = current.effort ?? meta.defaultEffort;
    write(`  ${!currentEffort ? '●' : ' '} [1] default  — Use provider/model default reasoning behavior\n`);
    for (const [idx, level] of EFFORT_LEVELS.entries()) {
      const marker = currentEffort === level ? '●' : ' ';
      write(`  ${marker} [${idx + 2}] ${level.padEnd(8)} — ${EFFORT_DESCRIPTIONS[level]}\n`);
    }
    const effortRaw = (await ask(`Select effort [1-${EFFORT_LEVELS.length + 1}] or name (default: ${currentEffort ?? 'default'}): `)).trim();
    const effort = parseEffortInput(effortRaw, currentEffort);

    const updated = await loadConfig({
      provider: preset.provider,
      baseUrl,
      model,
      effort,
      contextWindow: nextContextWindow,
      maxOutputTokens: nextMaxOutputTokens,
      keyEnv: current.keyEnv,
    });
    if (keyInput) await saveCredential(current.keyEnv, keyInput);
    await saveConfig(updated);

    const finalKey = await resolveApiKey(updated);
    write(
      `\n╭─── ✓ Setup Saved ───────────────────────────────────────────────────╮\n` +
      `│  Provider : ${padEndVisible(updated.provider, 54)}│\n` +
      `│  Base URL : ${padEndVisible(updated.baseUrl ?? 'default', 54)}│\n` +
      `│  Model    : ${padEndVisible(updated.model ?? '', 54)}│\n` +
      `│  Effort   : ${padEndVisible(updated.effort ?? 'default', 54)}│\n` +
      `│  Context  : ${padEndVisible(`${updated.contextWindow} tokens (out: ${updated.maxOutputTokens})`, 54)}│\n` +
      `│  API Key  : ${padEndVisible(finalKey ? 'Configured ✓' : `Missing (set ${updated.keyEnv})`, 54)}│\n` +
      `│  Config   : ${padEndVisible(configPath(), 54)}│\n` +
      `╰─────────────────────────────────────────────────────────────────────╯\n\n`
    );
    return updated;
  } finally {
    ownRl?.close();
  }
}

