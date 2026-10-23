import { mkdir, readdir, readFile, stat, unlink } from 'node:fs/promises';
import { join, resolve } from 'node:path';
import { z } from 'zod';
import { atomicJson, configDir, type EffortLevel } from './config.js';
import { sanitizeBashCommand } from './tools.js';
import { runCommand } from './process.js';
import { limitText } from './files.js';

export type WorkflowStepType = 'prompt' | 'shell' | 'subagent' | 'network' | 'approval';

export interface WorkflowStep {
  id: string;
  title?: string;
  type: WorkflowStepType;
  prompt?: string;
  command?: string;
  tasks?: string[];
  topic?: string;
  roles?: string[];
  rounds?: number;
  readOnly?: boolean;
  mode?: 'ask' | 'auto' | 'plan' | 'readonly';
  effort?: EffortLevel;
  continueOnError?: boolean;
  message?: string;
}

export interface WorkflowDefinition {
  name: string;
  description: string;
  scope: 'workspace' | 'user' | 'builtin';
  path?: string;
  inputs?: Record<string, string>;
  steps: WorkflowStep[];
}

export interface WorkflowStepResult {
  id: string;
  title: string;
  type: WorkflowStepType;
  status: 'completed' | 'skipped' | 'failed';
  output: string;
  durationMs: number;
}

export interface WorkflowRunResult {
  workflow: string;
  status: 'completed' | 'aborted' | 'failed';
  steps: WorkflowStepResult[];
  finalOutput: string;
}

const workflowStepSchema = z.object({
  id: z.string().trim().min(1).max(64).optional(),
  title: z.string().trim().max(140).optional(),
  type: z.enum(['prompt', 'shell', 'subagent', 'network', 'approval']).default('prompt'),
  prompt: z.string().optional(),
  command: z.string().optional(),
  tasks: z.array(z.string()).optional(),
  topic: z.string().optional(),
  roles: z.array(z.string()).optional(),
  rounds: z.number().int().min(1).max(4).optional(),
  readOnly: z.boolean().optional(),
  mode: z.enum(['ask', 'auto', 'plan', 'readonly']).optional(),
  effort: z.enum(['low', 'medium', 'high', 'xhigh', 'max']).optional(),
  continueOnError: z.boolean().optional(),
  message: z.string().optional(),
});

const workflowJsonSchema = z.object({
  name: z.string().trim().min(1).max(64).optional(),
  description: z.string().trim().max(240).optional(),
  inputs: z.record(z.string(), z.string()).optional(),
  steps: z.array(workflowStepSchema).min(1).max(20),
});

export const runWorkflowToolSchema = z.object({
  name: z
    .string()
    .trim()
    .min(1)
    .max(64)
    .describe('Name of the workflow to execute (e.g. "feature-dev", "bug-hunt", "tdd-cycle", or a custom workflow name).'),
  input: z
    .string()
    .trim()
    .max(16_000)
    .default('')
    .describe('Input arguments or task description passed to the workflow ($input, $1, key=value).'),
});

