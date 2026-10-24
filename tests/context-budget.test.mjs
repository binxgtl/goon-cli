import test from 'node:test';
import assert from 'node:assert/strict';
import {
  ContextBudgetError,
  allocateContextBudget,
  prepareContextBudget,
  pruneConversation,
  pruneToolResults,
  truncateSystemToBudget,
} from '../dist/context-budget.js';
import { estimatedTokens } from '../dist/context.js';
import { configSchema } from '../dist/config.js';

const BASE_CONFIG = {
  contextWindow: 32768,
  maxOutputTokens: 4096,
  contextReserveRatio: 0.15,
  contextConversationRatio: 0.45,
  contextFilesRatio: 0.25,
};

function toolCall(id) {
  return { role: 'assistant', content: [{ type: 'tool-call', toolCallId: id, toolName: 'bash', input: {} }] };
}
function toolResult(id, value) {
  return { role: 'tool', content: [{ type: 'tool-result', toolCallId: id, toolName: 'bash', output: { type: 'text', value } }] };
}

test('context-budget: config defaults for the three ratios (P18)', () => {
  const config = configSchema.parse({});
  assert.equal(config.contextReserveRatio, 0.15);
  assert.equal(config.contextConversationRatio, 0.45);
  assert.equal(config.contextFilesRatio, 0.25);
});

test('context-budget: allocation invariant reserve+system+conversation+files === inputBudget', () => {
  for (const [contextWindow, maxOutputTokens] of [[8192, 512], [8192, 128], [32768, 4096], [128000, 16384], [200000, 32000]]) {
    const config = { ...BASE_CONFIG, contextWindow, maxOutputTokens };
    const allocation = allocateContextBudget(config, 'You are Goon. ' + 'x'.repeat(100));
    assert.equal(allocation.inputBudget, Math.max(1024, contextWindow - maxOutputTokens));
    const sum = allocation.reserve + allocation.systemBudget + allocation.conversation + allocation.files;
    assert.equal(sum, allocation.inputBudget, `${contextWindow}/${maxOutputTokens}`);
    assert.ok(allocation.conversation > 0 && allocation.files > 0);
  }
});

test('context-budget: min context 8192/512 never truncates the base system prompt', () => {
  const config = { ...BASE_CONFIG, contextWindow: 8192, maxOutputTokens: 512 };
  // ~2600 chars of base system prompt (identity + rules), ~1300 estimated tokens: above the 15%
  // floor (1152) and far below the 35% cap (2688).
  const baseSystem = `You are Goon, an independent terminal coding agent. Work in /tmp/proj.
Complete the user's requested coding task, inspecting files before changing them.
Prefer small, reviewable changes. Run relevant checks and report actual results.
Use the provided tools; never claim an action succeeded without a tool result.
${'Additional operating rules. '.repeat(90)}`;
  const baseTokens = estimatedTokens([], baseSystem);
  assert.ok(baseTokens > Math.floor(7680 * 0.15), 'test system must sit above the floor');
  const allocation = allocateContextBudget(config, baseSystem);
  assert.equal(allocation.inputBudget, 7680);
  assert.equal(allocation.systemBudget, Math.min(baseTokens, Math.floor(7680 * 0.35)));
  assert.equal(allocation.truncated, false);
  const prepared = prepareContextBudget({ messages: [{ role: 'user', content: 'hi' }], system: baseSystem, config });
  assert.equal(prepared.system, baseSystem, 'base system must survive byte-for-byte');
  assert.ok(prepared.stats.after <= prepared.stats.inputBudget);
});

test('context-budget: truncateSystemToBudget cuts AGENTS.md/memories first, identity last', () => {
  const identity = 'You are Goon, an independent terminal coding agent. Keep safety rules.';
  const memories = '\nPersistent user memories:\n' + '- memory line\n'.repeat(200);
  const guidance = '\nWorkspace guidance (AGENTS.md):\n' + 'workspace rule line\n'.repeat(400);
  const system = identity + memories + guidance;
  const fullTokens = estimatedTokens([], system);
  // Budget that fits identity + memories but not the guidance block.
  const guidanceTokens = estimatedTokens([], guidance);
  const trimmed = truncateSystemToBudget(system, fullTokens - guidanceTokens + 20);
  assert.ok(!trimmed.includes('workspace rule line'), 'guidance body must be cut first');
  assert.ok(trimmed.includes('Persistent user memories:'), 'memories survive when they fit');
  assert.ok(trimmed.startsWith(identity), 'identity is preserved');
  // Budget that fits only the identity: memories are cut too.
  const tightest = truncateSystemToBudget(system, estimatedTokens([], identity) + 10);
  assert.ok(!tightest.includes('Persistent user memories:'));
  assert.ok(tightest.includes('Keep safety rules.'), 'identity rules are kept');
  // Absurdly small budget: hard truncation marker.
  const hard = truncateSystemToBudget(system, 10);
  assert.ok(hard.includes('[system prompt truncated to fit the context budget]'));
});

