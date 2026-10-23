import { randomUUID } from 'node:crypto';
import { rmdir, unlink } from 'node:fs/promises';
import { join } from 'node:path';
import { z } from 'zod';
import { Harness, type HarnessEvent } from './harness.js';
import { SessionStore } from './session.js';
import { ToolRegistry, type RunBudget } from './tools.js';
import type { RuntimeConfig } from './config.js';
import { limitText } from './files.js';
import { safeError } from './errors.js';
import { lastAssistantAnswer } from './reasoning.js';

export type NetworkMessageKind = 'idea' | 'question' | 'critique' | 'finding' | 'consensus' | 'message';

export interface NetworkMessage {
  id: string;
  from: string;
  to: string; // Agent nickname/ID or '*' for broadcast
  kind: NetworkMessageKind;
  topic?: string;
  content: string;
  createdAt: string;
  readBy: Set<string>;
}

export interface BlackboardEntry {
  id: string;
  author: string;
  role: string;
  topic: string;
  kind: NetworkMessageKind;
  content: string;
  round?: number;
  createdAt: string;
}

export interface NetworkNodeInfo {
  id: string;
  nickname: string;
  role: string;
  status: string;
  task?: string;
}

export interface NetworkPersona {
  id: string;
  name: string;
  role: string;
  focus: string;
}

export const DEFAULT_NETWORK_PERSONAS: Record<string, NetworkPersona> = {
  architect: {
    id: 'architect',
    name: 'Architect',
    role: 'System Architect',
    focus: 'Modular architecture, clean abstractions, data flow, extensibility, and long-term maintainability.',
  },
  researcher: {
    id: 'researcher',
    name: 'Researcher',
    role: 'Codebase & Domain Researcher',
    focus: 'Existing codebase patterns, concrete file/line evidence, reusable utilities, and edge-case discovery.',
  },
  critic: {
    id: 'critic',
    name: 'Critic',
    role: 'Devil’s Advocate & Risk Analyst',
    focus: 'Failure modes, race conditions, security flaws, over-engineering traps, and hidden assumptions.',
  },
  pragmatist: {
    id: 'pragmatist',
    name: 'Pragmatist',
    role: 'Pragmatic Builder',
    focus: 'Simplicity, minimal dependencies, developer experience (DX), fast delivery, and testability.',
  },
  security: {
    id: 'security',
    name: 'Security',
    role: 'Security & Reliability Engineer',
    focus: 'Auth, input validation, least privilege, secret handling, resource bounds, and safe defaults.',
  },
};

const peerSendSchema = z.object({
  to: z
    .string()
    .trim()
    .min(1)
    .max(64)
    .describe('Target agent nickname or ID (or "*" to broadcast to all agents in the network).'),
  message: z
    .string()
    .trim()
    .min(1)
    .max(8_000)
    .describe('Message, idea, critique, or question to send to the peer agent.'),
  kind: z
    .enum(['idea', 'question', 'critique', 'finding', 'consensus', 'message'])
    .default('message')
    .describe('Category of the message.'),
  topic: z
    .string()
    .trim()
    .max(120)
    .optional()
    .describe('Optional short topic tag.'),
});

const peerBroadcastSchema = z.object({
  topic: z
    .string()
    .trim()
    .min(1)
    .max(120)
    .describe('Short title or topic for this shared blackboard entry.'),
  message: z
    .string()
    .trim()
    .min(1)
    .max(12_000)
    .describe('Idea, proposal, finding, or critique to post on the shared Agent Network blackboard.'),
  kind: z
    .enum(['idea', 'question', 'critique', 'finding', 'consensus', 'message'])
    .default('idea')
    .describe('Type of contribution.'),
});

const peerReadSchema = z.object({
  include_read: z
    .boolean()
    .default(false)
    .describe('When true, return all messages instead of only unread direct messages.'),
  include_blackboard: z
    .boolean()
    .default(true)
    .describe('When true, also return shared blackboard entries posted by all agents.'),
});