export const BUILTIN_WORKFLOWS: WorkflowDefinition[] = [
  {
    name: 'feature-dev',
    description: 'Brainstorm with Agent Network -> Plan & Implement -> Run Verification',
    scope: 'builtin',
    steps: [
      {
        id: 'brainstorm',
        title: 'Agent Network Architecture Brainstorm',
        type: 'network',
        topic: 'Design and plan implementation for: $input',
        roles: ['architect', 'researcher', 'critic', 'pragmatist'],
        rounds: 2,
      },
      {
        id: 'implement',
        title: 'Implement Feature & Tests',
        type: 'prompt',
        prompt:
          'Implement the feature requested below based on the Agent Network consensus blueprint.\n\n' +
          'User Request:\n$input\n\n' +
          'Agent Network Blueprint:\n$prev\n\n' +
          'Write clean, well-typed code and add or update unit tests.',
      },
      {
        id: 'verify',
        title: 'Verify & Summarize',
        type: 'prompt',
        prompt: 'Run relevant typecheck/lint/tests to verify the implementation for "$input", fix any issues, and summarize the changes.',
      },
    ],
  },
  {
    name: 'bug-hunt',
    description: 'Parallel Subagent Root-Cause Investigation -> Fix -> Regression Test',
    scope: 'builtin',
    steps: [
      {
        id: 'investigate',
        title: 'Subagent Root-Cause Analysis',
        type: 'subagent',
        readOnly: true,
        tasks: [
          'Search the codebase and locate the exact files, functions, and root cause related to: $input',
          'Find existing tests covering the affected area and identify what regression test is needed for: $input',
        ],
      },
      {
        id: 'fix',
        title: 'Apply Minimal Fix & Regression Test',
        type: 'prompt',
        prompt:
          'Fix the bug described below using the investigation findings.\n\n' +
          'Bug Report:\n$input\n\n' +
          'Investigation Findings:\n$prev\n\n' +
          'Apply the minimal root-cause fix and add a regression test.',
      },
    ],
  },
  {
    name: 'tdd-cycle',
    description: 'Write Failing Test First (Red) -> Implement Fix/Feature (Green) -> Refactor',
    scope: 'builtin',
    steps: [
      {
        id: 'red',
        title: 'Write Failing Test (Red Phase)',
        type: 'prompt',
        prompt: 'TDD Step 1 (Red): Write a focused test for "$input" and run it to confirm it fails or tests the new behavior accurately. Do not implement the production code yet.',
      },
      {
        id: 'green',
        title: 'Implement to Pass Test (Green Phase)',
        type: 'prompt',
        prompt: 'TDD Step 2 (Green): Implement the minimal production code required so the test for "$input" passes, then run the test suite.',
      },
      {
        id: 'refactor',
        title: 'Refactor & Lint Check',
        type: 'prompt',
        prompt: 'TDD Step 3 (Refactor): Clean up any duplication, ensure types and lint pass, and provide a concise summary.',
      },
    ],
  },
  {
    name: 'security-audit',
    description: 'Multi-Agent Security & Reliability Review of Workspace Changes',
    scope: 'builtin',
    steps: [
      {
        id: 'git_diff',
        title: 'Collect Git Status & Diff',
        type: 'shell',
        command: 'git status --short && git diff HEAD',
        continueOnError: true,
      },
      {
        id: 'council_audit',
        title: 'Security & Critic Agent Network Audit',
        type: 'network',
        topic: 'Perform a security, edge-case, and reliability audit on this repository. Focus: $input\n\nRecent git diff:\n$prev',
        roles: ['security', 'critic', 'architect'],
        rounds: 2,
      },
    ],
  },
];

