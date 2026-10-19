import { join } from 'node:path';
import { tool, jsonSchema, type FlexibleSchema, type ToolSet } from 'ai';
import { Ajv } from 'ajv';
import { z } from 'zod';
import { WorkspaceFiles, limitText, unlinkSafe } from './files.js';
import { runCommand } from './process.js';
import { todoItemSchema, type TodoItem } from './session.js';

export function sanitizeBashCommand(command: string, platform = process.platform): string {
  if (platform !== 'win32') return command;
  return command.replace(/((?:^|[\s;&|()])(?:\d+|&)?>>?\s*)(?:nul|\/dev\/nul)(?=$|[\s;&|()><])/gi, '$1/dev/null');
}

export interface Approval { tool: string; input: unknown; signal: AbortSignal }
export type Approve = (request: Approval) => Promise<boolean>;
export interface ToolCall { toolCallId: string; toolName: string; input: unknown }
export interface ToolDefinition<Input = unknown> {
  description: string; schema: z.ZodType<Input>; readOnly: boolean;
  execute: (input: Input, signal: AbortSignal) => Promise<string>;
}
export interface RunBudget { spent: number; limit: number }
export class PermissionDeniedError extends Error {}
interface RegisteredTool {
  description: string; schema: FlexibleSchema<unknown>; readOnly: boolean;
  parse: (input: unknown) => unknown;
  execute: (input: unknown, signal: AbortSignal) => Promise<string>;
}

export function formatTodos(todos: TodoItem[]): string {
  if (!todos.length) return 'Todo list is empty.';
  const pending = todos.filter(t => t.status === 'pending').length;
  const inProgress = todos.filter(t => t.status === 'in_progress').length;
  const completed = todos.filter(t => t.status === 'completed').length;
  const lines = todos.map((t, i) => {
    const icon = t.status === 'completed' ? '[x]' : t.status === 'in_progress' ? '[>]' : '[ ]';
    return `${i + 1}. ${icon} (${t.priority}) ${t.content}`;
  });
  return `Todos (${completed}/${todos.length} completed, ${inProgress} in progress, ${pending} pending):\n${lines.join('\n')}`;
}

export const updatePlanItemSchema = z.object({
  step: z.string().min(1).describe('Task step text.'),
  status: z.enum(['pending', 'in_progress', 'completed']).describe('Step status.'),
});

export const userInputOptionSchema = z.object({
  label: z.string().min(1).describe('User-facing option label (1-5 words).'),
  description: z.string().default('').describe('One short sentence explaining impact/tradeoff if selected.'),
});

export const userInputQuestionSchema = z.object({
  id: z.string().min(1).describe('Stable identifier for mapping answers (snake_case).'),
  header: z.string().default('Question').describe('Short header label shown in the UI (12 or fewer chars).'),
  question: z.string().min(1).describe('Single-sentence prompt shown to the user.'),
  options: z.array(userInputOptionSchema).min(1).describe('2-3 choices. Put the recommended option first with "(Recommended)".'),
});
export type UserInputQuestion = z.infer<typeof userInputQuestionSchema>;

export const TOOL_NAME_ALIASES: Record<string, string> = {
  read: 'read_file',
  glob: 'list_files',
  ls: 'list_files',
  grep: 'search',
  edit: 'edit_file',
  write: 'write_file',
  wait: 'wait_agent',
  send_message: 'send_input',
  followup_task: 'send_input',
  interrupt_agent: 'close_agent',
  subagent: 'spawn_agent',
  task: 'spawn_agent',
  ask_user: 'request_user_input',
  ask_question: 'request_user_input',
  read_image: 'view_image',
};

export function normalizeToolName(name: string, hasExact?: (n: string) => boolean): string {
  const clean = name.trim().replace(/^(?:multi_agent_v1|multi_agent|functions)\./i, '');
  if (hasExact?.(clean)) return clean;
  return TOOL_NAME_ALIASES[clean] ?? TOOL_NAME_ALIASES[clean.toLowerCase()] ?? clean;
}