test('context-budget: per-result prune keeps pairing and shrinks oversized outputs', () => {
  const big = 'a'.repeat(8192) + 'END-MARKER';
  const messages = [toolCall('c1'), toolResult('c1', big), { role: 'user', content: 'summarize' }];
  const { messages: pruned, pruned: didPrune } = pruneToolResults(messages, 8192);
  assert.equal(didPrune, true);
  assert.equal(pruned.length, messages.length, 'messages are kept');
  assert.equal(pruned[1].content.length, 1, 'parts are kept (pairing intact)');
  const value = pruned[1].content[0].output.value;
  assert.ok(value.length < big.length);
  assert.ok(value.includes('[... tool result middle pruned to fit context budget ...]'));
  assert.ok(value.includes('END-MARKER'), 'tail is preserved');
  assert.ok(!value.includes('aaaaaa'.repeat(2000)));
  // Small results pass through untouched; input is not mutated.
  const untouched = pruneToolResults([toolResult('c2', 'short')], 8192);
  assert.equal(untouched.pruned, false);
  assert.equal(untouched.messages[0].content[0].output.value, 'short');
  assert.equal(messages[1].content[0].output.value, big);
});

test('context-budget: 200 x 8KiB tool results prune to the conversation budget with no orphans', () => {
  const config = { ...BASE_CONFIG, contextWindow: 32768, maxOutputTokens: 4096 };
  const messages = [{ role: 'user', content: 'run the sweep' }];
  for (let i = 0; i < 200; i++) {
    messages.push(toolCall(`c${i}`));
    messages.push(toolResult(`c${i}`, `result-${i}-` + String(i).padStart(4, '0').repeat(600) + 'x'.repeat(7500)));
  }
  messages.push({ role: 'user', content: 'summarize what happened' });
  const prepared = prepareContextBudget({ messages, system: '', config });
  assert.ok(prepared.stats.after <= prepared.stats.inputBudget, `after=${prepared.stats.after} budget=${prepared.stats.inputBudget}`);
  // Pairing: every remaining tool result still has its tool call, and no tool message is orphaned.
  const callIds = new Set();
  for (const msg of prepared.messages) {
    if (msg.role !== 'assistant' || !Array.isArray(msg.content)) continue;
    for (const part of msg.content) if (part.type === 'tool-call') callIds.add(part.toolCallId);
  }
  let sawOmitted = false;
  for (const msg of prepared.messages) {
    if (msg.role !== 'tool' || !Array.isArray(msg.content)) continue;
    for (const part of msg.content) {
      assert.ok(callIds.has(part.toolCallId), 'every tool result keeps its tool call');
      if (part.output.value.includes('[tool result omitted to fit context; re-run if needed]')) sawOmitted = true;
    }
  }
  assert.ok(sawOmitted, 'aggregate prune must blank old tool outputs');
  assert.ok(prepared.messages.some(m => m.role === 'user' && m.content === 'summarize what happened'), 'newest user message kept');
  assert.ok(prepared.stats.truncated);
});

test('context-budget: duplicate tool results collapse to a marker, keeping the part', () => {
  const value = 'identical-output-'.repeat(80);
  const messages = [toolCall('a'), toolResult('a', value), toolCall('b'), toolResult('b', value), toolCall('c'), toolResult('c', 'different-output-'.repeat(60))];
  const prepared = prepareContextBudget({ messages, system: '', config: BASE_CONFIG });
  const results = prepared.messages.filter(m => m.role === 'tool').map(m => m.content[0].output.value);
  assert.equal(results.length, 3, 'no message removed');
  assert.equal(results[0], value, 'first occurrence is kept');
  assert.equal(results[1], '[duplicate tool result omitted: same as earlier call]');
  assert.notEqual(results[2], '[duplicate tool result omitted: same as earlier call]');
});