export const agentNetworkToolSchema = z.object({
  topic: z
    .string()
    .trim()
    .min(1)
    .max(16_000)
    .describe('The problem, feature idea, architecture question, or design challenge for the Agent Network to brainstorm and debate.'),
  roles: z
    .array(z.string().trim().min(1).max(64))
    .min(2)
    .max(5)
    .optional()
    .describe('Agent personas to include (default: ["architect", "researcher", "critic", "pragmatist"]). Custom role names are also supported.'),
  rounds: z
    .number()
    .int()
    .min(1)
    .max(4)
    .default(2)
    .describe('Number of deliberation rounds (1 = independent proposals + synthesis, 2+ = cross-critique & peer debate rounds).'),
  explore_codebase: z
    .boolean()
    .default(true)
    .describe('When true, agents can use read-only codebase tools (read_file, list_files, search) while formulating ideas.'),
});

/**
 * Shared Peer-to-Peer Message Bus and Blackboard for all agents in a session.
 * Enables direct messaging (`peer_send`), shared ideation (`peer_broadcast`),
 * and discovery (`peer_list`, `peer_read`) across spawned subagents and brainstorm rooms.
 */
export class AgentNetworkBus {
  readonly messages: NetworkMessage[] = [];
  readonly blackboard: BlackboardEntry[] = [];
  private readonly nodes = new Map<string, NetworkNodeInfo>();
  private readonly onMessageListeners = new Set<(msg: NetworkMessage) => void>();
  private readonly onBlackboardListeners = new Set<(entry: BlackboardEntry) => void>();

  registerNode(info: NetworkNodeInfo): void {
    this.nodes.set(info.nickname.toLowerCase(), info);
  }

  updateNodeStatus(nickname: string, status: string, task?: string): void {
    const existing = this.nodes.get(nickname.toLowerCase());
    if (existing) {
      existing.status = status;
      if (task !== undefined) existing.task = task;
    }
  }

  listNodes(): NetworkNodeInfo[] {
    return [...this.nodes.values()];
  }

  onMessage(listener: (msg: NetworkMessage) => void): () => void {
    this.onMessageListeners.add(listener);
    return () => this.onMessageListeners.delete(listener);
  }

  onBlackboard(listener: (entry: BlackboardEntry) => void): () => void {
    this.onBlackboardListeners.add(listener);
    return () => this.onBlackboardListeners.delete(listener);
  }

  send(input: {
    from: string;
    to: string;
    content: string;
    kind?: NetworkMessageKind;
    topic?: string;
  }): NetworkMessage {
    const msg: NetworkMessage = {
      id: randomUUID().slice(0, 8),
      from: input.from,
      to: input.to.trim(),
      kind: input.kind ?? 'message',
      ...(input.topic ? { topic: input.topic.trim() } : {}),
      content: input.content.trim(),
      createdAt: new Date().toISOString(),
      readBy: new Set([input.from.toLowerCase()]),
    };
    this.messages.push(msg);
    if (this.messages.length > 500) this.messages.shift();
    for (const listener of this.onMessageListeners) {
      listener(msg);
    }
    return msg;
  }

  postBlackboard(input: {
    author: string;
    role: string;
    topic: string;
    content: string;
    kind?: NetworkMessageKind;
    round?: number;
  }): BlackboardEntry {
    const entry: BlackboardEntry = {
      id: randomUUID().slice(0, 8),
      author: input.author,
      role: input.role,
      topic: input.topic.trim(),
      kind: input.kind ?? 'idea',
      content: input.content.trim(),
      ...(input.round !== undefined ? { round: input.round } : {}),
      createdAt: new Date().toISOString(),
    };
    this.blackboard.push(entry);
    if (this.blackboard.length > 200) this.blackboard.shift();
    for (const listener of this.onBlackboardListeners) {
      listener(entry);
    }
    return entry;
  }

  readInbox(agentNickname: string, includeRead = false): NetworkMessage[] {
    const lower = agentNickname.toLowerCase();
    const matched = this.messages.filter(m => {
      const target = m.to.toLowerCase();
      const isRecipient =
        target === '*' ||
        target === 'all' ||
        target === lower ||
        (this.nodes.get(lower)?.id.toLowerCase().startsWith(target) ?? false);
      if (!isRecipient || m.from.toLowerCase() === lower) return false;
      if (!includeRead && m.readBy.has(lower)) return false;
      return true;
    });
    for (const m of matched) {
      m.readBy.add(lower);
    }
    return matched;
  }

  drainUnreadPromptBlock(agentNickname: string): string {
    const unread = this.readInbox(agentNickname, false);
    if (!unread.length) return '';
    const formatted = unread.map(
      m => `  • [${m.kind.toUpperCase()}] from ${m.from}${m.topic ? ` (${m.topic})` : ''}: ${m.content}`,
    );
    return `\n<peer_messages_inbox>\nPeer agents sent you the following messages:\n${formatted.join('\n')}\n</peer_messages_inbox>\n`;
  }