export function parseWorkflowArgs(rawInput: string, defaults?: Record<string, string>): {
  fullInput: string;
  positional: string[];
  named: Record<string, string>;
} {
  const named: Record<string, string> = { ...defaults };
  const positional: string[] = [];
  const tokens = rawInput.match(/(?:[^\s"']+|"[^"]*"|'[^']*')+/g) ?? [];
  const remainingTokens: string[] = [];

  for (const tok of tokens) {
    const kvMatch = /^(?:--)?([A-Za-z_][A-Za-z0-9_-]*)=(.*)$/.exec(tok);
    if (kvMatch) {
      const k = kvMatch[1];
      const v = kvMatch[2].replace(/^["']|["']$/g, '');
      named[k] = v;
    } else {
      const clean = tok.replace(/^["']|["']$/g, '');
      positional.push(clean);
      remainingTokens.push(tok);
    }
  }

  return {
    fullInput: remainingTokens.join(' ').trim() || rawInput.trim(),
    positional,
    named,
  };
}

function getShellQuoteContext(prefix: string): 'none' | 'single' | 'double' {
  let state: 'none' | 'single' | 'double' = 'none';
  for (let i = 0; i < prefix.length; i++) {
    const ch = prefix[i];
    if (state === 'none') {
      if (ch === '\\') i++;
      else if (ch === "'") state = 'single';
      else if (ch === '"') state = 'double';
    } else if (state === 'single') {
      if (ch === "'") state = 'none';
    } else if (state === 'double') {
      if (ch === '\\') i++;
      else if (ch === '"') state = 'none';
    }
  }
  return state;
}

export function shellEscape(value: string, quoteContext: 'none' | 'single' | 'double' = 'none'): string {
  if (quoteContext === 'single') {
    return value.replace(/'/g, "'\\''");
  }
  if (quoteContext === 'double') {
    return value.replace(/[\\"$`!]/g, '\\$&');
  }
  return `'${value.replace(/'/g, "'\\''")}'`;
}

export function interpolateWorkflowTemplate(
  template: string,
  context: {
    input: string;
    positional: string[];
    named: Record<string, string>;
    prevOutput: string;
    stepOutputs: Record<string, string>;
  },
  options?: { mode?: 'text' | 'shell' },
): string {
  const isShell = options?.mode === 'shell';
  const formatVal = (val: string, offset: number, src: string): string => {
    if (!isShell) return val;
    const qCtx = getShellQuoteContext(src.slice(0, offset));
    return shellEscape(val, qCtx);
  };

  let out = template;
  out = out.replace(/\{\{\s*steps\.([A-Za-z0-9_-]+)\.output\s*\}\}/g, (_m, id: string, offset: number, src: string) =>
    formatVal(context.stepOutputs[id] ?? '', offset, src),
  );
  out = out.replace(/\{\{\s*input\s*\}\}|\$input\b/g, (_m, offset: number, src: string) =>
    formatVal(context.input, offset, src),
  );
  out = out.replace(/\{\{\s*prev\s*\}\}|\$prev\b/g, (_m, offset: number, src: string) =>
    formatVal(context.prevOutput, offset, src),
  );

  if (isShell) {
    // In shell mode, only replace explicit {{1}}..{{N}} or {{varName}} placeholders so $1, $2 (awk/bash) and $ENV_VARS stay intact.
    out = out.replace(/\{\{\s*(\d+)\s*\}\}/g, (_m, g1: string, offset: number, src: string) => {
      const idx = Number(g1) - 1;
      return formatVal(context.positional[idx] ?? '', offset, src);
    });
    out = out.replace(/\{\{\s*([A-Za-z_][A-Za-z0-9_-]*)\s*\}\}/g, (full, key: string, offset: number, src: string) => {
      if (key in context.named) return formatVal(context.named[key], offset, src);
      if (key in context.stepOutputs) return formatVal(context.stepOutputs[key], offset, src);
      return full;
    });
    return out;
  }

  out = out.replace(/\{\{\s*(\d+)\s*\}\}|\$(\d+)\b/g, (_, g1: string | undefined, g2: string | undefined) => {
    const idx = Number(g1 ?? g2) - 1;
    return context.positional[idx] ?? '';
  });
  out = out.replace(/\{\{\s*([A-Za-z_][A-Za-z0-9_-]*)\s*\}\}|\$([A-Za-z_][A-Za-z0-9_-]*)\b/g, (full, g1: string | undefined, g2: string | undefined) => {
    const key = g1 ?? g2 ?? '';
    if (key in context.named) return context.named[key];
    if (key in context.stepOutputs) return context.stepOutputs[key];
    return full;
  });
  return out;
}

export function parseMarkdownWorkflow(
  filename: string,
  content: string,
  scope: 'workspace' | 'user',
  filePath: string,
): WorkflowDefinition {
  let name = filename.replace(/\.(?:md|json)$/i, '');
  let description = '';
  const inputs: Record<string, string> = {};
  let body = content;

  const fmMatch = content.match(/^---\r?\n([\s\S]*?)\r?\n---\r?\n?/);
  if (fmMatch) {
    body = content.slice(fmMatch[0].length);
    for (const line of fmMatch[1].split(/\r?\n/)) {
      const m = line.match(/^([A-Za-z0-9_-]+)\s*:\s*(.+)$/);
      if (!m) continue;
      const k = m[1].toLowerCase();
      const v = m[2].trim().replace(/^["']|["']$/g, '');
      if (k === 'name' && v) name = v;
      else if (k === 'description' && v) description = v;
      else if (k.startsWith('input.') && v) inputs[k.slice(6)] = v;
    }
  }

  // Split by `## Step:` or `## [type]` headings if present
  const stepRegex = /^##\s+(?:Step\s*\d*\s*:\s*)?(.+?)(?:\s+\[([^\]]+)\])?\s*$/gim;
  const matches = [...body.matchAll(stepRegex)];
  const steps: WorkflowStep[] = [];

  if (matches.length > 0) {
    for (let i = 0; i < matches.length; i++) {
      const m = matches[i];
      const title = m[1].trim();
      const attrRaw = m[2] ?? '';
      const start = (m.index ?? 0) + m[0].length;
      const end = i + 1 < matches.length ? (matches[i + 1].index ?? body.length) : body.length;
      const sectionBody = body.slice(start, end).trim();

      let type: WorkflowStepType = 'prompt';
      let id = `step_${i + 1}`;
      let roles: string[] | undefined;
      let rounds: number | undefined;
      for (const part of attrRaw.split(/[\s,]+/).filter(Boolean)) {
        const [k, v] = part.split('=');
        if (!v && ['prompt', 'shell', 'subagent', 'network', 'approval'].includes(k.toLowerCase())) {
          type = k.toLowerCase() as WorkflowStepType;
        } else if (k === 'type' && ['prompt', 'shell', 'subagent', 'network', 'approval'].includes(v.toLowerCase())) {
          type = v.toLowerCase() as WorkflowStepType;
        } else if (k === 'id' && v) {
          id = v;
        } else if (k === 'roles' && v) {
          roles = v.split('|').map(s => s.trim()).filter(Boolean);
        } else if (k === 'rounds' && v) {
          const r = Number(v);
          if (Number.isInteger(r) && r >= 1 && r <= 4) rounds = r;
        }
      }

      if (type === 'shell') {
        const codeBlock = /```(?:bash|sh|shell)?\r?\n([\s\S]*?)```/i.exec(sectionBody);
        const command = (codeBlock ? codeBlock[1] : sectionBody).trim();
        steps.push({ id, title, type: 'shell', command });
      } else if (type === 'network') {
        steps.push({ id, title, type: 'network', topic: sectionBody, roles, rounds });
      } else if (type === 'subagent') {
        const bullets = sectionBody
          .split(/\r?\n/)
          .map(l => l.replace(/^\s*[-*]\s+/, '').trim())
          .filter(Boolean);
        steps.push({ id, title, type: 'subagent', tasks: bullets.length ? bullets : [sectionBody], readOnly: true });
      } else if (type === 'approval') {
        steps.push({ id, title, type: 'approval', message: sectionBody || title });
      } else {
        steps.push({ id, title, type: 'prompt', prompt: sectionBody });
      }
    }
  } else {
    const trimmed = body.trim();
    steps.push({
      id: 'main',
      title: name,
      type: 'prompt',
      prompt: trimmed || 'Complete workflow task: $input',
    });
  }

  if (!description) {
    const firstLine = body.split(/\r?\n/).find(l => l.trim() && !l.trim().startsWith('#')) ?? '';
    description = firstLine.trim().slice(0, 120) || `Custom workflow (${steps.length} step${steps.length > 1 ? 's' : ''})`;
  }

  return {
    name,
    description,
    scope,
    path: filePath,
    ...(Object.keys(inputs).length ? { inputs } : {}),
    steps,
  };
}

export async function discoverWorkflows(cwd: string): Promise<WorkflowDefinition[]> {
  const roots: Array<{ dir: string; scope: 'workspace' | 'user' }> = [
    { dir: join(cwd, '.goon', 'workflows'), scope: 'workspace' },
    { dir: join(cwd, '.agents', 'workflows'), scope: 'workspace' },
    { dir: join(configDir(), 'workflows'), scope: 'user' },
  ];

  const seen = new Set<string>();
  const discovered: WorkflowDefinition[] = [];

  for (const root of roots) {
    let entries;
    try {
      entries = await readdir(root.dir, { withFileTypes: true });
    } catch {
      continue;
    }
    for (const entry of entries.sort((a, b) => a.name.localeCompare(b.name))) {
      if (!entry.isFile()) continue;
      if (!entry.name.endsWith('.json') && !entry.name.endsWith('.md')) continue;
      const fullPath = join(root.dir, entry.name);
      try {
        const st = await stat(fullPath);
        if (st.size > 256 * 1024) continue;
        const raw = await readFile(fullPath, 'utf8');
        let def: WorkflowDefinition;
        if (entry.name.endsWith('.json')) {
          const parsed = workflowJsonSchema.parse(JSON.parse(raw));
          const wfName = parsed.name ?? entry.name.slice(0, -5);
          def = {
            name: wfName,
            description: parsed.description ?? `Workflow (${parsed.steps.length} steps)`,
            scope: root.scope,
            path: fullPath,
            inputs: parsed.inputs,
            steps: parsed.steps.map((s, idx) => ({
              ...s,
              id: s.id ?? `step_${idx + 1}`,
            })),
          };
        } else {
          def = parseMarkdownWorkflow(entry.name, raw, root.scope, fullPath);
        }
        const key = def.name.toLowerCase();
        if (!seen.has(key)) {
          seen.add(key);
          discovered.push(def);
        }
      } catch {
        // Ignore malformed workflow files
      }
    }
  }

  for (const builtin of BUILTIN_WORKFLOWS) {
    if (!seen.has(builtin.name.toLowerCase())) {
      seen.add(builtin.name.toLowerCase());
      discovered.push(builtin);
    }
  }

  return discovered;
}

export async function createWorkflowScaffold(
  cwd: string,
  name: string,
  description?: string,
  globalScope = false,
  overwrite = false,
): Promise<{ path: string; workflow: WorkflowDefinition }> {
  const cleanName = name.trim().replace(/[^A-Za-z0-9_-]/g, '-').replace(/^-+|-+$/g, '');
  if (!cleanName) throw new Error('Workflow name cannot be empty.');
  const targetDir = globalScope ? join(configDir(), 'workflows') : join(resolve(cwd), '.goon', 'workflows');
  await mkdir(targetDir, { recursive: true });
  const filePath = join(targetDir, `${cleanName}.json`);
  const mdPath = join(targetDir, `${cleanName}.md`);

  if (!overwrite) {
    const existingJson = await stat(filePath).then(() => true, () => false);
    if (existingJson) {
      throw new Error(`Workflow "${cleanName}" already exists at ${filePath}. Remove it first with "goon workflow rm ${cleanName}".`);
    }
    const existingMd = await stat(mdPath).then(() => true, () => false);
    if (existingMd) {
      throw new Error(`Workflow "${cleanName}" already exists at ${mdPath}. Remove it first with "goon workflow rm ${cleanName}".`);
    }
  }

  const def: WorkflowDefinition = {
    name: cleanName,
    description: description?.trim() || `Custom multi-step workflow: ${cleanName}`,
    scope: globalScope ? 'user' : 'workspace',
    path: filePath,
    inputs: {
      focus: 'quality and tests',
    },
    steps: [
      {
        id: 'brainstorm',
        title: 'Brainstorm Ideas with Agent Network',
        type: 'network',
        topic: 'Brainstorm approach for: $input (Focus: $focus)',
        roles: ['architect', 'critic', 'pragmatist'],
        rounds: 1,
      },
      {
        id: 'execute',
        title: 'Implement Solution',
        type: 'prompt',
        prompt: 'Execute task "$input" following this plan:\n\n$prev',
      },
    ],
  };

  await atomicJson(filePath, {
    name: def.name,
    description: def.description,
    inputs: def.inputs,
    steps: def.steps,
  });

  return { path: filePath, workflow: def };
}

export async function removeCustomWorkflow(cwd: string, name: string): Promise<string> {
  const all = await discoverWorkflows(cwd);
  const found = all.find(w => w.name.toLowerCase() === name.trim().toLowerCase());
  if (!found) throw new Error(`Workflow "${name}" not found.`);
  if (found.scope === 'builtin' || !found.path) {
    throw new Error(`Workflow "${found.name}" is a built-in workflow and cannot be deleted.`);
  }
  await unlink(found.path);
  return found.path;
}

export interface WorkflowExecutionHooks {
  cwd: string;
  signal: AbortSignal;
  effectiveReadOnly?: boolean;
  onProgress?: (text: string) => void;
  approveShell?: (command: string, signal: AbortSignal) => Promise<boolean>;
  runPrompt: (prompt: string, step: WorkflowStep, signal: AbortSignal) => Promise<string>;
  runSubagents?: (tasks: string[], readOnly: boolean, signal: AbortSignal) => Promise<string>;
  runNetwork?: (
    topic: string,
    roles: string[] | undefined,
    rounds: number | undefined,
    signal: AbortSignal,
  ) => Promise<string>;
  confirmStep?: (message: string, signal: AbortSignal) => Promise<boolean>;
}

export async function executeWorkflow(
  workflow: WorkflowDefinition,
  rawArgs: string,
  hooks: WorkflowExecutionHooks,
): Promise<WorkflowRunResult> {
  const parsedArgs = parseWorkflowArgs(rawArgs, workflow.inputs);
  const stepOutputs: Record<string, string> = {};
  const results: WorkflowStepResult[] = [];
  let prevOutput = '';

  hooks.onProgress?.(
    `\n╭─── Workflow: ${workflow.name} (${workflow.steps.length} step${workflow.steps.length > 1 ? 's' : ''}) ───╮\n` +
      `  Description : ${workflow.description}\n` +
      (parsedArgs.fullInput ? `  Input       : ${parsedArgs.fullInput}\n` : ''),
  );

  for (let i = 0; i < workflow.steps.length; i++) {
    hooks.signal.throwIfAborted();
    const step = workflow.steps[i];
    const stepTitle = step.title ?? `${step.type} (${step.id})`;
    const started = Date.now();
    const interpContext = {
      input: parsedArgs.fullInput,
      positional: parsedArgs.positional,
      named: parsedArgs.named,
      prevOutput,
      stepOutputs,
    };
    const interp = (text: string | undefined, mode: 'text' | 'shell' = 'text') =>
      interpolateWorkflowTemplate(text ?? '', interpContext, { mode });

    hooks.onProgress?.(`\n◆ Step [${i + 1}/${workflow.steps.length}] ${stepTitle} [${step.type}]\n`);

    try {
      let stepOutput = '';

      if (step.type === 'approval') {
        const msg = interp(step.message ?? step.prompt ?? `Approve workflow step "${stepTitle}"?`);
        const ok = hooks.confirmStep ? await hooks.confirmStep(msg, hooks.signal) : true;
        if (!ok) {
          results.push({
            id: step.id,
            title: stepTitle,
            type: step.type,
            status: 'skipped',
            output: 'Aborted at approval gate by user.',
            durationMs: Date.now() - started,
          });
          hooks.onProgress?.(`╰─── Workflow "${workflow.name}" stopped at approval gate ───╯\n`);
          return {
            workflow: workflow.name,
            status: 'aborted',
            steps: results,
            finalOutput: prevOutput,
          };
        }
        stepOutput = 'Approved.';
      } else if (step.type === 'shell') {
        if (hooks.effectiveReadOnly || step.readOnly === true || step.mode === 'plan' || step.mode === 'readonly') {
          throw new Error(`Shell step "${step.id}" is blocked in read-only or plan mode.`);
        }
        const rawCmd = interp(step.command ?? step.prompt ?? '', 'shell');
        if (!rawCmd.trim()) throw new Error(`Shell step "${step.id}" has no command.`);
        const cmd = sanitizeBashCommand(rawCmd);
        if (hooks.approveShell) {
          const allowed = await hooks.approveShell(cmd, hooks.signal);
          if (!allowed) {
            throw new Error(`Permission denied by the user for shell command: ${cmd}`);
          }
        }
        hooks.onProgress?.(`  $ ${cmd}\n`);
        const wfEnv: NodeJS.ProcessEnv = {
          ...process.env,
          GOON_WF_INPUT: parsedArgs.fullInput,
          GOON_WF_PREV: prevOutput,
        };
        for (let pIdx = 0; pIdx < parsedArgs.positional.length; pIdx++) {
          wfEnv[`GOON_WF_ARG_${pIdx + 1}`] = parsedArgs.positional[pIdx];
        }
        for (const [k, v] of Object.entries(parsedArgs.named)) {
          wfEnv[`GOON_WF_VAR_${k.replace(/[^A-Za-z0-9_]/g, '_').toUpperCase()}`] = v;
        }
        const res = await runCommand('bash', ['-c', cmd], {
          cwd: hooks.cwd,
          env: wfEnv,
          signal: AbortSignal.any([hooks.signal, AbortSignal.timeout(120_000)]),
          timeoutMs: 120_000,
        });
        stepOutput = limitText(`Exit code: ${res.code}\n${res.output}`.trim(), 12_000);
        if (res.code !== 0 && !step.continueOnError) {
          throw new Error(`Command exited with code ${res.code}:\n${res.output.slice(0, 1000)}`);
        }
      } else if (step.type === 'network') {
        const topic = interp(step.topic ?? step.prompt ?? '$input');
        if (!hooks.runNetwork) {
          stepOutput = await hooks.runPrompt(
            `Brainstorm from multiple perspectives (${(step.roles ?? ['architect', 'critic']).join(', ')}):\n\n${topic}`,
            step,
            hooks.signal,
          );
        } else {
          stepOutput = await hooks.runNetwork(topic, step.roles, step.rounds, hooks.signal);
        }
      } else if (step.type === 'subagent') {
        const rawTasks = step.tasks?.length ? step.tasks : [step.prompt ?? '$input'];
        const tasks = rawTasks.map(t => interp(t)).filter(Boolean);
        if (!hooks.runSubagents) {
          stepOutput = await hooks.runPrompt(tasks.join('\n\n'), step, hooks.signal);
        } else {
          stepOutput = await hooks.runSubagents(tasks, step.readOnly !== false, hooks.signal);
        }
      } else {
        const promptText = interp(step.prompt ?? '$input');
        stepOutput = await hooks.runPrompt(promptText, step, hooks.signal);
      }

      prevOutput = stepOutput;
      stepOutputs[step.id] = stepOutput;
      results.push({
        id: step.id,
        title: stepTitle,
        type: step.type,
        status: 'completed',
        output: stepOutput,
        durationMs: Date.now() - started,
      });
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      results.push({
        id: step.id,
        title: stepTitle,
        type: step.type,
        status: 'failed',
        output: msg,
        durationMs: Date.now() - started,
      });
      if (!step.continueOnError) {
        hooks.onProgress?.(`  ✗ Step "${stepTitle}" failed: ${msg}\n╰─── Workflow "${workflow.name}" failed ───╯\n`);
        return {
          workflow: workflow.name,
          status: 'failed',
          steps: results,
          finalOutput: msg,
        };
      }
      prevOutput = msg;
      stepOutputs[step.id] = msg;
    }
  }

  hooks.onProgress?.(`\n╰─── Workflow "${workflow.name}" completed (${results.length} steps) ───╯\n`);
  return {
    workflow: workflow.name,
    status: 'completed',
    steps: results,
    finalOutput: prevOutput,
  };
}

export function formatWorkflowDetails(wf: WorkflowDefinition): string {
  const lines: string[] = [
    `Workflow: ${wf.name} [${wf.scope}]${wf.path ? ` (${wf.path})` : ''}`,
    `Description: ${wf.description}`,
  ];
  if (wf.inputs && Object.keys(wf.inputs).length > 0) {
    lines.push(`Default inputs: ${Object.entries(wf.inputs).map(([k, v]) => `${k}="${v}"`).join(', ')}`);
  }
  lines.push(`Steps (${wf.steps.length}):`);
  for (let i = 0; i < wf.steps.length; i++) {
    const s = wf.steps[i];
    const detail =
      s.type === 'shell'
        ? `$ ${s.command ?? ''}`
        : s.type === 'network'
          ? `roles=[${(s.roles ?? ['architect', 'researcher', 'critic', 'pragmatist']).join(', ')}] rounds=${s.rounds ?? 2} topic="${(s.topic ?? '').slice(0, 80)}"`
          : s.type === 'subagent'
            ? `${s.tasks?.length ?? 1} subagent task(s)`
            : (s.prompt ?? s.message ?? '').replace(/\s+/g, ' ').slice(0, 90);
    lines.push(`  ${i + 1}. [${s.id}] ${s.title ?? s.id} (${s.type}) — ${detail}`);
  }
  return lines.join('\n');
}