test('context-budget: aggregate prune keeps the newest user+assistant and never orphans tool results', () => {
  const messages = [
    { role: 'user', content: 'first request ' + 'old '.repeat(500) },
    toolCall('old-call'),
    toolResult('old-call', 'old result ' + 'z'.repeat(400)),
    { role: 'user', content: 'second request ' + 'mid '.repeat(500) },
    { role: 'assistant', content: 'working on it ' + 'q'.repeat(300) },
    { role: 'user', content: 'final question ' + 'new '.repeat(400) },
  ];
  const budget = estimatedTokens(
    [{ role: 'user', content: 'final question ' + 'new '.repeat(400) }, { role: 'assistant', content: 'working on it ' + 'q'.repeat(300) }],
    '',
  ) + 50;
  const pruned = pruneConversation(messages, budget);
  assert.equal(pruned.at(-1).content, 'final question ' + 'new '.repeat(400), 'newest user message is always kept');
  assert.ok(pruned.at(-2).content.startsWith('working on it'), 'newest assistant message is kept');
  assert.ok(pruned[0].role !== 'tool', 'history never starts with an orphan tool message');
  for (const msg of pruned) {
    if (msg.role !== 'tool') continue;
    assert.ok(callStillPresent(pruned, msg), 'tool result still has its call');
  }
  assert.ok(pruned.length < messages.length, 'old messages were dropped');
  // The input is untouched.
  assert.equal(messages.length, 6);
});

function callStillPresent(messages, toolMsg) {
  const ids = new Set(toolMsg.content.map(p => p.toolCallId));
  const idx = messages.indexOf(toolMsg);
  for (let i = idx - 1; i >= 0; i--) {
    const msg = messages[i];
    if (msg.role !== 'assistant' || !Array.isArray(msg.content)) continue;
    for (const part of msg.content) if (part.type === 'tool-call' && ids.has(part.toolCallId)) return true;
  }
  return false;
}

test('context-budget: untranscribable conversation throws ContextBudgetError, never exceeds budget', () => {
  const config = { ...BASE_CONFIG, contextWindow: 8192, maxOutputTokens: 512 };
  const hugeUser = 'g'.repeat(60_000);
  const messages = [{ role: 'user', content: 'old small' }, { role: 'assistant', content: 'ok' }, { role: 'user', content: hugeUser }];
  assert.throws(() => prepareContextBudget({ messages, system: '', config }), ContextBudgetError);
  assert.throws(() => pruneConversation(messages, 1000), ContextBudgetError);
  // The newest user message alone exceeding the budget cannot be fixed by dropping history.
  assert.throws(() => pruneConversation([{ role: 'user', content: hugeUser }], 1000), ContextBudgetError);
});

test('context-budget: retrieved memory is injected as an untrusted user block, never in the system prompt', () => {
  const config = { ...BASE_CONFIG };
  const context = {
    rag: [
      { path: 'src/a.ts', startLine: 1, endLine: 12, content: 'export const alphaToken = "A".repeat(300);', score: 0.91 },
      { path: 'src/b.ts', startLine: 1, endLine: 12, content: 'export const betaToken = "B".repeat(300);', score: 0.42 },
    ],
    repoTree: 'src/\n  a.ts\n  b.ts',
    workingNotes: 'Task: wire the widget',
    preferences: ['style: concise'],
  };
  const prepared = prepareContextBudget({ messages: [{ role: 'user', content: 'wire the widget' }], system: 'You are Goon.', config, context });
  const last = prepared.messages.at(-1);
  assert.equal(last.role, 'user', 'untrusted context must be a user-role message');
  const text = typeof last.content === 'string' ? last.content : '';
  assert.ok(text.startsWith('<untrusted_context source="rag,repo" trust="untrusted">'));
  assert.ok(text.includes('not instructions'));
  assert.ok(text.includes('src/a.ts') && text.includes('betaToken'));
  assert.ok(text.includes('<repo_tree>') && text.includes('Task: wire the widget') && text.includes('style: concise'));
  assert.ok(!prepared.system.includes('alphaToken'), 'RAG never leaks into the system prompt');
  assert.ok(prepared.stats.after <= prepared.stats.inputBudget);
  // stats report the retrieval budget honestly.
  assert.ok(prepared.stats.files > 0);
});

