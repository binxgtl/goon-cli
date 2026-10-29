// Builds an instrumented copy of the TypeScript build (dist/) whose pure exports record every
// call (arguments and result) into PARITY_OUT. Running the JS unit tests against it harvests a
// corpus the Rust parity test replays, so both implementations are compared input-for-input.
import { cpSync, readFileSync, writeFileSync, rmSync } from 'node:fs';
import { join, resolve } from 'node:path';

const FUNCTIONS = {
  risk: ['riskLevel', 'isGenericRunner', 'tokenize', 'baseName', 'classifyBashRisk', 'classifyMcpRisk'],
  approval: ['bashApprovalPrefix', 'canAutoApprove', 'bashApprovalExact', 'isApprovedBashExact', 'isApprovedBashPrefix'],
  policy: ['normalizeNetworkHost', 'parseNetworkHostList', 'assessCommandNetwork', 'extractNetworkHost', 'isNetworkAllowedForCommand', 'networkDeniedMessage'],
  tools: ['sanitizeBashCommand', 'formatTodos', 'normalizeToolName', 'normalizeToolInput', 'toolApprovalSignature'],
  syntax: ['isJsoncPath', 'validateSyntax'],
  verify: ['clampVerifyTimeout', 'parseConfiguredCommand', 'verifyCommandLine', 'parseIssues', 'formatVerifyErrors', 'formatVerifyWarnings', 'redactText'],
  files: ['replaceFirstLiteral', 'limitTextBytes', 'computeUnifiedDiff', 'parsePatchText', 'applyHunksToContent'],
  redact: ['redactSecrets', 'redactObject', 'isSensitivePath'],
  rag: ['fnv1a', 'tokenize', 'cosineSimilarity', 'chunkFile', 'parseGoonignore', 'isIgnored', 'looksBinary'],
  context: ['extractCandidatePaths', 'buildGoalContinuationPrompt', 'buildGoalBudgetLimitPrompt', 'buildGoalObjectiveUpdatedPrompt', 'buildUserGoalContextMessage', 'estimatedTokens', 'withoutBinaryParts'],
  'context-budget': ['allocateContextBudget', 'truncateSystemToBudget', 'pruneToolResults', 'pruneConversation', 'renderMemoryContext'],
  cost: ['normalizePricing', 'toUsageSplit', 'formatUsd', 'formatDurationMs'],
  session: ['goalStatusLabel', 'formatGoalElapsedSeconds', 'formatGoalTokensCompact', 'formatGoalUsageSummary', 'formatGoalStatusIndicator', 'formatGoalSummaryLines', 'parseGoalDraftArgs', 'sameWorkspace'],
  workflow: ['classifyWorkflowComplexity', 'parseWorkflowArgs', 'shellEscape', 'interpolateWorkflowTemplate', 'parseMarkdownWorkflow'],
  reasoning: ['withReasoningReplay', 'visibleAnswer', 'lastAssistantAnswer'],
  mcp: ['mcpToolName', 'sanitizeAndCompactToolSchema', 'parseCodexMcpServers'],
  eventlog: ['findSeqGap', 'isEventLogPath'],
  tui: ['formatTokenCount', 'terminalText', 'wrapTerminal', 'visibleLength', 'sliceVisible', 'sliceAnsiLine', 'padEndVisible', 'formatInlineMarkdown'],
  provider: ['resolveModelCapabilities', 'formatModelBadge'],
  config: ['normalizeGatewayUrl'],
  coord: ['roleToolset', 'validatePlan', 'diffPaths', 'scopeRoundCap', 'candidateCap', 'rerank'],
};

const RECORDER = `import { appendFileSync } from 'node:fs';
const out = process.env.PARITY_OUT;
function enc(v, seen = new Set()) {
  if (v === undefined) return { $undefined: true };
  if (v === null || typeof v === 'boolean' || typeof v === 'string') return v;
  if (typeof v === 'number') return Number.isFinite(v) ? v : { $number: String(v) };
  if (typeof v === 'bigint' || typeof v === 'function' || typeof v === 'symbol') throw new Error('unserializable');
  if (seen.has(v)) throw new Error('cycle');
  seen.add(v);
  try {
    if (Array.isArray(v)) return v.map(x => enc(x, seen));
    if (v instanceof Set) return { $set: [...v].map(x => enc(x, seen)) };
    if (v instanceof Map) return { $map: [...v].map(([k, x]) => [enc(k, seen), enc(x, seen)]) };
    if (v instanceof Uint8Array) return { $bytes: Buffer.from(v).toString('base64') };
    const proto = Object.getPrototypeOf(v);
    if (proto !== Object.prototype && proto !== null) throw new Error('class instance');
    const o = {};
    for (const [k, x] of Object.entries(v)) o[k] = enc(x, seen);
    return o;
  } finally { seen.delete(v); }
}
export function record(module, name, args, run) {
  let before;
  try { before = enc(args); } catch { return run(); }
  let result, error;
  try { result = run(); } catch (e) { error = e; }
  if (out && !(result && typeof result.then === 'function')) {
    try {
      const entry = { module, fn: name, args: before, after: enc(args) };
      if (error) entry.error = String(error && error.message || error); else entry.result = enc(result);
      appendFileSync(out, JSON.stringify(entry) + '\\n');
    } catch { /* unserializable result */ }
  }
  if (error) throw error;
  return result;
}
`;

const src = resolve(process.argv[2] ?? 'dist');
const dst = resolve(process.argv[3] ?? 'dist-instrumented');
rmSync(dst, { recursive: true, force: true });
cpSync(src, dst, { recursive: true });
writeFileSync(join(dst, '__recorder.js'), RECORDER);
for (const [module, names] of Object.entries(FUNCTIONS)) {
  const file = join(dst, `${module}.js`);
  let text = readFileSync(file, 'utf8');
  let wrappers = `\nimport { record as __record } from './__recorder.js';\n`;
  for (const name of names) {
    const re = new RegExp(`^export function ${name}\\(`, 'm');
    if (!re.test(text)) throw new Error(`${module}.${name} not found`);
    text = text.replace(re, `function __orig_${name}(`);
    wrappers += `export function ${name}(...args) { return __record(${JSON.stringify(module)}, ${JSON.stringify(name)}, args, () => __orig_${name}(...args)); }\n`;
  }
  writeFileSync(file, text + wrappers);
}
console.log(`instrumented ${dst}`);