export function normalizeToolInput(toolName: string, input: unknown): unknown {
  if (typeof input === 'string') {
    if (toolName === 'delegate_task' || toolName === 'delegate_edit') return { tasks: [input] };
    if (toolName === 'spawn_agent') return { message: input };
    if (toolName === 'wait_agent') return { targets: [input] };
    if (toolName === 'close_agent' || toolName === 'resume_agent') return { target: input };
    if (toolName === 'view_image') return { path: input };
    return input;
  }
  if (!input || typeof input !== 'object' || Array.isArray(input)) return input;
  const obj = { ...(input as Record<string, unknown>) };
  if (['read_file', 'write_file', 'edit_file', 'list_files', 'search', 'view_image'].includes(toolName)) {
    if (obj.path === undefined) {
      const aliasPath = obj.filePath ?? obj.file_path ?? obj.directory;
      if (aliasPath !== undefined) obj.path = aliasPath;
    }
    delete obj.filePath;
    delete obj.file_path;
    delete obj.directory;
  }
  if (toolName === 'search') {
    if (obj.query === undefined && obj.pattern !== undefined) {
      obj.query = obj.pattern;
    }
    delete obj.pattern;
  }
  if (toolName === 'edit_file') {
    if (obj.oldText === undefined) {
      const aliasOld = obj.old_string ?? obj.old_str;
      if (aliasOld !== undefined) obj.oldText = aliasOld;
    }
    if (obj.newText === undefined) {
      const aliasNew = obj.new_string ?? obj.new_str;
      if (aliasNew !== undefined) obj.newText = aliasNew;
    }
    delete obj.old_string;
    delete obj.old_str;
    delete obj.new_string;
    delete obj.new_str;
  }
  if (toolName === 'bash') {
    if (obj.command === undefined && obj.cmd !== undefined) {
      obj.command = obj.cmd;
    }
    delete obj.cmd;
    if (typeof obj.command === 'string') {
      obj.command = sanitizeBashCommand(obj.command);
    }
  }
  if (toolName === 'delegate_task' || toolName === 'delegate_edit') {
    if (obj.tasks === undefined) {
      const single = obj.task ?? obj.prompt ?? obj.message ?? obj.instructions;
      if (typeof single === 'string') obj.tasks = [single];
      else if (Array.isArray(single)) obj.tasks = single;
    } else if (typeof obj.tasks === 'string') {
      obj.tasks = [obj.tasks];
    } else if (Array.isArray(obj.tasks)) {
      obj.tasks = obj.tasks.map(item => {
        if (typeof item === 'string') return item;
        if (item && typeof item === 'object') {
          const rec = item as Record<string, unknown>;
          const text = rec.task ?? rec.prompt ?? rec.message ?? rec.description;
          if (typeof text === 'string') return text;
        }
        return item;
      });
    }
    delete obj.task;
    delete obj.prompt;
    delete obj.message;
    delete obj.instructions;
  }
  if (toolName === 'spawn_agent') {
    if (obj.message === undefined) {
      const msg = obj.prompt ?? obj.task ?? obj.instruction ?? obj.instructions;
      if (typeof msg === 'string') obj.message = msg;
      else if (Array.isArray(obj.items)) {
        const parts = (obj.items as Array<Record<string, unknown>>)
          .map(it => (typeof it?.text === 'string' ? it.text : ''))
          .filter(Boolean);
        if (parts.length) obj.message = parts.join('\n');
      }
    }
    if (obj.agent_type === undefined) {
      const role = obj.role ?? obj.subagent_type ?? obj.type;
      if (typeof role === 'string') obj.agent_type = role;
    }
    if (obj.nickname === undefined && typeof obj.task_name === 'string') {
      obj.nickname = obj.task_name;
    }
    if (obj.fork_context === undefined && obj.fork_turns !== undefined) {
      obj.fork_context = obj.fork_turns === 'all' || obj.fork_turns === true;
    }
    if (obj.reasoning_effort === 'minimal') {
      obj.reasoning_effort = 'low';
    }
    delete obj.prompt;
    delete obj.task;
    delete obj.instruction;
    delete obj.instructions;
    delete obj.items;
    delete obj.role;
    delete obj.subagent_type;
    delete obj.type;
    delete obj.task_name;
    delete obj.fork_turns;
  }
  if (toolName === 'wait_agent') {
    const rawTargets = obj.targets ?? obj.ids ?? obj.target ?? obj.id ?? obj.agent_ids ?? obj.agent_id;
    if (typeof rawTargets === 'string') {
      obj.targets = [rawTargets];
    } else if (Array.isArray(rawTargets)) {
      obj.targets = rawTargets.map(String).filter(Boolean);
    }
    if (typeof obj.timeoutMs === 'number' && obj.timeout_ms === undefined) {
      obj.timeout_ms = obj.timeoutMs;
    }
    delete obj.ids;
    delete obj.target;
    delete obj.id;
    delete obj.agent_ids;
    delete obj.agent_id;
    delete obj.timeoutMs;
  }
  if (toolName === 'send_input') {
    if (obj.target === undefined) {
      const t = obj.id ?? obj.agent_id ?? obj.task_name ?? obj.recipient;
      if (typeof t === 'string') obj.target = t;
    }
    if (obj.message === undefined) {
      const m = obj.prompt ?? obj.task ?? obj.input;
      if (typeof m === 'string') obj.message = m;
      else if (Array.isArray(obj.items)) {
        const parts = (obj.items as Array<Record<string, unknown>>)
          .map(it => (typeof it?.text === 'string' ? it.text : ''))
          .filter(Boolean);
        if (parts.length) obj.message = parts.join('\n');
      }
    }
    delete obj.id;
    delete obj.agent_id;
    delete obj.task_name;
    delete obj.recipient;
    delete obj.prompt;
    delete obj.task;
    delete obj.input;
    delete obj.items;
  }
  if (toolName === 'close_agent' || toolName === 'resume_agent') {
    if (obj.target === undefined) {
      const t = obj.id ?? obj.agent_id ?? obj.task_name;
      if (typeof t === 'string') obj.target = t;
    }
    delete obj.id;
    delete obj.agent_id;
    delete obj.task_name;
  }
  if (toolName === 'update_plan') {
    if (obj.plan === undefined) {
      const alt = obj.steps ?? obj.items ?? obj.todos;
      if (Array.isArray(alt)) {
        obj.plan = alt.map(item => {
          if (typeof item === 'string') return { step: item, status: 'pending' };
          if (item && typeof item === 'object') {
            const rec = item as Record<string, unknown>;
            return {
              step: String(rec.step ?? rec.content ?? rec.title ?? rec.text ?? ''),
              status: rec.status === 'in_progress' || rec.status === 'completed' ? rec.status : 'pending',
            };
          }
          return item;
        });
      }
    }
    delete obj.steps;
    delete obj.items;
    delete obj.todos;
  }
  if (toolName === 'request_user_input') {
    if (obj.questions === undefined && typeof obj.question === 'string') {
      const rawOpts = Array.isArray(obj.options) ? obj.options : ['Yes (Recommended)', 'No'];
      obj.questions = [{
        id: typeof obj.id === 'string' ? obj.id : 'question_1',
        header: typeof obj.header === 'string' ? obj.header : 'Question',
        question: obj.question,
        options: rawOpts.map(o => typeof o === 'string' ? { label: o, description: '' } : o),
      }];
      delete obj.question;
      delete obj.options;
      delete obj.id;
      delete obj.header;
    }
  }
  return obj;
}