  clear(): void {
    this.messages.length = 0;
    this.blackboard.length = 0;
  }

  formatBoardSummary(limit = 20): string {
    const lines: string[] = [];
    const nodes = this.listNodes();
    lines.push(`Agent Network Nodes (${nodes.length}):`);
    if (!nodes.length) {
      lines.push('  (no active peer nodes registered yet)');
    } else {
      for (const n of nodes) {
        lines.push(`  • ${n.nickname} [${n.role}] — ${n.status}${n.task ? ` · ${n.task.slice(0, 70)}` : ''}`);
      }
    }
    lines.push('');
    lines.push(`Shared Blackboard (${this.blackboard.length} entries):`);
    if (!this.blackboard.length) {
      lines.push('  (blackboard is empty)');
    } else {
      for (const b of this.blackboard.slice(-limit)) {
        const roundTag = b.round ? ` · R${b.round}` : '';
        lines.push(`  [${b.id}] ${b.author} (${b.role}${roundTag}) [${b.kind}] — ${b.topic}`);
        lines.push(`    ${b.content.replace(/\s+/g, ' ').slice(0, 220)}${b.content.length > 220 ? '…' : ''}`);
      }
    }
    lines.push('');
    lines.push(`Direct Peer Messages (${this.messages.length}):`);
    if (!this.messages.length) {
      lines.push('  (no peer messages sent yet)');
    } else {
      for (const m of this.messages.slice(-limit)) {
        lines.push(
          `  [${m.id}] ${m.from} → ${m.to} [${m.kind}]${m.topic ? ` (${m.topic})` : ''}: ${m.content.replace(/\s+/g, ' ').slice(0, 180)}`,
        );
      }
    }
    return lines.join('\n');
  }

  /**
   * Registers P2P communication tools (`peer_list`, `peer_send`, `peer_broadcast`, `peer_read`)
   * into a `ToolRegistry` for the given agent identity (`selfNickname` and `selfRole`).
   */
  registerPeerTools(
    registry: ToolRegistry,
    selfNickname: string,
    selfRole: string,
    onDirectSend?: (msg: NetworkMessage) => void,
  ): () => void {
    const unregisters: Array<() => void> = [];

    unregisters.push(
      registry.register('peer_list', {
        description:
          'List all agents in the Agent Network, including their nicknames, roles, statuses, and blackboard activity.',
        schema: z.object({}),
        readOnly: true,
        execute: async () =>
          JSON.stringify({
            self: selfNickname,
            agents: this.listNodes(),
            blackboard_count: this.blackboard.length,
            messages_count: this.messages.length,
          }),
      }),
    );

    unregisters.push(
      registry.register('peer_send', {
        description:
          'Send a direct peer-to-peer message (idea, question, critique, finding) to another agent in the network by nickname/ID, or "*" to broadcast.',
        schema: peerSendSchema,
        readOnly: true,
        execute: async input => {
          const msg = this.send({
            from: selfNickname,
            to: input.to,
            content: input.message,
            kind: input.kind,
            topic: input.topic,
          });
          onDirectSend?.(msg);
          return JSON.stringify({
            ok: true,
            message_id: msg.id,
            from: msg.from,
            to: msg.to,
            kind: msg.kind,
          });
        },
      }),
    );

    unregisters.push(
      registry.register('peer_broadcast', {
        description:
          'Post an idea, design proposal, codebase finding, or critique onto the shared Agent Network blackboard for all agents to read and build upon.',
        schema: peerBroadcastSchema,
        readOnly: true,
        execute: async input => {
          const entry = this.postBlackboard({
            author: selfNickname,
            role: selfRole,
            topic: input.topic,
            content: input.message,
            kind: input.kind,
          });
          return JSON.stringify({
            ok: true,
            entry_id: entry.id,
            author: entry.author,
            topic: entry.topic,
            kind: entry.kind,
          });
        },
      }),
    );

    unregisters.push(
      registry.register('peer_read', {
        description:
          'Read direct messages sent to you by other agents and view the shared Agent Network blackboard.',
        schema: peerReadSchema,
        readOnly: true,
        execute: async input => {
          const inbox = this.readInbox(selfNickname, input.include_read);
          return JSON.stringify({
            self: selfNickname,
            inbox: inbox.map(m => ({
              id: m.id,
              from: m.from,
              to: m.to,
              kind: m.kind,
              topic: m.topic,
              content: m.content,
              createdAt: m.createdAt,
            })),
            ...(input.include_blackboard
              ? {
                  blackboard: this.blackboard.slice(-30).map(b => ({
                    id: b.id,
                    author: b.author,
                    role: b.role,
                    round: b.round,
                    kind: b.kind,
                    topic: b.topic,
                    content: b.content,
                  })),
                }
              : {}),
          });
        },
      }),
    );

    return () => {
      for (const fn of unregisters) fn();
    };
  }
}

