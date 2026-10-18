import { emitKeypressEvents, type Key } from 'node:readline';
import { stripVTControlCharacters } from 'node:util';
import type { ReadStream, WriteStream } from 'node:tty';
import type { HarnessEvent, ToolEventMeta } from './harness.js';
import type { SubagentEvent } from './subagents.js';

const SPINNER = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
export const SLASH_COMMANDS = [
  '/help', '/setup', '/model', '/effort', '/mode', '/plan', '/revert', '/undo',
  '/checkpoints', '/todo', '/resume', '/new', '/compact', '/diff', '/review', '/fork', '/rename',
  '/export', '/init', '/status', '/tools', '/sessions', '/doctor', '/clear', '/exit',
];

const SLASH_DESCRIPTIONS: Record<string, string> = {
  '/help': 'show commands & shortcuts',
  '/model': 'switch model & auto-sync limits',
  '/effort': 'set reasoning effort (low..max)',
  '/plan': 'toggle read-only Plan Mode',
  '/mode': 'cycle ask / plan / auto / readonly',
  '/revert': 'revert turns (code + messages)',
  '/undo': 'undo last turn (code + messages)',
  '/compact': 'summarize history to free context',
  '/diff': 'show git diff in workspace',
  '/review': 'run code review on current diff',
  '/resume': 'resume a saved session',
  '/new': 'start a fresh session',
  '/status': 'show session & token status',
  '/doctor': 'verify environment & gateway /models',
  '/exit': 'quit goon cli',
};

// Strip whole escape sequences before removing remaining control bytes.
export function terminalText(value: string): string {
  // oxlint-disable-next-line eslint/no-control-regex
  return stripVTControlCharacters(value).replace(/[\u0000-\u0008\u000b-\u001f\u007f-\u009f]/g, '');
}

const width = (c: string) =>
  /\p{Mark}/u.test(c)
    ? 0
    : /[\p{Extended_Pictographic}\u1100-\u115f\u2e80-\ua4cf\uac00-\ud7a3\uf900-\ufaff\ufe10-\ufe6f\uff01-\uff60]/u.test(c)
      ? 2
      : 1;

export function wrapTerminal(text: string, columns: number): string[] {
  const lines = [''];
  let used = 0;
  for (const c of terminalText(text).replace(/\t/g, '  ')) {
    if (c === '\n') {
      lines.push('');
      used = 0;
      continue;
    }
    const size = width(c);
    if (used + size > columns) {
      lines.push('');
      used = 0;
    }
    lines[lines.length - 1] += c;
    used += size;
  }
  return lines;
}

function visibleLength(str: string): number {
  let total = 0;
  for (const c of stripVTControlCharacters(str)) total += width(c);
  return total;
}

function padRightAnsi(str: string, cols: number, bgAnsi = ''): string {
  const vis = visibleLength(str);
  const pad = Math.max(0, cols - vis);
  return `${bgAnsi}${str}${bgAnsi}${' '.repeat(pad)}\x1b[0m`;
}