test('context-budget: oversized memory context is trimmed to the files budget (lowest score dropped first)', () => {
  const config = { ...BASE_CONFIG, contextWindow: 8192, maxOutputTokens: 512 };
  const context = {
    rag: Array.from({ length: 8 }, (_, i) => ({
      path: `src/f${i}.ts`,
      startLine: 1,
      endLine: 20,
      content: `chunk ${i}: ` + 'payload'.repeat(200),
      score: 1 - i * 0.1,
    })),
  };
  const prepared = prepareContextBudget({ messages: [{ role: 'user', content: 'explore' }], system: '', config, context });
  const last = prepared.messages.at(-1);
  assert.equal(last.role, 'user');
  const text = typeof last.content === 'string' ? last.content : '';
  assert.ok(text.includes('chunk 0:'), 'highest-scored chunk is kept');
  assert.ok(text.length < 8000, `block was trimmed (got ${text.length} chars)`);
  assert.ok(prepared.stats.after <= prepared.stats.inputBudget);
  // Zero files budget: no block at all.
  const noFiles = prepareContextBudget({
    messages: [{ role: 'user', content: 'explore' }],
    system: '',
    config: { contextWindow: 8192, maxOutputTokens: 512, contextReserveRatio: 0.5, contextConversationRatio: 1, contextFilesRatio: 0 },
    context,
  });
  const lastNone = noFiles.messages.at(-1);
  assert.equal(lastNone.role, 'user');
  assert.ok(!(typeof lastNone.content === 'string' && lastNone.content.includes('<untrusted_context')), 'no block when the files budget is 0');
});

test('context-budget: referenced files are truncated to the files budget with a read_file marker', () => {
  const config = { ...BASE_CONFIG, contextWindow: 8192, maxOutputTokens: 512 };
  const block = body => `<referenced_file path="src/big.ts">\n${body}\n</referenced_file>`;
  const messages = [{ role: 'user', content: `explain\n\n${block('r'.repeat(5000))}\n\n${block('s'.repeat(5000))}` }];
  const prepared = prepareContextBudget({ messages, system: '', config });
  const text = prepared.messages[0].content;
  assert.ok(text.includes('[... referenced file truncated to fit the context budget; use read_file for the full content ...]'));
  assert.ok(text.includes('[referenced file omitted: context budget]'), 'second block is dropped entirely');
  const kept = [...text.matchAll(/<referenced_file[^>]*>([\s\S]*?)<\/referenced_file>/g)].map(m => m[1].replace(/\[[^\]]*\]/g, '').length);
  assert.ok(kept[0] > 0 && kept[0] <= 3842 + 120, `first block truncated to budget (got ${kept[0]})`);
  assert.ok(prepared.stats.after <= prepared.stats.inputBudget);
  assert.ok(prepared.stats.reason?.includes('referenced files truncated'));
  // The original session message is untouched.
  assert.ok(messages[0].content.includes('r'.repeat(5000)));
});

test('context-budget: prepareContextBudget never mutates the input messages', () => {
  const value = 'm'.repeat(9000);
  const messages = [toolCall('x1'), toolResult('x1', value), { role: 'user', content: 'go' }];
  const snapshot = JSON.stringify(messages);
  prepareContextBudget({ messages, system: 'sys', config: { ...BASE_CONFIG, contextWindow: 8192, maxOutputTokens: 512 } });
  assert.equal(JSON.stringify(messages), snapshot);
});

test('context-budget: stats report before/after and truncation reasons', () => {
  const config = { ...BASE_CONFIG };
  const messages = [toolCall('d1'), toolResult('d1', 'x'.repeat(20_000)), { role: 'user', content: 'done' }];
  const prepared = prepareContextBudget({ messages, system: '', config });
  assert.equal(prepared.stats.inputBudget, 28672);
  assert.ok(prepared.stats.before > prepared.stats.after);
  assert.equal(prepared.stats.truncated, true);
  assert.match(prepared.stats.reason, /tool results pruned/);
  assert.ok(!prepared.stats.reason.includes('system truncated'), 'empty system is not truncated');
});