export interface BrainstormOptions {
  topic: string;
  roles?: string[];
  rounds?: number;
  exploreCodebase?: boolean;
  signal: AbortSignal;
  budget?: RunBudget;
  onProgress?: (line: string) => void;
}

export interface BrainstormResult {
  topic: string;
  rounds: number;
  participants: NetworkPersona[];
  contributions: Array<{
    round: number;
    persona: NetworkPersona;
    response: string;
  }>;
  synthesis: string;
}

function resolvePersonas(roles?: string[]): NetworkPersona[] {
  const requested = roles?.length ? roles : ['architect', 'researcher', 'critic', 'pragmatist'];
  const out: NetworkPersona[] = [];
  const seen = new Set<string>();
  for (const raw of requested) {
    const key = raw.trim().toLowerCase();
    if (!key || seen.has(key)) continue;
    seen.add(key);
    const preset = DEFAULT_NETWORK_PERSONAS[key];
    if (preset) {
      out.push(preset);
    } else {
      const display = raw.trim().charAt(0).toUpperCase() + raw.trim().slice(1);
      out.push({
        id: key,
        name: display,
        role: `${display} Specialist`,
        focus: `Provide specialized ${display} insights, constructive critique, and concrete recommendations.`,
      });
    }
  }
  return out.length >= 2 ? out : [DEFAULT_NETWORK_PERSONAS.architect, DEFAULT_NETWORK_PERSONAS.critic];
}

export const DEFAULT_NETWORK_TOKEN_BUDGET = 120_000;

/**
 * Multi-Agent Brainstorm & Deliberation Network.
 * Runs multiple distinct agent personas across rounds of exploration, P2P debate, and synthesis.
 */
export class AgentNetwork {
  constructor(
    readonly config: RuntimeConfig,
    readonly parent: SessionStore,
    readonly bus: AgentNetworkBus,
    readonly emitEvent: (event: HarnessEvent) => void = () => {},
  ) {}

  get networkSessionsRoot(): string {
    return join(this.config.dataDir, 'network', this.parent.session.id).replace(/\\/g, '/');
  }