export class ToolRegistry {
  readonly files: WorkspaceFiles;
  budget?: RunBudget;
  planMode = false;
  todos: TodoItem[] = [];
  onTodoUpdate?: (todos: TodoItem[]) => void;
  onPlanExit?: (plan: string) => void;
  askUser?: (questions: UserInputQuestion[], signal: AbortSignal) => Promise<Record<string, string[]>>;
  pendingImages: Array<{ path: string; mimeType: string; bytes: Uint8Array }> = [];
  private definitions = new Map<string, RegisteredTool>();
  constructor(readonly cwd: string, readonly approve: Approve, public readOnly = false) {
    this.files = new WorkspaceFiles(cwd);
    this.register('read_file', {
      description: 'Read a workspace text file with line numbers. Read an existing file before editing or overwriting it.',
      schema: z.object({ path: z.string(), offset: z.number().int().min(1).default(1), lines: z.number().int().min(1).max(500).default(200) }),
      readOnly: true, execute: input => this.files.read(input.path, input.offset, input.lines),
    });
    this.register('list_files', {
      description: 'List one workspace directory. Directories end in /, symlinks in @.',
      schema: z.object({ path: z.string().default('.') }), readOnly: true,
      execute: input => this.files.list(input.path),
    });
    this.register('search', {
      description: 'Search for literal text within a workspace directory. Uses system ripgrep when available, with a bounded portable fallback.',
      schema: z.object({ query: z.string().min(1), path: z.string().default('.') }), readOnly: true,
      execute: async (input, signal) => {
        const path = await this.files.path(input.path);
        try {
          const result = await runCommand('rg', ['--fixed-strings', '--line-number', '--hidden', '--glob', '!.git', '--glob', '!node_modules', '--max-count', '50', '--max-columns', '1000', '--max-columns-preview', '--', input.query, path], { cwd, signal, timeoutMs: 30_000 });
          if (result.code > 1) throw new Error(result.output || 'ripgrep failed.');
          return limitText(result.output || 'No matches found.');
        } catch (error) {
          if ((error as NodeJS.ErrnoException).code !== 'ENOENT') throw error;
          signal.throwIfAborted();
          return this.files.fallbackSearch(input.query, input.path, signal);
        }
      },
    });
    this.register('todo_write', {
      description: 'Record and update the complete task checklist to track multi-step implementation progress. Pass the full updated list of todos.',
      schema: z.object({ todos: z.array(todoItemSchema) }), readOnly: true,
      execute: async input => {
        this.todos = input.todos;
        this.onTodoUpdate?.(this.todos);
        return `Updated todo list.\n${formatTodos(this.todos)}`;
      },
    });
    this.register('todo_read', {
      description: 'Read the current session task checklist.',
      schema: z.object({}), readOnly: true,
      execute: async () => formatTodos(this.todos),
    });
    this.register('update_plan', {
      description: 'Updates the task plan (Codex-compatible). Provide an optional explanation and a list of plan items, each with a step and status (pending, in_progress, completed). At most one step should be in_progress at a time.',
      schema: z.object({
        explanation: z.string().optional().describe('Optional note explaining why the plan changed.'),
        plan: z.array(updatePlanItemSchema).describe('The list of plan steps.'),
      }),
      readOnly: true,
      execute: async input => {
        this.todos = input.plan.map((item, idx) => ({
          id: String(idx + 1),
          content: item.step,
          status: item.status,
          priority: 'medium' as const,
        }));
        this.onTodoUpdate?.(this.todos);
        const header = input.explanation ? `Plan updated (${input.explanation}).` : 'Plan updated.';
        return `${header}\n${formatTodos(this.todos)}`;
      },
    });
    this.register('request_user_input', {
      description: 'Request user input for one to three short multiple-choice questions and wait for the response.',
      schema: z.object({
        questions: z.array(userInputQuestionSchema).min(1).max(3).describe('Questions to show the user. Prefer 1 and do not exceed 3.'),
      }),
      readOnly: true,
      execute: async (input, signal) => {
        if (this.askUser) {
          const answers = await this.askUser(input.questions, signal);
          const formatted = Object.fromEntries(
            Object.entries(answers).map(([id, list]) => [id, { answers: list }]),
          );
          return JSON.stringify({ answers: formatted });
        }
        const fallback = Object.fromEntries(
          input.questions.map(q => [q.id, { answers: [q.options[0]?.label ?? ''] }]),
        );
        return JSON.stringify({ answers: fallback, note: 'Non-interactive mode: selected default first option.' });
      },
    });
    this.register('view_image', {
      description: 'Attach a local image from the filesystem (PNG, JPEG, WEBP, GIF) to the conversation context so you can inspect it visually.',
      schema: z.object({
        path: z.string().min(1).describe('Local filesystem path to an image file.'),
        detail: z.enum(['high', 'original']).optional().describe('Optional detail override.'),
      }),
      readOnly: true,
      execute: async input => {
        const img = await this.files.readImage(input.path);
        this.pendingImages.push({ path: img.resolvedPath, mimeType: img.mediaType, bytes: img.buffer });
        return `Attached image ${img.resolvedPath} (${img.mediaType}, ${img.buffer.byteLength} bytes).`;
      },
    });
    this.register('exit_plan_mode', {
      description: 'Use in plan mode when your implementation plan is complete. Presents the Markdown plan for user approval and exits plan mode upon approval so you can begin coding.',
      schema: z.object({ plan: z.string().min(1) }), readOnly: true,
      execute: async (input, signal) => {
        if (!this.planMode) {
          return 'Session is not in plan mode; you can already edit files and run tools directly.';
        }
        const ok = await this.approve({ tool: 'exit_plan_mode', input, signal });
        if (!ok) {
          return 'User chose to stay in plan mode and revise the plan. Incorporate any feedback and call exit_plan_mode again when ready.';
        }
        this.planMode = false;
        this.onPlanExit?.(input.plan);
        return `Plan approved — exited plan mode. You can now use write_file, edit_file, apply_patch, todo_write, and bash to implement the plan step by step.\n\n## Approved Plan:\n${input.plan}`;
      },
    });
    this.register('write_file', {
      description: 'Create or overwrite a workspace UTF-8 file. Existing files must first be read. Use edit_file for small changes.',
      schema: z.object({ path: z.string(), content: z.string() }), readOnly: false,
      execute: input => this.files.write(input.path, input.content),
    });
    this.register('edit_file', {
      description: 'Replace exact text in a previously read file. Refuses stale reads and ambiguous matches.',
      schema: z.object({ path: z.string(), oldText: z.string().min(1), newText: z.string(), replaceAll: z.boolean().default(false) }),
      readOnly: false, execute: input => this.files.edit(input.path, input.oldText, input.newText, input.replaceAll),
    });
    this.register('apply_patch', {
      description: 'Apply a multi-hunk or multi-file patch ("*** Begin Patch" ... "*** Update File: <path>" / "*** Add File: <path>" / "*** Delete File: <path>" ... "*** End Patch").',
      schema: z.object({ patch: z.string().min(1) }), readOnly: false,
      execute: input => this.files.applyPatch(input.patch),
    });
    this.register('bash', {
      description: 'Execute a POSIX bash command in the workspace (Git Bash on Windows, NOT cmd.exe). Always use POSIX syntax and /dev/null (never >nul or 2>nul). Output is bounded; do not start persistent background servers.',
      schema: z.object({ command: z.string().min(1), timeoutMs: z.number().int().min(1000).max(600_000).default(120_000) }),
      readOnly: false, execute: async (input, signal) => {
        const safeCmd = sanitizeBashCommand(input.command);
        try {
          const result = await runCommand('bash', ['-c', safeCmd], { cwd, signal, timeoutMs: input.timeoutMs });
          return `Exit code: ${result.code}\n${result.output}`;
        } finally {
          if (process.platform === 'win32') {
            await unlinkSafe(join(cwd, 'nul')).catch(() => {});
          }
        }
      },
    });
  }
  takePendingImages() {
    const out = this.pendingImages.slice();
    this.pendingImages.length = 0;
    return out;
  }
  get effectiveReadOnly(): boolean {
    return this.readOnly || this.planMode;
  }
  private ajv = new Ajv({ strict: false, validateFormats: false, allowUnionTypes: true });
  register<Input>(name: string, definition: ToolDefinition<Input>) {
    return this.add(name, { ...definition, parse: input => definition.schema.parse(input),
      execute: (input, signal) => definition.execute(definition.schema.parse(input), signal) });
  }
  registerJson(name: string, definition: Omit<ToolDefinition, 'schema'> & { schema: Record<string, unknown> }) {
    const cleanSchema = { ...definition.schema };
    delete cleanSchema.$schema;
    delete cleanSchema.$id;
    let parse: (input: unknown) => unknown;
    try {
      const validate = this.ajv.compile(cleanSchema);
      parse = (input: unknown) => {
        if (!validate(input)) throw new Error(`Invalid arguments for ${name}: ${this.ajv.errorsText(validate.errors)}`);
        return input;
      };
    } catch {
      parse = (input: unknown) => {
        if (input === null || typeof input !== 'object' || Array.isArray(input)) {
          throw new Error(`Invalid arguments for ${name}: expected an object.`);
        }
        return input;
      };
    }
    return this.add(name, { ...definition, schema: jsonSchema(cleanSchema), parse });
  }
  private add(name: string, definition: RegisteredTool) {
    if (!/^[a-z][a-z0-9_]*$/.test(name) || this.definitions.has(name)) throw new Error(`Invalid or duplicate tool: ${name}`);
    this.definitions.set(name, definition);
    return () => this.definitions.delete(name);
  }
  resolveToolName(name: string): string {
    return normalizeToolName(name, n => this.definitions.has(n));
  }
  schemas(): ToolSet {
    const collabTools = new Set([
      'spawn_agent', 'wait_agent', 'send_input', 'close_agent', 'resume_agent', 'list_agents',
      'update_plan', 'request_user_input', 'view_image',
      'list_mcp_resources', 'list_mcp_resource_templates', 'read_mcp_resource',
    ]);
    return Object.fromEntries([...this.definitions].filter(([name, d]) => {
      if (name === 'exit_plan_mode' && !this.planMode) return false;
      if ((name === 'todo_write' || name === 'todo_read' || collabTools.has(name)) && this.readOnly && !this.planMode) return false;
      return !this.effectiveReadOnly || d.readOnly;
    }).map(([name, d]) => [name, tool({ description: d.description, inputSchema: d.schema })]));
  }
  isReadOnly(name: string) {
    const canonical = this.resolveToolName(name);
    if (
      canonical === 'exit_plan_mode' ||
      canonical === 'todo_write' ||
      canonical === 'update_plan' ||
      canonical === 'request_user_input' ||
      canonical === 'wait_agent' ||
      canonical === 'send_input' ||
      canonical === 'close_agent' ||
      canonical === 'resume_agent'
    ) {
      return false; // serialize state transitions
    }
    return this.definitions.get(canonical)?.readOnly === true;
  }
  async execute(call: ToolCall, signal: AbortSignal): Promise<{ text: string; denied?: boolean }> {
    signal.throwIfAborted();
    const canonicalName = this.resolveToolName(call.toolName);
    const definition = this.definitions.get(canonicalName);
    if (!definition || (this.effectiveReadOnly && !definition.readOnly)) {
      throw new Error(`Tool is not available in ${this.planMode ? 'plan' : 'read-only'} mode: ${call.toolName}`);
    }
    const normalizedInput = normalizeToolInput(canonicalName, call.input);
    const input = definition.parse(normalizedInput);
    if (!definition.readOnly && !await this.approve({ tool: canonicalName, input, signal })) {
      return { text: 'Permission denied by the user. Stop this run.', denied: true };
    }
    signal.throwIfAborted();
    try { return { text: await definition.execute(input, signal) }; }
    catch (error) {
      if (error instanceof PermissionDeniedError) return { text: error.message, denied: true };
      throw error;
    }
  }
}