/** Format inline Markdown (**bold**, *italic*, `code`, [label](url)) into ANSI spans. */
export function formatInlineMarkdown(raw: string, baseSgr = '\x1b[0m'): string {
  let out = raw;
  // Inline code `...` -> Codex Accent Blue (#63A8F8)
  out = out.replace(/`([^`\n]+)`/g, `\x1b[38;2;99;168;248m$1${baseSgr}`);
  // Markdown links [label](url)
  out = out.replace(/\[([^\]]+)\]\(([^)]+)\)/g, `\x1b[4;38;2;99;168;248m$1\x1b[24;2m ($2)${baseSgr}`);
  // Bold **...**
  out = out.replace(/\*\*([^*]+)\*\*/g, `\x1b[1m$1\x1b[22m${baseSgr}`);
  // Strikethrough ~~...~~
  out = out.replace(/~~([^~]+)~~/g, `\x1b[9m$1\x1b[29m${baseSgr}`);
  return out;
}

function extractReasoningSummary(buffer: string): { header?: string; detail?: string } {
  const lines = buffer.split(/\r?\n/).map(l => l.trim()).filter(Boolean);
  if (!lines.length) return {};
  let header: string | undefined;
  let detail: string | undefined;
  for (let i = lines.length - 1; i >= 0; i--) {
    const line = lines[i];
    if (line.startsWith('<!--')) continue;
    const boldMatch = /^\*\*([^*]{3,80})\*\*/.exec(line) ?? /^#{1,3}\s+(.{3,80})$/.exec(line);
    if (boldMatch && !header) {
      header = boldMatch[1].trim();
    } else if (!detail) {
      detail = line.replace(/\*\*([^*]+)\*\*/g, '$1').replace(/`([^`]+)`/g, '$1').slice(0, 120);
    }
    if (header && detail) break;
  }
  return { header, detail };
}

function formatCompactDuration(ms: number): string {
  const totalSec = Math.max(0, Math.floor(ms / 1000));
  if (totalSec < 60) return `${totalSec}s`;
  const m = Math.floor(totalSec / 60);
  const s = totalSec % 60;
  return `${m}m ${String(s).padStart(2, '0')}s`;
}

function formatCompactDiff(diffText: string, maxLines = 14): string[] {
  const rawLines = diffText.split(/\r?\n/);
  const out: string[] = [];
  let oldLine = 1;
  let newLine = 1;
  let shown = 0;
  let omitted = 0;
  for (const line of rawLines) {
    if (line.startsWith('--- ') || line.startsWith('+++ ')) continue;
    const hunkMatch = /^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@/.exec(line);
    if (hunkMatch) {
      if (shown > 0 && shown < maxLines) out.push('      ⋮');
      oldLine = Number(hunkMatch[1]);
      newLine = Number(hunkMatch[2]);
      continue;
    }
    if (line.startsWith('+')) {
      if (shown < maxLines) {
        out.push(`    ${String(newLine).padStart(4)} +${line.slice(1)}`);
        shown++;
      } else omitted++;
      newLine++;
    } else if (line.startsWith('-')) {
      if (shown < maxLines) {
        out.push(`    ${String(oldLine).padStart(4)} -${line.slice(1)}`);
        shown++;
      } else omitted++;
      oldLine++;
    } else if (line.startsWith(' ')) {
      if (shown < maxLines) {
        out.push(`    ${String(newLine).padStart(4)}  ${line.slice(1)}`);
        shown++;
      }
      oldLine++;
      newLine++;
    }
  }
  if (omitted > 0) out.push(`    … +${omitted} more diff lines`);
  return out;
}

interface Pending {
  resolve: (value: string) => void;
  reject: (error: Error) => void;
  cleanup: () => void;
}

export class TerminalUI {
  private transcript = '';
  private input: string[] = [];
  private cursor = 0;
  private scroll = 0;
  private label = 'goon> ';
  private status = 'Ready';
  private modeLabel = 'ask';
  private effortLabel = 'default';
  private todoLabel = '';
  private workspaceFiles: string[] = [];
  private tokens = 0;
  private pending?: Pending;
  private closed = false;
  private timer?: NodeJS.Timeout;
  private spinTimer?: NodeJS.Timeout;
  private spinTick = 0;
  private oldRaw: boolean;
  private history: string[] = [];
  private historyIndex = 0;
  private remember = false;
  private toolStates = new Map<string, string>();
  private outputChannel?: 'text' | 'reasoning';
  private reasoningSource?: 'inline' | 'provider';
  private turnStartedAt?: number;
  private reasoningBuffer = '';
  private reasoningHeader?: string;
  private reasoningDetail?: string;
  private activeToolStream?: { name: string; preview: string };
  private exploreGroup: Array<{ kind: 'read' | 'list' | 'search'; summary: string }> = [];
  private lastRenderedFrame = '';
  onToggleMode?: () => void;
  onCycleEffort?: (direction: 1 | -1) => void;

  constructor(
    public title: string,
    readonly cancel: () => void,
    readonly source: ReadStream = process.stdin,
    readonly target: WriteStream = process.stdout,
  ) {
    this.oldRaw = source.isRaw;
    emitKeypressEvents(source);
    source.setRawMode(true);
    source.resume();
    source.on('keypress', this.keypress);
    source.on('end', this.ended);
    target.on('resize', this.resize);
    target.write('\x1b[?1049h\x1b[?25l');
    this.render();
  }

  clear() {
    this.transcript = '';
    this.scroll = 0;
    this.exploreGroup = [];
    this.reasoningBuffer = '';
    this.reasoningHeader = undefined;
    this.reasoningDetail = undefined;
    this.activeToolStream = undefined;
    this.schedule();
  }

  setTitle(title: string) {
    this.title = terminalText(title);
    this.schedule();
  }

  setMode(mode: string) {
    this.modeLabel = terminalText(mode);
    this.schedule();
  }

  setEffort(effort: string | undefined) {
    this.effortLabel = terminalText(effort || 'default');
    this.schedule();
  }

  setTokens(tokens: number) {
    this.tokens = tokens;
    this.schedule();
  }

  setWorkspaceFiles(files: string[]) {
    this.workspaceFiles = files;
  }

  private flushExploreGroup() {
    if (!this.exploreGroup.length) return;
    const reads = this.exploreGroup.filter(e => e.kind === 'read').map(e => e.summary);
    const others = this.exploreGroup.filter(e => e.kind !== 'read');
    const childLines: string[] = [];
    if (reads.length) childLines.push(`Read ${reads.join(', ')}`);
    for (const item of others) {
      const verb = item.kind === 'list' ? 'List' : 'Search';
      childLines.push(`${verb} ${item.summary}`);
    }
    const formatted = childLines.map((l, idx) => `  ${idx === 0 ? '└' : ' '} ${l}`).join('\n');
    this.appendTranscript(`\n• Explored\n${formatted}\n`);
    this.exploreGroup = [];
  }

  private appendTranscript(text: string, forceBottom = false) {
    const wasAtBottom = this.scroll === 0;
    this.transcript = (this.transcript + terminalText(text)).slice(-120_000);
    if (forceBottom || wasAtBottom) this.scroll = 0;
    this.schedule(16);
  }

  write(text: string) {
    const isUserPrompt = text.trimStart().startsWith('> ') || text.trimStart().startsWith('› ');
    if (isUserPrompt) this.flushExploreGroup();
    this.appendTranscript(text, isUserPrompt);
  }

  event(event: HarnessEvent | SubagentEvent) {
    if (event.type === 'subagent') {
      if (event.state === 'progress' && event.event.type === 'usage') this.tokens += event.event.total;
      const key = `agent:${event.id}`;
      const state =
        event.state === 'progress'
          ? event.event.type === 'tool'
            ? `${event.event.name}: ${event.event.state}`
            : 'running'
          : event.state;
      this.toolStates.set(key, `agent ${event.id.slice(0, 8)}: ${state}`);
      if (event.state === 'start') {
        this.flushExploreGroup();
        this.appendTranscript(`\n[agent ${event.id.slice(0, 8)}] ${event.task.slice(0, 160)}\n`);
      }
    } else if (event.type === 'tool_stream') {
      this.activeToolStream = { name: event.name, preview: event.preview };
      this.schedule(16);
      return;
    } else if (event.type === 'text' || event.type === 'reasoning') {
      this.activeToolStream = undefined;
      if (event.type === 'text') this.flushExploreGroup();
      if (event.type === 'reasoning') {
        this.reasoningBuffer = (this.reasoningBuffer + event.text).slice(-8000);
        const summary = extractReasoningSummary(this.reasoningBuffer);
        if (summary.header) this.reasoningHeader = summary.header;
        if (summary.detail) this.reasoningDetail = summary.detail;
      }
      if (event.type === 'reasoning' && (this.outputChannel !== 'reasoning' || this.reasoningSource !== event.source)) {
        this.appendTranscript(`\n[reasoning · ${event.source}]\n`);
      } else if (event.type === 'text' && this.outputChannel === 'reasoning') {
        this.appendTranscript('\n[answer]\n');
      } else if (event.type === 'text' && this.outputChannel !== 'text') {
        this.appendTranscript('\n• ');
      }
      if (event.type === 'reasoning') this.reasoningSource = event.source;
      this.outputChannel = event.type;
      this.appendTranscript(event.text);
    } else if (event.type === 'tool') {
      this.activeToolStream = undefined;
      const summary = event.meta?.summary ? ` ${event.meta.summary}` : '';
      this.toolStates.set(event.id, `${event.name}: ${event.state}`);
      if (event.state === 'start') {
        this.reasoningHeader = `${this.formatToolActiveVerb(event.name, event.meta)}${summary}`.slice(0, 72);
      } else if (event.meta) {
        this.recordCompletedToolCell(event.name, event.state, event.meta);
      }
    } else if (event.type === 'todo') {
      this.flushExploreGroup();
      const done = event.todos.filter(t => t.status === 'completed').length;
      this.todoLabel = event.todos.length ? `todo:${done}/${event.todos.length}` : '';
      const lines = event.todos.map(t => {
        const mark = t.status === 'completed' ? '[x]' : t.status === 'in_progress' ? '[>]' : '[ ]';
        return `  ${mark} ${t.content}`;
      });
      this.appendTranscript(`\n[todo · ${done}/${event.todos.length} completed]\n${lines.join('\n')}\n`);
    } else if (event.type === 'plan_exit') {
      this.flushExploreGroup();
      this.appendTranscript('\n[Plan approved — exited plan mode]\n');
    } else if (event.type === 'usage') {
      this.tokens += event.total;
    } else if (event.type === 'compaction') {
      this.flushExploreGroup();
      this.appendTranscript('\n[Context compacted]\n');
    } else if (event.type === 'stopped') {
      this.flushExploreGroup();
      this.appendTranscript(`\n${event.reason}\n`);
    }
    while (this.toolStates.size > 6) this.toolStates.delete(this.toolStates.keys().next().value!);
    this.schedule(16);
  }

  private formatToolActiveVerb(name: string, meta?: ToolEventMeta): string {
    if (meta?.kind === 'read') return 'Reading';
    if (meta?.kind === 'list') return 'Listing';
    if (meta?.kind === 'search') return 'Searching';
    if (meta?.kind === 'exec') return 'Running';
    if (meta?.kind === 'edit') return 'Editing';
    return `Running ${name}`;
  }

  private recordCompletedToolCell(name: string, state: 'done' | 'error', meta: ToolEventMeta) {
    if (meta.kind === 'read' || meta.kind === 'list' || meta.kind === 'search') {
      this.exploreGroup.push({ kind: meta.kind, summary: meta.summary });
      return;
    }
    this.flushExploreGroup();
    if (meta.kind === 'exec') {
      const dur = meta.durationMs !== undefined ? ` (${formatCompactDuration(meta.durationMs)})` : '';
      const header = `\n• ${state === 'error' ? 'Failed' : 'Ran'} ${meta.summary}${dur}`;
      const previewLines: string[] = [];
      if (meta.outputPreview?.length) {
        meta.outputPreview.forEach((line, i) => {
          previewLines.push(`  ${i === 0 ? '└' : ' '} ${line.slice(0, 120)}`);
        });
        if (meta.omittedLines) previewLines.push(`    … +${meta.omittedLines} lines`);
      } else {
        previewLines.push('  └ (completed)');
      }
      this.appendTranscript(`${header}\n${previewLines.join('\n')}\n`);
      return;
    }
    if (meta.kind === 'edit') {
      const verb = meta.verb ?? 'Edited';
      const stats = meta.added !== undefined || meta.removed !== undefined
        ? ` (+${meta.added ?? 0} -${meta.removed ?? 0})`
        : '';
      const diffLines = meta.diff ? formatCompactDiff(meta.diff, 12) : [];
      const body = diffLines.length ? `\n${diffLines.join('\n')}` : '';
      this.appendTranscript(`\n• ${verb} ${meta.summary}${stats}${body}\n`);
    }
  }

  setStatus(status: string) {
    this.status = status;
    const busy = status === 'Running' || status === 'Connecting MCP' || status === 'Compacting';
    if (status === 'Running') {
      this.outputChannel = undefined;
      this.turnStartedAt = Date.now();
      this.reasoningBuffer = '';
      this.reasoningHeader = undefined;
      this.reasoningDetail = undefined;
      this.activeToolStream = undefined;
    } else if (!busy) {
      this.flushExploreGroup();
      this.turnStartedAt = undefined;
      this.activeToolStream = undefined;
    }
    if (busy && !this.spinTimer && !this.closed) {
      this.spinTimer = setInterval(() => {
        this.spinTick = (this.spinTick + 1) % SPINNER.length;
        this.schedule(50);
      }, 100);
      this.spinTimer.unref();
    } else if (!busy && this.spinTimer) {
      clearInterval(this.spinTimer);
      this.spinTimer = undefined;
    }
    this.schedule();
  }

  question(label = 'goon> ', signal?: AbortSignal, remember = false): Promise<string> {
    if (this.closed) return Promise.reject(new Error('Terminal closed.'));
    if (this.pending) return Promise.reject(new Error('A terminal question is already active.'));
    signal?.throwIfAborted();
    this.label = terminalText(label);
    this.input = [];
    this.cursor = 0;
    this.historyIndex = this.history.length;
    this.remember = remember;
    const result = new Promise<string>((resolve, reject) => {
      const abort = () => this.reject(new Error('Input cancelled.'));
      this.pending = { resolve, reject, cleanup: () => signal?.removeEventListener('abort', abort) };
      signal?.addEventListener('abort', abort, { once: true });
    });
    this.schedule();
    return result;
  }

  private reject(error: Error) {
    const pending = this.pending;
    this.pending = undefined;
    pending?.cleanup();
    pending?.reject(error);
    this.schedule();
  }

  private keypress = (text: string | undefined, key: Key = {}) => {
    if (key.ctrl && key.name === 'c') {
      this.cancel();
      return;
    }
    if (key.name === 'escape' && (this.status === 'Running' || this.status === 'Compacting')) {
      this.cancel();
      return;
    }
    if (key.ctrl && key.name === 'l') {
      this.clear();
      return;
    }
    if (key.ctrl && key.name === 'd' && !this.input.length) {
      this.ended();
      return;
    }
    if (key.name === 'pageup') {
      this.scroll += Math.max(1, (this.target.rows || 24) - 8);
      this.schedule();
      return;
    }
    if (key.name === 'pagedown') {
      this.scroll = Math.max(0, this.scroll - Math.max(1, (this.target.rows || 24) - 8));
      this.schedule();
      return;
    }
    if ((key.name === 'tab' && key.shift) || key.sequence === '\x1b[Z') {
      this.onToggleMode?.();
      this.schedule();
      return;
    }
    if (key.meta && (text === '.' || key.sequence === '\x1b.')) {
      this.onCycleEffort?.(1);
      this.schedule();
      return;
    }
    if (key.meta && (text === ',' || key.sequence === '\x1b,')) {
      this.onCycleEffort?.(-1);
      this.schedule();
      return;
    }
    if (!this.pending) return;
    if (key.name === 'tab') {
      const current = this.input.join('');
      if (current.startsWith('/')) {
        const match = SLASH_COMMANDS.find(cmd => cmd.startsWith(current.trim()));
        if (match) {
          this.input = [...(match + ' ')];
          this.cursor = this.input.length;
          this.schedule();
        }
        return;
      }
      const atMatch = /(?:^|\s)@([A-Za-z0-9_./\\-]*)$/.exec(current);
      if (atMatch && this.workspaceFiles.length > 0) {
        const query = atMatch[1].toLowerCase();
        const found =
          this.workspaceFiles.find(f => f.toLowerCase().startsWith(query)) ??
          this.workspaceFiles.find(f => f.toLowerCase().includes(query));
        if (found) {
          const prefixLen = current.length - atMatch[1].length;
          this.input = [...(current.slice(0, prefixLen) + found + ' ')];
          this.cursor = this.input.length;
          this.schedule();
        }
      }
      return;
    }
    if (key.name === 'return' || key.name === 'enter') {
      const value = this.input.join('');
      const pending = this.pending;
      this.pending = undefined;
      pending.cleanup();
      if (this.remember && value.trim()) {
        this.history.push(value);
        if (this.history.length > 100) this.history.shift();
      }
      this.input = [];
      this.cursor = 0;
      this.scroll = 0;
      pending.resolve(value);
      this.schedule();
      return;
    }
    if (key.name === 'left') this.cursor = Math.max(0, this.cursor - 1);
    else if (key.name === 'right') this.cursor = Math.min(this.input.length, this.cursor + 1);
    else if (key.name === 'home' || (key.ctrl && key.name === 'a')) this.cursor = 0;
    else if (key.name === 'end' || (key.ctrl && key.name === 'e')) this.cursor = this.input.length;
    else if (key.name === 'backspace') {
      if (this.cursor) this.input.splice(--this.cursor, 1);
    } else if (key.name === 'delete') {
      this.input.splice(this.cursor, 1);
    } else if (key.ctrl && key.name === 'w') {
      while (this.cursor > 0 && this.input[this.cursor - 1] === ' ') this.input.splice(--this.cursor, 1);
      while (this.cursor > 0 && this.input[this.cursor - 1] !== ' ') this.input.splice(--this.cursor, 1);
    } else if ((key.name === 'up' || key.name === 'down') && this.remember) {
      this.historyIndex = Math.min(
        this.history.length,
        Math.max(0, this.historyIndex + (key.name === 'up' ? -1 : 1)),
      );
      this.input = [...(this.history[this.historyIndex] ?? '')];
      this.cursor = this.input.length;
    } else if (key.ctrl && key.name === 'u') {
      this.input = [];
      this.cursor = 0;
    } else if (text && !key.ctrl && !key.meta && this.input.length < 16_000) {
      const chars = [...terminalText(text).replace(/\n/g, ' ')];
      this.input.splice(this.cursor, 0, ...chars);
      this.cursor += chars.length;
    }
    this.schedule();
  };

  private ended = () => {
    this.cancel();
    this.close();
  };

  private resize = () => {
    this.lastRenderedFrame = '';
    this.schedule();
  };

  private schedule(delayMs = 16) {
    if (!this.closed && !this.timer) {
      this.timer = setTimeout(() => {
        this.timer = undefined;
        this.render();
      }, delayMs);
    }
  }

  /**
   * Context-aware Codex Markdown & Cell Styler.
   * Tracks code fences, reasoning blocks, and unified diff blocks across lines
   * so Markdown bullet lists (`- item`) are NEVER mis-colored as red diff lines.
   */
  private renderStyledBodyLines(columns: number, rich: boolean): string[] {
    const rawLines = this.transcript.split('\n');
    const wrapped: Array<{ text: string; block: 'normal' | 'reasoning' | 'code' | 'diff' | 'user' }> = [];
    let currentBlock: 'normal' | 'reasoning' | 'code' | 'diff' = 'normal';

    for (const rawLine of rawLines) {
      const trimmed = rawLine.trim();
      if (trimmed.startsWith('```')) {
        const lang = trimmed.slice(3).trim();
        if (currentBlock === 'code') {
          currentBlock = 'normal';
          wrapped.push({ text: '  ╰─', block: 'code' });
        } else {
          currentBlock = 'code';
          wrapped.push({ text: `  ╭─ ${lang || 'code'}`, block: 'code' });
        }
        continue;
      }
      if (rawLine.startsWith('[reasoning')) {
        currentBlock = 'reasoning';
      } else if (
        rawLine.startsWith('[answer]') ||
        rawLine.startsWith('• ') ||
        rawLine.startsWith('> ') ||
        rawLine.startsWith('› ') ||
        rawLine.startsWith('[todo ') ||
        rawLine.startsWith('[Plan ') ||
        rawLine.startsWith('[agent ')
      ) {
        if (currentBlock === 'reasoning' || currentBlock === 'diff') currentBlock = 'normal';
      }
      if (
        rawLine.startsWith('--- a/') ||
        rawLine.startsWith('+++ b/') ||
        rawLine.startsWith('@@ ') ||
        /^• (?:Edited|Added|Deleted) /.test(rawLine)
      ) {
        currentBlock = 'diff';
      } else if (
        currentBlock === 'diff' &&
        trimmed &&
        !rawLine.startsWith('+') &&
        !rawLine.startsWith('-') &&
        !/^\s*\d+\s+[+-]?\s*/.test(rawLine) &&
        !trimmed.startsWith('⋮') &&
        !trimmed.startsWith('…')
      ) {
        currentBlock = 'normal';
      }

      const isUser = rawLine.startsWith('> ') || rawLine.startsWith('› ');
      const lineBlock = isUser ? 'user' : currentBlock;
      let normalized = rawLine;
      if (lineBlock === 'normal') {
        // Transform markdown bullet list `- item` or `* item` into Codex `  • item`
        normalized = normalized.replace(/^(\s*)[-*]\s+/, '$1  • ');
      }
      const chunks = wrapTerminal(normalized, columns);
      for (const chunk of chunks) {
        wrapped.push({ text: chunk, block: lineBlock });
      }
    }

    if (!rich) return wrapped.map(w => w.text);
    return wrapped.map(({ text: line, block }) => this.styleLine(line, block, columns));
  }

  private styleLine(
    line: string,
    block: 'normal' | 'reasoning' | 'code' | 'diff' | 'user',
    columns: number,
  ): string {
    if (line.startsWith('╭') || line.startsWith('╰') || line.startsWith('│') || line.startsWith('  >_ ')) {
      return `\x1b[38;2;99;168;248m${line}\x1b[0m`;
    }
    if (block === 'user' || line.startsWith('> ') || line.startsWith('› ')) {
      const clean = line.replace(/^>\s/, '› ');
      return padRightAnsi(`\x1b[1;97m ${clean}`, columns, '\x1b[48;5;236m');
    }
    if (line.startsWith('[reasoning')) {
      return `\x1b[2;35m• Thinking ${line}\x1b[0m`;
    }
    if (block === 'reasoning') {
      return `\x1b[2;3m  │ ${line}\x1b[0m`;
    }
    if (line.startsWith('[answer]')) {
      return `\x1b[1;32m• Answer ${line}\x1b[0m`;
    }
    if (block === 'code') {
      if (line.startsWith('  ╭─') || line.startsWith('  ╰─')) return `\x1b[2;36m${line}\x1b[0m`;
      return `\x1b[38;2;180;210;255m  │ ${line}\x1b[0m`;
    }
    // Codex Explored / Ran / Edited cells
    if (/^• (?:Explored|Exploring)\b/.test(line)) {
      return `\x1b[2m•\x1b[0m \x1b[1m${line.slice(2)}\x1b[0m`;
    }
    if (/^• (?:Ran|Running)\b/.test(line)) {
      return `\x1b[1;32m•\x1b[0m \x1b[1mRan\x1b[0m\x1b[38;2;99;168;248m${line.slice(5)}\x1b[0m`;
    }
    if (/^• Failed\b/.test(line)) {
      return `\x1b[1;31m•\x1b[0m \x1b[1;31m${line.slice(2)}\x1b[0m`;
    }
    if (/^• (?:Edited|Added|Deleted)\b/.test(line)) {
      const coloredStats = line
        .replace(/\(\+(\d+)/, '(\x1b[32m+$1\x1b[0m')
        .replace(/-(\d+)\)/, '\x1b[31m-$1\x1b[0m)');
      return `\x1b[2m•\x1b[0m \x1b[1m${coloredStats.slice(2)}\x1b[0m`;
    }
    if (/^\s*(?:└| )\s+(?:Read|List|Search)\b/.test(line)) {
      return line
        .replace(/^(\s*└\s+)/, '\x1b[2m$1\x1b[0m')
        .replace(/\b(Read|List|Search)\b/, '\x1b[38;2;99;168;248m$1\x1b[0m');
    }
    if (line.startsWith('  └ ')) {
      return `\x1b[2m${line}\x1b[0m`;
    }
    if (block === 'diff') {
      if (line.startsWith('--- ') || line.startsWith('+++ ') || line.startsWith('@@ ')) {
        return `\x1b[36m${line}\x1b[0m`;
      }
      if (/^\s*\d+\s+\+/.test(line) || (line.startsWith('+') && !line.startsWith('+++'))) {
        return `\x1b[48;2;33;58;43m\x1b[32m${line}\x1b[0m`;
      }
      if (/^\s*\d+\s+-/.test(line) || (line.startsWith('-') && !line.startsWith('---'))) {
        return `\x1b[48;2;74;34;29m\x1b[31m${line}\x1b[0m`;
      }
      if (line.trim().startsWith('⋮') || line.trim().startsWith('…')) {
        return `\x1b[2m${line}\x1b[0m`;
      }
      return `\x1b[2m${line}\x1b[0m`;
    }
    if (line.startsWith('[todo ') || line.startsWith('[Plan ')) return `\x1b[1;36m${line}\x1b[0m`;
    if (line.startsWith('  [x] ')) return `\x1b[2;32m  ✔ ${line.slice(6)}\x1b[0m`;
    if (line.startsWith('  [>] ')) return `\x1b[1;38;2;99;168;248m  □ ${line.slice(6)}\x1b[0m`;
    if (line.startsWith('  [ ] ')) return `\x1b[2m  □ ${line.slice(6)}\x1b[0m`;
    if (line.startsWith('↺ Reverted')) return `\x1b[1;33m${line}\x1b[0m`;
    if (line.startsWith('[agent ')) return `\x1b[33m${line}\x1b[0m`;
    if (line.startsWith('Allow ')) return `\x1b[1;33m${line}\x1b[0m`;

    // Markdown headings
    const headingMatch = /^(#{1,4})\s+(.*)$/.exec(line.trimStart());
    if (headingMatch) {
      const level = headingMatch[1].length;
      const title = formatInlineMarkdown(headingMatch[2], '\x1b[1;38;2;99;168;248m');
      return level === 1
        ? `\x1b[1;4;38;2;99;168;248m${title}\x1b[0m`
        : `\x1b[1;38;2;99;168;248m${title}\x1b[0m`;
    }
    // Markdown table rows
    if (line.trimStart().startsWith('|') && line.trimEnd().endsWith('|')) {
      if (/^\|[\s:|-]+\|$/.test(line.trim())) {
        return `\x1b[2m  ${'─'.repeat(Math.min(columns - 4, line.trim().length))}\x1b[0m`;
      }
      const cells = line
        .trim()
        .slice(1, -1)
        .split('|')
        .map(c => formatInlineMarkdown(c.trim()));
      return `  \x1b[2m│\x1b[0m ${cells.join(' \x1b[2m│\x1b[0m ')} \x1b[2m│\x1b[0m`;
    }
    return formatInlineMarkdown(line);
  }

  private render() {
    if (this.closed) return;
    const columns = Math.max(4, this.target.columns || 80);
    const rows = Math.max(8, this.target.rows || 24);
    const clip = (s: string) => wrapTerminal(s, columns - 1)[0];
    const currentInput = this.input.join('');
    const slashMatches = currentInput.startsWith('/')
      ? SLASH_COMMANDS.filter(c => c.startsWith(currentInput.trim().split(/\s+/)[0]))
      : [];
    const busy = this.status === 'Running' || this.status === 'Connecting MCP' || this.status === 'Compacting';
    const indicator = busy ? SPINNER[this.spinTick % SPINNER.length] : '●';
    const rich = columns >= 48 && rows >= 12;
    const todoBadge = this.todoLabel ? ` · ${this.todoLabel}` : '';

    // Top Codex Bar
    const headerRaw = clip(`>_ goon · ${this.title} · [${this.modeLabel}] · effort:${this.effortLabel}${todoBadge}`);
    const header = rich
      ? `\x1b[1;38;2;99;168;248m>_\x1b[0m \x1b[1mgoon\x1b[0m\x1b[2m${headerRaw.slice(7)}\x1b[0m`
      : clip(`goon · ${this.title}`);

    // Codex Live Status Indicator / Slash Command Bar
    const elapsedStr = this.turnStartedAt ? `${formatCompactDuration(Date.now() - this.turnStartedAt)} • ` : '';
    const liveActivity = this.activeToolStream
      ? `Streaming ${this.activeToolStream.name}${this.activeToolStream.preview ? `: ${this.activeToolStream.preview}` : '…'}`
      : this.reasoningHeader
        ? `${this.reasoningHeader}${this.reasoningDetail ? ` — ${this.reasoningDetail}` : ''}`
        : [...this.toolStates.values()].slice(-2).join(' | ');

    let toolsText = '';
    if (slashMatches.length) {
      const firstCmd = slashMatches[0];
      const desc = SLASH_DESCRIPTIONS[firstCmd] ? ` (${SLASH_DESCRIPTIONS[firstCmd]})` : '';
      toolsText = `Commands (Tab): ${slashMatches.slice(0, 6).join('  ')}${desc}`;
    } else if (busy || this.activeToolStream) {
      const statusTitle = liveActivity || this.status;
      toolsText = `• ${statusTitle} (${elapsedStr}esc / ctrl+c to interrupt)`;
    } else {
      toolsText = [...this.toolStates.values()].slice(-2).join(' | ');
    }

    const toolsLine = rich && toolsText
      ? busy || this.activeToolStream
        ? `\x1b[1;38;2;99;168;248m${indicator}\x1b[0m \x1b[1m${clip(toolsText.slice(2))}\x1b[0m`
        : `\x1b[33m${clip(toolsText)}\x1b[0m`
      : clip(toolsText);

    const footerRaw = clip(
      `${indicator} ${this.status} · ${this.modeLabel} · effort:${this.effortLabel}${todoBadge} · ${this.tokens} tokens · Shift+Tab mode · Alt+. effort · /help · Ctrl+C cancel`,
    );
    const footer = rich
      ? `\x1b[2m${footerRaw}\x1b[0m`
      : clip(`${this.status} · ${this.tokens} tokens · Ctrl+C cancel · PgUp/PgDn scroll`);

    const all = this.renderStyledBodyLines(columns - 1, rich);
    const height = rows - 5;
    this.scroll = Math.min(this.scroll, Math.max(0, all.length - height));
    const end = all.length - this.scroll;
    const body = all.slice(Math.max(0, end - height), end);
    while (body.length < height) body.push('');

    const label = clip(this.label);
    let visible = this.input.slice(0, this.cursor).join('');
    while (visible && wrapTerminal(label + visible, columns - 1).length > 1) {
      visible = [...visible].slice(1).join('');
    }
    const afterCursor = this.input.slice(this.cursor).join('');
    const promptRaw = clip(label + visible + afterCursor);
    const isEmptyInput = this.input.length === 0 && this.label === 'goon> ';
    const placeholder = isEmptyInput && rich ? '\x1b[2mAsk anything… (/ commands, @ files, ! shell)\x1b[0m' : '';
    const prompt = rich
      ? `\x1b[1;38;2;99;168;248m${label}\x1b[0m${placeholder || clip(visible + afterCursor)}`
      : promptRaw;
    const cursorColumn = Math.min(columns, [...(label + visible)].reduce((n, c) => n + width(c), 0) + 1);

    const frame =
      `\x1b[?2026h\x1b[?25l\x1b[H\x1b[2J${[header, ...body, toolsLine, footer, prompt].join('\r\n')}` +
      (this.pending ? `\x1b[${rows - 1};${cursorColumn}H\x1b[?25h` : '') +
      `\x1b[?2026l`;

    if (frame !== this.lastRenderedFrame) {
      this.lastRenderedFrame = frame;
      this.target.write(frame);
    }
  }

  close() {
    if (this.closed) return;
    this.closed = true;
    clearTimeout(this.timer);
    if (this.spinTimer) {
      clearInterval(this.spinTimer);
      this.spinTimer = undefined;
    }
    this.reject(new Error('Terminal closed.'));
    this.source.removeListener('keypress', this.keypress);
    this.source.removeListener('end', this.ended);
    this.target.removeListener('resize', this.resize);
    this.source.setRawMode(this.oldRaw);
    this.source.pause();
    this.target.write('\x1b[?25h\x1b[?1049l');
  }
}