  async brainstorm(options: BrainstormOptions): Promise<BrainstormResult> {
    const personas = resolvePersonas(options.roles);
    const rounds = Math.max(1, Math.min(4, options.rounds ?? 2));
    const explore = options.exploreCodebase !== false;
    const parentBudget = options.budget;
    const configuredLimit = parentBudget && parentBudget.limit > 0 ? parentBudget.limit : this.config.runTokenBudget;
    const budget: RunBudget = {
      spent: parentBudget?.spent ?? 0,
      limit: configuredLimit > 0 ? configuredLimit : DEFAULT_NETWORK_TOKEN_BUDGET,
    };
    const contributions: BrainstormResult['contributions'] = [];

    for (const p of personas) {
      this.bus.registerNode({
        id: `net-${p.id}`,
        nickname: p.name,
        role: p.role,
        status: 'ready',
        task: options.topic.slice(0, 120),
      });
    }

    options.onProgress?.(
      `\n╭─── Agent Network Brainstorm · ${personas.map(p => p.name).join(' · ')} (${rounds} round${rounds > 1 ? 's' : ''}) ───╮\n` +
        `  Topic: ${options.topic}\n`,
    );

    try {
      for (let round = 1; round <= rounds; round++) {
        options.signal.throwIfAborted();
        options.onProgress?.(`\n◈ Round ${round}/${rounds} — ${round === 1 ? 'Initial Exploration & Proposals' : 'Cross-Critique & Peer Debate'}\n`);

        // In Round 1, run up to 2 personas concurrently for speed; in debate rounds, run sequentially
        // and append to `contributions` immediately so each persona sees the latest replies from peers in the same round.
        const concurrency = round === 1 ? 2 : 1;
        let idx = 0;

        const worker = async () => {
          while (idx < personas.length && !options.signal.aborted) {
            const curIdx = idx++;
            const persona = personas[curIdx];
            this.bus.updateNodeStatus(persona.name, `round-${round}`);
            options.onProgress?.(`  ▸ [${persona.name} · ${persona.role}] thinking...\n`);

            const response = await this.runPersonaTurn({
              persona,
              allPersonas: personas,
              topic: options.topic,
              round,
              totalRounds: rounds,
              priorContributions: [...contributions],
              explore,
              signal: options.signal,
              budget,
            });
            if (parentBudget) parentBudget.spent = budget.spent;

            contributions.push({ round, persona, response });
            this.bus.postBlackboard({
              author: persona.name,
              role: persona.role,
              topic: `Round ${round}: ${options.topic.slice(0, 60)}`,
              content: response,
              kind: round === 1 ? 'idea' : 'critique',
              round,
            });
            this.bus.updateNodeStatus(persona.name, 'idle');

            const preview = response.replace(/\s+/g, ' ').trim().slice(0, 180);
            options.onProgress?.(`  ● [${persona.name}] ${preview}${response.length > 180 ? '…' : ''}\n`);
          }
        };

        await Promise.all(Array.from({ length: Math.min(concurrency, personas.length) }, worker));
      }

      options.signal.throwIfAborted();
      options.onProgress?.(`\n◈ Synthesizing Agent Network Consensus...\n`);
      const synthesis = await this.runSynthesis({
        topic: options.topic,
        personas,
        rounds,
        contributions,
        signal: options.signal,
        budget,
      });
      if (parentBudget) parentBudget.spent = budget.spent;

      this.bus.postBlackboard({
        author: 'Moderator',
        role: 'Network Synthesizer',
        topic: `Consensus: ${options.topic.slice(0, 60)}`,
        content: synthesis,
        kind: 'consensus',
      });

      options.onProgress?.(`╰─── Agent Network Complete ──────────────────────────────────────╯\n`);

      return {
        topic: options.topic,
        rounds,
        participants: personas,
        contributions,
        synthesis,
      };
    } finally {
      await rmdir(this.networkSessionsRoot).catch(() => {});
    }
  }

  private async runPersonaTurn(params: {
    persona: NetworkPersona;
    allPersonas: NetworkPersona[];
    topic: string;
    round: number;
    totalRounds: number;
    priorContributions: BrainstormResult['contributions'];
    explore: boolean;
    signal: AbortSignal;
    budget: RunBudget;
  }): Promise<string> {
    const store = await SessionStore.open(this.networkSessionsRoot, this.config);
    const sessionId = store.session.id;
    store.session.title = `Network · ${params.persona.name} (R${params.round})`;
    try {
      const tools = new ToolRegistry(this.config.cwd, async () => false, true);
      tools.allowPeerToolsInReadOnly = true;
      this.bus.registerPeerTools(tools, params.persona.name, params.persona.role);
      if (!params.explore) {
        // Expose only peer communication tools when codebase exploration is disabled
        tools.onlyPeerTools = true;
      }

      const peerNames = params.allPersonas
        .filter(p => p.id !== params.persona.id)
        .map(p => `${p.name} (${p.role})`)
        .join(', ');

      const inboxBlock = this.bus.drainUnreadPromptBlock(params.persona.name);
      const priorText = params.priorContributions.length
        ? params.priorContributions
            .map(c => `### [Round ${c.round}] ${c.persona.name} (${c.persona.role}):\n${c.response}`)
            .join('\n\n')
        : '';

      const prompt =
        params.round === 1
          ? `You are **${params.persona.name}** (${params.persona.role}) in a collaborative Agent Network brainstorming session.\n` +
            `Your focus: ${params.persona.focus}\n` +
            `Other agents in this network: ${peerNames}.\n\n` +
            `Topic / Challenge:\n${params.topic}\n\n` +
            `Instructions for Round 1:\n` +
            `1. ${params.explore ? 'Use read-only tools (`list_files`, `search`, `read_file`) if needed to ground your ideas in the actual codebase.' : 'Analyze the topic from your specialist angle.'}\n` +
            `2. You may use \`peer_send\` to ask a pointed question or challenge another agent (${params.allPersonas.map(p => p.name).join(', ')}).\n` +
            `3. Present a sharp, concrete proposal (key ideas, architecture/design choices, and questions for peers) in 150-300 words.`
          : `You are **${params.persona.name}** (${params.persona.role}) in Round ${params.round} of ${params.totalRounds} of the Agent Network deliberation.\n` +
            `Your focus: ${params.persona.focus}\n\n` +
            `Topic:\n${params.topic}\n\n` +
            `Previous Round Proposals from the Network:\n${priorText}\n` +
            inboxBlock +
            `\nInstructions for Round ${params.round}:\n` +
            `1. Directly respond to your peers' proposals and any direct messages in your inbox.\n` +
            `2. Point out specific strengths, blind spots, or trade-offs in other agents' ideas (reference them by name, e.g. @Architect, @Critic).\n` +
            `3. Refine your recommendation so the group converges toward the strongest practical solution (150-300 words).`;

      const harness = new Harness(this.config, store, tools);
      await harness.run(prompt, params.signal, params.budget);
      const answer = lastAssistantAnswer(store.session.messages, this.config.inlineReasoning);
      return limitText(answer || '(no response)', 6_000);
    } catch (err) {
      if (params.signal.aborted) throw err;
      return `[${params.persona.name} error: ${safeError(err, this.config.apiKey)}]`;
    } finally {
      await store.close().catch(() => {});
      await unlink(join(this.networkSessionsRoot, `${sessionId}.json`)).catch(() => {});
    }
  }

