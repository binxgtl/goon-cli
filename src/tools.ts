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

export const TOOL_NAME_ALIASES: Record<string, string> = {
  read: 'read_file',
  glob: 'list_files',
  ls: 'list_files',
  grep: 'search',
  edit: 'edit_file',
  write: 'write_file',
};

export function normalizeToolName(name: string, hasExact?: (n: string) => boolean): string {
  const clean = name.trim();
  if (hasExact?.(clean)) return clean;
  return TOOL_NAME_ALIASES[clean] ?? clean;
}

export function normalizeToolInput(toolName: string, input: unknown): unknown {
  if (!input || typeof input !== 'object' || Array.isArray(input)) return input;
  const obj = { ...(input as Record<string, unknown>) };
  if (['read_file', 'write_file', 'edit_file', 'list_files', 'search'].includes(toolName)) {
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
  return obj;
}

export class ToolRegistry {
  readonly files: WorkspaceFiles;
  budget?: RunBudget;
  planMode = false;
  todos: TodoItem[] = [];
  onTodoUpdate?: (todos: TodoItem[]) => void;
  onPlanExit?: (plan: string) => void;
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
        const beforeSnap = await this.files.snapshotWorkspace();
        const safeCmd = sanitizeBashCommand(input.command);
        try {
          const result = await runCommand('bash', ['-c', safeCmd], { cwd, signal, timeoutMs: input.timeoutMs });
          return `Exit code: ${result.code}\n${result.output}`;
        } finally {
          if (process.platform === 'win32') {
            await unlinkSafe(join(cwd, 'nul')).catch(() => {});
          }
          await this.files.captureWorkspaceDiff(beforeSnap).catch(() => {});
        }
      },
    });
  }
  get effectiveReadOnly(): boolean {
    return this.readOnly || this.planMode;
  }
  register<Input>(name: string, definition: ToolDefinition<Input>) {
    return this.add(name, { ...definition, parse: input => definition.schema.parse(input),
      execute: (input, signal) => definition.execute(definition.schema.parse(input), signal) });
  }
  registerJson(name: string, definition: Omit<ToolDefinition, 'schema'> & { schema: Record<string, unknown> }) {
    const ajv = new Ajv({ strict: false, validateFormats: false });
    const validate = ajv.compile(definition.schema);
    const parse = (input: unknown) => {
      if (!validate(input)) throw new Error(`Invalid arguments for ${name}: ${ajv.errorsText(validate.errors)}`);
      return input;
    };
    return this.add(name, { ...definition, schema: jsonSchema(definition.schema), parse });
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
    return Object.fromEntries([...this.definitions].filter(([name, d]) => {
      if (name === 'exit_plan_mode' && !this.planMode) return false;
      if ((name === 'todo_write' || name === 'todo_read') && this.readOnly && !this.planMode) return false;
      return !this.effectiveReadOnly || d.readOnly;
    }).map(([name, d]) => [name, tool({ description: d.description, inputSchema: d.schema })]));
  }
  isReadOnly(name: string) {
    const canonical = this.resolveToolName(name);
    if (canonical === 'exit_plan_mode' || canonical === 'todo_write') return false; // serialize state transitions
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


