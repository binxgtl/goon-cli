import { existsSync } from 'node:fs';
import { basename } from 'node:path';
import { emitKeypressEvents, type Key } from 'node:readline';
import type { ReadStream, WriteStream } from 'node:tty';
import { stripVTControlCharacters } from 'node:util';
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
  const maxCols = Math.max(1, columns);
  const lines = [''];
  let used = 0;
  for (const c of terminalText(text).replace(/\t/g, '  ')) {
    if (c === '\n') {
      lines.push('');
      used = 0;
      continue;
    }
    const size = width(c);
    if (used + size > maxCols) {
      lines.push('');
      used = 0;
    }
    lines[lines.length - 1] += c;
    used += size;
  }
  return lines;
}

function wrapWithIndent(text: string, columns: number, firstPrefix: string, contPrefix = firstPrefix): string[] {
  const firstPrefixCols = visibleLength(firstPrefix);
  const contPrefixCols = visibleLength(contPrefix);
  const firstAvail = Math.max(2, columns - firstPrefixCols);
  const contAvail = Math.max(2, columns - contPrefixCols);

  const out: string[] = [];
  const rawParagraphs = terminalText(text).replace(/\t/g, '  ').split('\n');
  for (const para of rawParagraphs) {
    if (!para) {
      out.push(firstPrefix.trimEnd());
      continue;
    }
    let current = '';
    let used = 0;
    let isFirst = true;
    for (const c of para) {
      const size = width(c);
      const limit = isFirst ? firstAvail : contAvail;
      if (used + size > limit && current) {
        out.push((isFirst ? firstPrefix : contPrefix) + current);
        current = '';
        used = 0;
        isFirst = false;
      }
      current += c;
      used += size;
    }
    if (current || isFirst) {
      out.push((isFirst ? firstPrefix : contPrefix) + current);
    }
  }
  return out.length ? out : [firstPrefix];
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
  // Highlight [Image #N...] pills in Codex Cyan/Blue badge style
  out = out.replace(/(\[Image #\d+[^\]]*\])/g, `\x1b[1;38;2;99;168;248m$1${baseSgr}`);
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

const LOCAL_IMAGE_PATH_REGEX =
  /(?:"((?:[A-Za-z]:[\\/]|\\\\|\/)[^"\r\n]+?\.(?:png|jpe?g|webp|gif))"|'((?:[A-Za-z]:[\\/]|\\\\|\/)[^'\r\n]+?\.(?:png|jpe?g|webp|gif))'|((?:[A-Za-z]:[\\/]|\\\\|\/)[^\s"']+?\.(?:png|jpe?g|webp|gif)))/gi;

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
  private maxScroll = 0;
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
  private pasteBurstTimer?: NodeJS.Timeout;
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
  private inBracketedPaste = false;
  private inSgrMouse = false;
  private sgrMouseBuf = '';
  private attachedImages = new Map<string, string>();
  private nextImageIdx = 1;
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
    // Enter Alternate Screen (\x1b[?1049h), enable SGR Mouse Wheel Tracking (\x1b[?1000h\x1b[?1006h) +
    // DEC Alternate Scroll (\x1b[?1007h), enable Bracketed Paste (\x1b[?2004h), and hide cursor (\x1b[?25l).
    target.write('\x1b[?1049h\x1b[?1000h\x1b[?1006h\x1b[?1007h\x1b[?2004h\x1b[?25l');
    this.render();
  }

  clear() {
    this.transcript = '';
    this.scroll = 0;
    this.maxScroll = 0;
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
    let cleaned = text;
    // Clean up raw @"C:\...\image.png" in user prompt echo so transcript shows [Image #1: name.png]
    cleaned = cleaned.replace(/@"([^"]+\.(?:png|jpe?g|webp|gif))"/gi, (_, p: string) => `[Image: ${basename(p)}]`);
    const isUserPrompt = cleaned.trimStart().startsWith('> ') || cleaned.trimStart().startsWith('› ');
    if (isUserPrompt) this.flushExploreGroup();
    this.appendTranscript(cleaned, isUserPrompt);
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
        const icon = t.status === 'completed' ? '[x]' : t.status === 'in_progress' ? '[>]' : '[ ]';
        return `  ${icon} ${t.content}`;
      });
      if (lines.length) {
        this.appendTranscript(`\n[todo ${done}/${event.todos.length}]\n${lines.join('\n')}\n`);
      }
    } else if (event.type === 'plan_exit') {
      this.flushExploreGroup();
      this.appendTranscript('\n[Plan approved — exited plan mode]\n');
    } else if (event.type === 'usage') {
      this.tokens += event.total;
    } else if (event.type === 'compaction') {
      this.flushExploreGroup();
      this.appendTranscript(`\n[context compacted #${event.count}]\n`);
    } else if (event.type === 'stopped') {
      this.flushExploreGroup();
      this.appendTranscript(`\n${event.reason}\n`);
    }
    this.schedule(16);
  }

  private formatToolActiveVerb(name: string, meta?: ToolEventMeta): string {
    if (meta?.kind === 'read') return 'Reading';
    if (meta?.kind === 'list') return 'Listing';
    if (meta?.kind === 'search') return 'Searching';
    if (meta?.kind === 'exec') return 'Running';
    if (meta?.kind === 'edit') return 'Editing';
    return `Calling ${name}`;
  }

  private recordCompletedToolCell(name: string, state: 'done' | 'error', meta: ToolEventMeta) {
    if (state === 'done' && (meta.kind === 'read' || meta.kind === 'list' || meta.kind === 'search')) {
      this.exploreGroup.push({ kind: meta.kind, summary: meta.summary });
      if (this.exploreGroup.length >= 6) this.flushExploreGroup();
      return;
    }
    this.flushExploreGroup();
    if (meta.kind === 'exec') {
      const dur = meta.durationMs !== undefined ? ` (${formatCompactDuration(meta.durationMs)})` : '';
      const header = state === 'error' ? `• Failed ${meta.summary}${dur}` : `• Ran ${meta.summary}${dur}`;
      const previewLines: string[] = [];
      if (meta.outputPreview?.length) {
        for (const [idx, line] of meta.outputPreview.entries()) {
          previewLines.push(`  ${idx === 0 ? '└' : ' '} ${line}`);
        }
        if (meta.omittedLines && meta.omittedLines > 0) {
          previewLines.push(`    … +${meta.omittedLines} lines`);
        }
      } else {
        previewLines.push('  └ (no output)');
      }
      this.appendTranscript(`\n${header}\n${previewLines.join('\n')}\n`);
      return;
    }
    if (meta.kind === 'edit') {
      const verb = meta.verb ?? 'Edited';
      const stats =
        meta.added !== undefined || meta.removed !== undefined
          ? ` (+${meta.added ?? 0} -${meta.removed ?? 0})`
          : '';
      const diffLines = meta.diff ? formatCompactDiff(meta.diff) : [];
      const diffBlock = diffLines.length ? `\n${diffLines.join('\n')}` : '';
      this.appendTranscript(`\n• ${verb} ${meta.summary}${stats}${diffBlock}\n`);
      return;
    }
    if (name !== 'todo_write') {
      const dur = meta.durationMs !== undefined ? ` (${formatCompactDuration(meta.durationMs)})` : '';
      this.appendTranscript(`\n• ${state === 'error' ? 'Failed' : 'Completed'} ${meta.summary}${dur}\n`);
    }
  }

  setStatus(status: string) {
    this.status = terminalText(status);
    const busy = this.status === 'Running' || this.status === 'Connecting MCP' || this.status === 'Compacting';
    if (busy) {
      if (!this.turnStartedAt) this.turnStartedAt = Date.now();
      if (!this.spinTimer) {
        this.spinTimer = setInterval(() => {
          this.spinTick = (this.spinTick + 1) % SPINNER.length;
          this.schedule();
        }, 80);
        this.spinTimer.unref?.();
      }
    } else {
      this.flushExploreGroup();
      this.turnStartedAt = undefined;
      this.reasoningBuffer = '';
      this.reasoningHeader = undefined;
      this.reasoningDetail = undefined;
      this.activeToolStream = undefined;
      if (this.spinTimer) {
        clearInterval(this.spinTimer);
        this.spinTimer = undefined;
      }
    }
    this.outputChannel = undefined;
    this.reasoningSource = undefined;
    if (status === 'Ready') this.toolStates.clear();
    this.schedule();
  }

  question(label = 'goon> ', signal?: AbortSignal, remember = false): Promise<string> {
    if (this.closed) return Promise.reject(new Error('Terminal closed.'));
    if (this.pending) return Promise.reject(new Error('A terminal question is already active.'));
    signal?.throwIfAborted();
    this.label = terminalText(label);
    this.input = [];
    this.cursor = 0;
    this.attachedImages.clear();
    this.nextImageIdx = 1;
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

  /**
   * Detects any pasted local image paths (such as Windows ScreenClip paths) inside `this.input`
   * and collapses them into Codex-style `[Image #1]` atomic tokens while recording the full path.
   */
  private collapsePastedImagePathsInInput(): boolean {
    const current = this.input.join('');
    if (!current || !/\.(?:png|jpe?g|webp|gif)/i.test(current)) return false;

    let changed = false;
    // Check if the entire input (trimmed/unquoted) is a single existing image path (including paths with spaces)
    const trimmed = current.trim();
    const unquoted =
      (trimmed.startsWith('"') && trimmed.endsWith('"')) || (trimmed.startsWith("'") && trimmed.endsWith("'"))
        ? trimmed.slice(1, -1).trim()
        : trimmed;
    if (
      /\.(?:png|jpe?g|webp|gif)$/i.test(unquoted) &&
      (/^[A-Za-z]:[\\/]/.test(unquoted) || unquoted.startsWith('\\\\') || unquoted.startsWith('/')) &&
      existsSync(unquoted)
    ) {
      const placeholder = `[Image #${this.nextImageIdx++}]`;
      this.attachedImages.set(placeholder, unquoted);
      const next = `${placeholder} `;
      this.input = [...next];
      this.cursor = this.input.length;
      return true;
    }

    // Check inline pasted image paths (e.g. C:\Users\...\ScreenClip\{GUID}.png)
    const replaced = current.replace(LOCAL_IMAGE_PATH_REGEX, (fullMatch, qDouble, qSingle, bare) => {
      const candidate = (qDouble || qSingle || bare || '').trim();
      if (!candidate || !existsSync(candidate)) return fullMatch;
      changed = true;
      const placeholder = `[Image #${this.nextImageIdx++}]`;
      this.attachedImages.set(placeholder, candidate);
      return `${placeholder} `;
    });

    if (changed) {
      this.input = [...replaced];
      this.cursor = this.input.length;
    }
    return changed;
  }

  private schedulePasteBurstCheck() {
    if (this.pasteBurstTimer) clearTimeout(this.pasteBurstTimer);
    this.pasteBurstTimer = setTimeout(() => {
      this.pasteBurstTimer = undefined;
      if (this.collapsePastedImagePathsInInput()) {
        this.schedule(0);
      }
    }, 35);
    this.pasteBurstTimer.unref?.();
  }

  /**
   * Reads an image (or file drop / text) directly from the OS clipboard when the user presses Alt+V
   * or when Ctrl+V sends raw \x16 instead of terminal-pasted text (Codex `clipboard_paste.rs` pattern).
   */
  private async pasteFromSystemClipboard(): Promise<void> {
    if (process.platform !== 'win32' || !this.pending) return;
    try {
      const { runCommand } = await import('./process.js');
      const psScript = [
        '[Console]::OutputEncoding = [System.Text.Encoding]::UTF8;',
        'Add-Type -AssemblyName System.Windows.Forms;',
        'Add-Type -AssemblyName System.Drawing;',
        'if ([System.Windows.Forms.Clipboard]::ContainsImage()) {',
        '  $img = [System.Windows.Forms.Clipboard]::GetImage();',
        "  $p = Join-Path ([System.IO.Path]::GetTempPath()) ('goon-clip-' + [guid]::NewGuid().ToString('N').Substring(0,8) + '.png');",
        '  $img.Save($p, [System.Drawing.Imaging.ImageFormat]::Png);',
        "  Write-Output ('IMG:' + $p);",
        '} elseif ([System.Windows.Forms.Clipboard]::ContainsFileDropList()) {',
        '  $files = [System.Windows.Forms.Clipboard]::GetFileDropList();',
        "  if ($files.Count -gt 0) { Write-Output ('FILE:' + $files[0]); }",
        '} elseif ([System.Windows.Forms.Clipboard]::ContainsText()) {',
        "  Write-Output ('TXT:' + [System.Windows.Forms.Clipboard]::GetText());",
        '}',
      ].join(' ');
      const res = await runCommand('powershell.exe', ['-NoProfile', '-NonInteractive', '-Sta', '-Command', psScript], {
        cwd: process.cwd(),
        signal: AbortSignal.timeout(4000),
        timeoutMs: 4000,
      });
      if (res.code !== 0 || !res.output.trim() || !this.pending) return;
      const out = res.output.trim();
      if (out.startsWith('IMG:') || out.startsWith('FILE:')) {
        const filePath = out.replace(/^(?:IMG|FILE):/, '').trim();
        if (/\.(?:png|jpe?g|webp|gif)$/i.test(filePath) && existsSync(filePath)) {
          const placeholder = `[Image #${this.nextImageIdx++}]`;
          this.attachedImages.set(placeholder, filePath);
          const chars = [...`${placeholder} `];
          this.input.splice(this.cursor, 0, ...chars);
          this.cursor += chars.length;
          this.schedule(0);
          return;
        }
      }
      if (out.startsWith('TXT:')) {
        const txt = terminalText(out.slice(4)).replace(/\r?\n/g, ' ');
        const chars = [...txt];
        this.input.splice(this.cursor, 0, ...chars);
        this.cursor += chars.length;
        this.collapsePastedImagePathsInInput();
        this.schedule(0);
      }
    } catch {
      // Ignore clipboard helper errors
    }
  }

  private scrollBy(deltaLines: number) {
    if (deltaLines === 0) return;
    this.scroll = Math.max(0, Math.min(this.maxScroll, this.scroll + deltaLines));
    this.schedule(0);
  }

  private keypress = (text: string | undefined, key: Key = {}) => {
    // 1. Filter SGR mouse sequences (\x1b[<btn;col;rowM/m) so mouse events never leak into input
    if (key.sequence === '\x1b[<') {
      this.inSgrMouse = true;
      this.sgrMouseBuf = '';
      return;
    }
    if (this.inSgrMouse) {
      const ch = text || key.sequence || '';
      this.sgrMouseBuf += ch;
      if (ch === 'M' || ch === 'm' || this.sgrMouseBuf.length > 24 || !/^[0-9;Mm]+$/.test(this.sgrMouseBuf)) {
        const m = /^(\d+);\d+;\d+[Mm]$/.exec(this.sgrMouseBuf);
        this.inSgrMouse = false;
        this.sgrMouseBuf = '';
        if (m) {
          const btn = Number(m[1]);
          if ((btn & 64) !== 0) {
            if ((btn & 3) === 0) this.scrollBy(3);
            else if ((btn & 3) === 1) this.scrollBy(-3);
          }
        }
      }
      return;
    }

    // 2. Bracketed paste mode boundaries (\x1b[200~ ... \x1b[201~)
    if (key.name === 'paste-start') {
      this.inBracketedPaste = true;
      return;
    }
    if (key.name === 'paste-end') {
      this.inBracketedPaste = false;
      this.collapsePastedImagePathsInInput();
      this.schedule(0);
      return;
    }

    if (!this.inBracketedPaste) {
      if (key.ctrl && key.name === 'c') {
        this.cancel();
        return;
      }
      if (key.name === 'escape') {
        if (this.status === 'Running' || this.status === 'Compacting') {
          this.cancel();
          return;
        }
        if (this.scroll > 0) {
          this.scroll = 0;
          this.schedule(0);
          return;
        }
      }
      if (key.ctrl && key.name === 'l') {
        this.clear();
        return;
      }
      if (key.ctrl && key.name === 'd' && !this.input.length) {
        this.ended();
        return;
      }
      // PageUp / PageDown & Shift+Up / Shift+Down always scroll the transcript
      if (key.name === 'pageup' || (key.name === 'up' && (key.shift || key.ctrl))) {
        const step = key.name === 'pageup' ? Math.max(2, (this.target.rows || 24) - 8) : 4;
        this.scrollBy(step);
        return;
      }
      if (key.name === 'pagedown' || (key.name === 'down' && (key.shift || key.ctrl))) {
        const step = key.name === 'pagedown' ? Math.max(2, (this.target.rows || 24) - 8) : 4;
        this.scrollBy(-step);
        return;
      }
      // Plain Up / Down arrow (also emitted by Windows Terminal Mouse Wheel via \x1b[?1007h):
      // Scroll transcript when agent is busy, when already scrolled up, or when transcript overflows and input is empty.
      if (key.name === 'up' || key.name === 'down') {
        const dir = key.name === 'up' ? 2 : -2;
        if (!this.pending) {
          this.scrollBy(dir);
          return;
        }
        if (this.scroll > 0) {
          this.scrollBy(dir);
          return;
        }
        if (
          key.name === 'up' &&
          this.maxScroll > 0 &&
          this.input.length === 0 &&
          this.historyIndex === this.history.length
        ) {
          this.scrollBy(2);
          return;
        }
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
      // Direct clipboard paste (Ctrl+V raw \x16 or Alt+V)
      if ((key.ctrl && key.name === 'v') || (key.meta && key.name === 'v')) {
        void this.pasteFromSystemClipboard();
        return;
      }
    }

    if (!this.pending) return;

    if (!this.inBracketedPaste && key.name === 'tab') {
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

    if (!this.inBracketedPaste && (key.name === 'return' || key.name === 'enter')) {
      this.collapsePastedImagePathsInInput();
      let value = this.input.join('');
      // Expand any [Image #N] placeholders to @"<path>" so expandFileMentions attaches them
      for (const [placeholder, fullPath] of this.attachedImages.entries()) {
        if (value.includes(placeholder)) {
          value = value.split(placeholder).join(`@"${fullPath}"`);
        }
      }
      const pending = this.pending;
      this.pending = undefined;
      pending.cleanup();
      if (this.remember && value.trim()) {
        this.history.push(this.input.join(''));
        if (this.history.length > 100) this.history.shift();
      }
      this.input = [];
      this.cursor = 0;
      this.scroll = 0;
      this.attachedImages.clear();
      pending.resolve(value);
      this.schedule();
      return;
    }

    if (!this.inBracketedPaste) {
      if (key.name === 'left') {
        this.cursor = Math.max(0, this.cursor - 1);
      } else if (key.name === 'right') {
        this.cursor = Math.min(this.input.length, this.cursor + 1);
      } else if (key.name === 'home' || (key.ctrl && key.name === 'a')) {
        this.cursor = 0;
      } else if (key.name === 'end' || (key.ctrl && key.name === 'e')) {
        if (this.scroll > 0 && this.cursor === this.input.length) {
          this.scroll = 0;
        } else {
          this.cursor = this.input.length;
        }
      } else if (key.name === 'backspace') {
        if (this.cursor > 0) {
          // Atomic deletion of [Image #N] placeholder if cursor is right after it
          const before = this.input.slice(0, this.cursor).join('');
          const imgMatch = /(\[Image #\d+\]\s?)$/.exec(before);
          if (imgMatch) {
            const token = imgMatch[1].trim();
            this.attachedImages.delete(token);
            const delLen = [...imgMatch[1]].length;
            this.input.splice(this.cursor - delLen, delLen);
            this.cursor -= delLen;
          } else {
            this.input.splice(--this.cursor, 1);
          }
        }
      } else if (key.name === 'delete') {
        this.input.splice(this.cursor, 1);
      } else if (key.ctrl && key.name === 'w') {
        while (this.cursor > 0 && this.input[this.cursor - 1] === ' ') this.input.splice(--this.cursor, 1);
        while (this.cursor > 0 && this.input[this.cursor - 1] !== ' ') this.input.splice(--this.cursor, 1);
      } else if (
        (key.name === 'up' || key.name === 'down' || (key.ctrl && (key.name === 'p' || key.name === 'n'))) &&
        this.remember
      ) {
        const delta = key.name === 'up' || key.name === 'p' ? -1 : 1;
        this.historyIndex = Math.min(this.history.length, Math.max(0, this.historyIndex + delta));
        this.input = [...(this.history[this.historyIndex] ?? '')];
        this.cursor = this.input.length;
      } else if (key.ctrl && key.name === 'u') {
        if (this.input.length > 0) {
          this.input = [];
          this.cursor = 0;
          this.attachedImages.clear();
        } else {
          this.scrollBy(5);
        }
      } else if (text && !key.ctrl && !key.meta && this.input.length < 16_000) {
        const chars = [...terminalText(text).replace(/\n/g, ' ')];
        this.input.splice(this.cursor, 0, ...chars);
        this.cursor += chars.length;
        this.scroll = 0;
        const lastChar = chars[chars.length - 1] ?? '';
        if (chars.length > 1 || /[gfpe"']/i.test(lastChar)) {
          this.schedulePasteBurstCheck();
        }
      }
      this.schedule();
      return;
    }

    // Inside bracketed paste: buffer characters rapidly
    if (text && this.input.length < 16_000) {
      const chars = [...terminalText(text).replace(/\r?\n/g, ' ')];
      this.input.splice(this.cursor, 0, ...chars);
      this.cursor += chars.length;
    }
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
   * Applies all line prefixes BEFORE wrapping to `columns`, guaranteeing that NO line
   * can ever exceed the terminal width or cause terminal soft-wrap cursor drift.
   */
  private renderStyledBodyLines(columns: number, rich: boolean): string[] {
    const safeCols = Math.max(4, columns);
    const rawLines = this.transcript.split('\n');
    const wrapped: Array<{ text: string; block: 'normal' | 'reasoning' | 'code' | 'diff' | 'user' }> = [];
    let currentBlock: 'normal' | 'reasoning' | 'code' | 'diff' = 'normal';

    for (const rawLine of rawLines) {
      const trimmed = rawLine.trim();
      if (trimmed.startsWith('```')) {
        const lang = trimmed.slice(3).trim();
        if (currentBlock === 'code') {
          currentBlock = 'normal';
          wrapped.push({ text: wrapTerminal('  ╰─', safeCols)[0], block: 'code' });
        } else {
          currentBlock = 'code';
          wrapped.push({ text: wrapTerminal(`  ╭─ ${lang || 'code'}`, safeCols)[0], block: 'code' });
        }
        continue;
      }
      if (rawLine.startsWith('[reasoning')) {
        currentBlock = 'reasoning';
        for (const chunk of wrapTerminal(`• Thinking ${rawLine}`, safeCols)) {
          wrapped.push({ text: chunk, block: 'reasoning' });
        }
        continue;
      }
      if (rawLine.startsWith('[answer]')) {
        currentBlock = 'normal';
        for (const chunk of wrapTerminal(`• Answer ${rawLine}`, safeCols)) {
          wrapped.push({ text: chunk, block: 'normal' });
        }
        continue;
      }
      if (
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

      if (lineBlock === 'user') {
        const cleanUser = rawLine.replace(/^>\s/, '› ');
        for (const chunk of wrapWithIndent(cleanUser, safeCols, ' ', '   ')) {
          wrapped.push({ text: chunk, block: 'user' });
        }
        continue;
      }

      if (lineBlock === 'reasoning' || lineBlock === 'code') {
        for (const chunk of wrapWithIndent(rawLine, safeCols, '  │ ', '  │ ')) {
          wrapped.push({ text: chunk, block: lineBlock });
        }
        continue;
      }

      if (lineBlock === 'normal') {
        const bulletMatch = /^(\s*)[-*]\s+(.*)$/.exec(rawLine);
        if (bulletMatch) {
          const indent = bulletMatch[1];
          const content = bulletMatch[2];
          for (const chunk of wrapWithIndent(content, safeCols, `${indent}  • `, `${indent}    `)) {
            wrapped.push({ text: chunk, block: 'normal' });
          }
          continue;
        }
      }

      for (const chunk of wrapTerminal(rawLine, safeCols)) {
        wrapped.push({ text: chunk, block: lineBlock });
      }
    }

    if (!rich) return wrapped.map(w => w.text);
    return wrapped.map(({ text: line, block }) => this.styleLine(line, block, safeCols));
  }

  private styleLine(
    line: string,
    block: 'normal' | 'reasoning' | 'code' | 'diff' | 'user',
    columns: number,
  ): string {
    if (line.startsWith('╭') || line.startsWith('╰') || line.startsWith('│') || line.startsWith('  >_ ')) {
      return `\x1b[38;2;99;168;248m${line}\x1b[0m`;
    }
    if (block === 'user') {
      return padRightAnsi(`\x1b[1;97m${formatInlineMarkdown(line, '\x1b[1;97m\x1b[48;5;236m')}`, columns, '\x1b[48;5;236m');
    }
    if (line.startsWith('• Thinking [reasoning')) {
      return `\x1b[2;35m${line}\x1b[0m`;
    }
    if (block === 'reasoning') {
      return `\x1b[2;3m${line}\x1b[0m`;
    }
    if (line.startsWith('• Answer [answer]')) {
      return `\x1b[1;32m${line}\x1b[0m`;
    }
    if (block === 'code') {
      if (line.startsWith('  ╭─') || line.startsWith('  ╰─')) return `\x1b[2;36m${line}\x1b[0m`;
      return `\x1b[38;2;180;210;255m${line}\x1b[0m`;
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
      return wrapTerminal(`  │ ${cells.join(' │ ')} │`, columns)[0];
    }
    return formatInlineMarkdown(line);
  }

  private render() {
    if (this.closed) return;
    const columns = Math.max(4, this.target.columns || 80);
    const rows = Math.max(8, this.target.rows || 24);
    const safeCols = Math.max(3, columns - 1);
    const clip = (s: string) => wrapTerminal(s, safeCols)[0];
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

    // Body lines with strict width bound
    const all = this.renderStyledBodyLines(safeCols, rich);
    const height = rows - 5;
    this.maxScroll = Math.max(0, all.length - height);
    this.scroll = Math.min(this.scroll, this.maxScroll);
    const end = all.length - this.scroll;
    const body = all.slice(Math.max(0, end - height), end);
    while (body.length < height) body.push('');

    // Codex Live Status Indicator / Slash Command / Attached Images / Scroll Indicator Bar
    const elapsedStr = this.turnStartedAt ? `${formatCompactDuration(Date.now() - this.turnStartedAt)} • ` : '';
    const liveActivity = this.activeToolStream
      ? `Streaming ${this.activeToolStream.name}${this.activeToolStream.preview ? `: ${this.activeToolStream.preview}` : '…'}`
      : this.reasoningHeader
        ? `${this.reasoningHeader}${this.reasoningDetail ? ` — ${this.reasoningDetail}` : ''}`
        : [...this.toolStates.values()].slice(-2).join(' | ');

    let toolsText = '';
    if (this.scroll > 0) {
      toolsText = `↑ Scrolled up ${this.scroll}/${this.maxScroll} lines (Wheel / ↑↓ scroll · End / Esc to bottom)`;
    } else if (slashMatches.length) {
      const firstCmd = slashMatches[0];
      const desc = SLASH_DESCRIPTIONS[firstCmd] ? ` (${SLASH_DESCRIPTIONS[firstCmd]})` : '';
      toolsText = `Commands (Tab): ${slashMatches.slice(0, 6).join('  ')}${desc}`;
    } else if (this.attachedImages.size > 0 && !busy) {
      const items = [...this.attachedImages.entries()].map(([k, p]) => `${k}: ${basename(p)}`).join(' · ');
      toolsText = `📎 Attached ${items} (Enter to send · Backspace to remove)`;
    } else if (busy || this.activeToolStream) {
      const statusTitle = liveActivity || this.status;
      toolsText = `• ${statusTitle} (${elapsedStr}esc / ctrl+c to interrupt)`;
    } else {
      toolsText = [...this.toolStates.values()].slice(-2).join(' | ');
    }

    const toolsLine = rich && toolsText
      ? this.scroll > 0
        ? `\x1b[1;36m${clip(toolsText)}\x1b[0m`
        : busy || this.activeToolStream
          ? `\x1b[1;38;2;99;168;248m${indicator}\x1b[0m \x1b[1m${clip(toolsText.slice(2))}\x1b[0m`
          : `\x1b[33m${clip(toolsText)}\x1b[0m`
      : clip(toolsText);

    const footerRaw = clip(
      `${indicator} ${this.status} · ${this.modeLabel} · effort:${this.effortLabel}${todoBadge} · ${this.tokens} tokens · ↑↓/Wheel scroll · Shift+Tab mode · Alt+V paste img`,
    );
    const footer = rich
      ? `\x1b[2m${footerRaw}\x1b[0m`
      : clip(`${this.status} · ${this.tokens} tokens · Ctrl+C cancel · PgUp/PgDn scroll`);

    // Composer prompt with strict width math so `label + input` NEVER exceeds `safeCols`
    const label = clip(this.label);
    const labelCols = visibleLength(label);
    const inputAvailCols = Math.max(1, safeCols - labelCols);

    let visibleBeforeCursor = this.input.slice(0, this.cursor).join('');
    while (visibleBeforeCursor && visibleLength(visibleBeforeCursor) >= inputAvailCols) {
      visibleBeforeCursor = [...visibleBeforeCursor].slice(1).join('');
    }
    const afterCursor = this.input.slice(this.cursor).join('');
    const shownInput = wrapTerminal(visibleBeforeCursor + afterCursor, inputAvailCols)[0];
    const isEmptyInput = this.input.length === 0 && this.label === 'goon> ';
    const placeholder = isEmptyInput && rich
      ? wrapTerminal('Ask anything… (/ commands, @ files, Ctrl+V / Alt+V paste image)', inputAvailCols)[0]
      : '';
    const styledShownInput = formatInlineMarkdown(shownInput);
    const prompt = rich
      ? `\x1b[1;38;2;99;168;248m${label}\x1b[0m${placeholder ? `\x1b[2m${placeholder}\x1b[0m` : styledShownInput}`
      : wrapTerminal(label + shownInput, safeCols)[0];

    const cursorColumn = Math.min(columns, labelCols + visibleLength(visibleBeforeCursor) + 1);

    // Position cursor directly on the current (`prompt`) line via \r\x1b[${cursorColumn}G
    // so it is mathematically impossible for the cursor to land on the wrong row.
    const frame =
      `\x1b[?2026h\x1b[?25l\x1b[H\x1b[2J${[header, ...body, toolsLine, footer, prompt].join('\r\n')}` +
      (this.pending ? `\r\x1b[${cursorColumn}G\x1b[?25h` : '') +
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
    if (this.pasteBurstTimer) clearTimeout(this.pasteBurstTimer);
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
    this.target.write('\x1b[?2004l\x1b[?1007l\x1b[?1006l\x1b[?1000l\x1b[?25h\x1b[?1049l');
  }
}