  private async runSynthesis(params: {
    topic: string;
    personas: NetworkPersona[];
    rounds: number;
    contributions: BrainstormResult['contributions'];
    signal: AbortSignal;
    budget: RunBudget;
  }): Promise<string> {
    const store = await SessionStore.open(this.networkSessionsRoot, this.config);
    const sessionId = store.session.id;
    store.session.title = `Network · Moderator Synthesis`;
    try {
      const tools = new ToolRegistry(this.config.cwd, async () => false, true);
      tools.onlyPeerTools = true;
      const transcript = params.contributions
        .map(c => `### [Round ${c.round}] ${c.persona.name} (${c.persona.role})\n${c.response}`)
        .join('\n\n');
      const peerMessages = this.bus.messages
        .slice(-20)
        .map(m => `- ${m.from} → ${m.to} [${m.kind}]: ${m.content}`)
        .join('\n');

      const prompt =
        `You are the **Lead Moderator** synthesizing a ${params.rounds}-round Agent Network deliberation among ${params.personas.map(p => `${p.name} (${p.role})`).join(', ')}.\n\n` +
        `Topic:\n${params.topic}\n\n` +
        `Full Agent Network Deliberation:\n${transcript}\n\n` +
        (peerMessages ? `Direct Peer-to-Peer Messages Exchanged:\n${peerMessages}\n\n` : '') +
        `Produce a clear, high-signal synthesis structured with:\n` +
        `1. **Consensus Solution & Core Idea** (what the agents agreed is the best approach and why)\n` +
        `2. **Key Debates & Trade-offs Resolved** (how critiques from Critic/Pragmatist/Architect shaped the design)\n` +
        `3. **Actionable Implementation Plan** (concrete files, components, and verification steps)\n` +
        `Respond in the same language as the user's Topic.`;

      const harness = new Harness(this.config, store, tools);
      await harness.run(prompt, params.signal, params.budget);
      const answer = lastAssistantAnswer(store.session.messages, this.config.inlineReasoning);
      return limitText(answer || '(no synthesis generated)', 12_000);
    } finally {
      await store.close().catch(() => {});
      await unlink(join(this.networkSessionsRoot, `${sessionId}.json`)).catch(() => {});
    }
  }
}

export function formatBrainstormReport(res: BrainstormResult): string {
  const lines: string[] = [];
  lines.push(`# Agent Network Report: ${res.topic}`);
  lines.push(`Participants: ${res.participants.map(p => `**${p.name}** (${p.role})`).join(', ')} · Rounds: ${res.rounds}`);
  lines.push('');
  for (const c of res.contributions) {
    lines.push(`## [Round ${c.round}] ${c.persona.name} (${c.persona.role})`);
    lines.push(c.response.trim());
    lines.push('');
  }
  lines.push(`## Consensus & Action Plan (Moderator Synthesis)`);
  lines.push(res.synthesis.trim());
  return lines.join('\n');
}
