import { tool, jsonSchema, type FlexibleSchema, type ToolSet } from 'ai';
import { Ajv } from 'ajv';
import { z } from 'zod';
import { WorkspaceFiles, limitText } from './files.js';
import { runCommand } from './process.js';

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
export class ToolRegistry {
  readonly files: WorkspaceFiles;
  budget?: RunBudget;
  private definitions = new Map<string, RegisteredTool>();
  constructor(readonly cwd: string, readonly approve: Approve, readonly readOnly = false) {
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
    this.register('bash', {
      description: 'Execute a shell command in the workspace as the current OS user. No OS sandbox. Use for git, builds and tests. Output is bounded; do not start persistent background servers.',
      schema: z.object({ command: z.string().min(1), timeoutMs: z.number().int().min(1000).max(600_000).default(120_000) }),
      readOnly: false, execute: async (input, signal) => {
        const result = await runCommand('bash', ['-c', input.command], { cwd, signal, timeoutMs: input.timeoutMs });
        return `Exit code: ${result.code}\n${result.output}`;
      },
    });
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
  schemas(): ToolSet {
    return Object.fromEntries([...this.definitions].filter(([, d]) => !this.readOnly || d.readOnly)
      .map(([name, d]) => [name, tool({ description: d.description, inputSchema: d.schema })]));
  }
  isReadOnly(name: string) { return this.definitions.get(name)?.readOnly === true; }
  async execute(call: ToolCall, signal: AbortSignal): Promise<{ text: string; denied?: boolean }> {
    signal.throwIfAborted();
    const definition = this.definitions.get(call.toolName);
    if (!definition || (this.readOnly && !definition.readOnly)) throw new Error(`Tool is not available: ${call.toolName}`);
    const input = definition.parse(call.input);
    if (!definition.readOnly && !await this.approve({ tool: call.toolName, input, signal })) {
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
