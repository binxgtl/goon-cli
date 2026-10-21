import { existsSync } from 'node:fs';
import { basename } from 'node:path';
import { emitKeypressEvents, type Key } from 'node:readline';
import type { ReadStream, WriteStream } from 'node:tty';
import { stripVTControlCharacters } from 'node:util';
import type { HarnessEvent, ToolEventMeta } from './harness.js';
import type { SubagentEvent } from './subagents.js';

const SPINNER = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

const fg = (r: number, g: number, b: number) => `\x1b[38;2;${r};${g};${b}m`;
const bgc = (r: number, g: number, b: number) => `\x1b[48;2;${r};${g};${b}m`;

/**
 * goon palette (truecolor, tuned for dark terminals). Violet is the brand/assistant voice, sky marks
 * things you can act on (commands, paths, code), mint/rose/amber carry status, slate carries chrome.
 */
export const THEME = {
  reset: '\x1b[0m',
  brand: fg(167, 139, 250),
  brandBold: `\x1b[1m${fg(167, 139, 250)}`,
  thinking: fg(150, 135, 200),
  bullet: fg(129, 120, 190),
  text: fg(226, 232, 240),
  strong: `\x1b[1m${fg(241, 245, 249)}`,
  muted: fg(148, 163, 184),
  faint: fg(100, 116, 139),
  info: fg(125, 211, 252),
  ok: fg(74, 222, 128),
  err: fg(251, 113, 133),
  warn: fg(251, 191, 36),
  codeText: fg(203, 213, 225),
  bandBg: bgc(24, 26, 36),
  userBg: bgc(36, 38, 54),
  codeBg: bgc(22, 24, 33),
  addBg: bgc(18, 52, 35),
  addFg: fg(134, 239, 172),
  delBg: bgc(66, 24, 32),
  delFg: fg(253, 164, 175),
} as const;

/** Single-column glyph vocabulary for transcript cells (none are Extended_Pictographic, so width() = 1). */
export const GLYPH = {
  brand: '◆',
  answer: '◆',
  user: '›',
  thinking: '∴',
  explored: '○',
  ran: '●',
  done: '●',
  failed: '✗',
  edit: '±',
  agent: '◈',
  todo: '≡',
  allow: '▲',
  compact: '◌',
  resumed: '↻',
  reverted: '↺',
  prompt: '◇',
} as const;

const CELL_RE = /^([○●✗±◈≡▲◌↻↺◇]) (\S.*)$/u;
const CELL_TONES: Record<string, string> = {
  '○': THEME.muted,
  '●': THEME.ok,
  '✗': THEME.err,
  '±': THEME.info,
  '◈': THEME.brand,
  '≡': THEME.brand,
  '▲': THEME.warn,
  '◌': THEME.faint,
  '↻': THEME.info,
  '↺': THEME.warn,
  '◇': THEME.brand,
};

/** Private-use code point marking user-turn lines inside the plain-text transcript. */
const USER_MARK = '\uE000';

type Seg = [text: string, sgr: string];
type RowKind =
  | 'blank' | 'user' | 'answer' | 'normal' | 'heading' | 'quote' | 'rule' | 'table'
  | 'reasoningHead' | 'reasoning' | 'codeHead' | 'code' | 'codeFoot'
  | 'cell' | 'child' | 'diff' | 'diffMeta' | 'box' | 'hint';
interface Row {
  kind: RowKind;
  pre: string;
  text: string;
  glyph?: string;
  sign?: '+' | '-' | ' ';
  level?: number;
}

export function formatTokenCount(n: number): string {
  if (!Number.isFinite(n) || n < 1000) return String(Math.max(0, Math.round(n || 0)));
  if (n < 1_000_000) return `${(n / 1000).toFixed(n < 10_000 ? 1 : 0).replace(/\.0$/, '')}k`;
  return `${(n / 1_000_000).toFixed(1).replace(/\.0$/, '')}M`;
}
export const SLASH_COMMANDS = [
  '/help', '/setup', '/model', '/effort', '/mode', '/auto', '/yolo', '/view', '/transcript',
  '/copy', '/btw', '/side', '/goal', '/recap', '/skills', '/memories', '/presets', '/mention',
  '/cd', '/pwd', '/mcp', '/subagents', '/agent', '/plan', '/revert', '/undo',
  '/checkpoints', '/todo', '/resume', '/new', '/delete', '/compact', '/diff', '/review', '/fork', '/rename',
  '/export', '/init', '/status', '/tools', '/sessions', '/debug-config', '/warnings', '/doctor', '/clear', '/exit',
];

const SLASH_DESCRIPTIONS: Record<string, string> = {
  '/help': 'show commands & shortcuts',
  '/model': 'switch model & auto-sync limits',
  '/effort': 'set reasoning effort (low..max)',
  '/auto': 'toggle auto-approve mode for writes & shell (or /mode auto)',
  '/yolo': 'enable auto-approve mode for all tools',
  '/view': 'switch view mode: normal | verbose | transcript',
  '/transcript': 'toggle full transcript mode (or Ctrl+T)',
  '/copy': 'copy last assistant response or code block to clipboard (or Ctrl+O)',
  '/btw': 'ask an ephemeral side question without modifying history',
  '/side': 'ask an ephemeral side question (/btw alias)',
  '/goal': 'set or inspect session objective & token tracking',
  '/recap': 'summarize current session progress without compacting',
  '/skills': 'list or inspect SKILL.md skills (use $skill-name in prompt)',
  '/memories': 'manage persistent user memories across sessions',
  '/presets': 'save or run reusable prompt presets',
  '/mention': 'attach @file mention into the composer',
  '/cd': 'change active workspace directory',
  '/pwd': 'show active workspace directory',
  '/mcp': 'list, check, add, remove, reload, or import-codex MCP servers',
  '/subagents': 'list spawned subagents, inspect transcripts, or run a subagent task',
  '/agent': 'spawn a subagent task or inspect subagents (/subagents)',
  '/plan': 'toggle read-only Plan Mode',
  '/mode': 'cycle ask / auto / plan / readonly (or Shift+Tab)',
  '/revert': 'rewind turn(s) & restore prompt to composer (or Esc Esc)',
  '/undo': 'rewind last turn & restore prompt to composer (or Esc Esc)',
  '/checkpoints': 'pick a previous turn checkpoint to rewind to',
  '/compact': 'summarize history to free context',
  '/diff': 'show git diff in workspace (including untracked files)',
  '/review': 'run code review on current diff, branch, or commit',
  '/resume': 'resume a saved session',
  '/new': 'start a fresh session',
  '/delete': 'delete a saved session by id or current session',
  '/debug-config': 'show resolved config paths, model capabilities, and settings',
  '/warnings': 'show retained MCP or runtime warnings',
  '/status': 'show session & token status',
  '/doctor': 'verify environment & gateway /models',
  '/setup': 'run the provider / model setup wizard',
  '/todo': 'show or clear the todo checklist',
  '/fork': 'fork this session into a new one',
  '/rename': 'rename the current session',
  '/export': 'export the session to a Markdown file',
  '/init': 'create or update AGENTS.md for this project',
  '/tools': 'list available tools (built-in + MCP)',
  '/sessions': 'list saved sessions',
  '/clear': 'clear the screen (history is kept)',
  '/exit': 'quit goon cli',
};

// oxlint-disable-next-line no-control-regex
const CONTROL_CHARS_RE = new RegExp('[\\u0000-\\u0008\\u000b-\\u001f\\u007f-\\u009f]', 'g');

// Strip whole escape sequences before removing remaining control bytes, normalizing combining accents to NFC.
export function terminalText(value: string): string {
  return stripVTControlCharacters(value)
    .normalize('NFC')
    .replace(CONTROL_CHARS_RE, '');
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

export function visibleLength(str: string): number {
  let total = 0;
  for (const c of stripVTControlCharacters(str)) total += width(c);
  return total;
}

export function sliceVisible(str: string, maxCols: number): string {
  const normalized = str.normalize('NFC');
  let out = '';
  let used = 0;
  for (const c of normalized) {
    const w = width(c);
    if (used + w > maxCols) break;
    out += c;
    used += w;
  }
  return out;
}

export function padEndVisible(str: string, cols: number): string {
  const sliced = sliceVisible(str, cols);
  const vis = visibleLength(sliced);
  return `${sliced}${' '.repeat(Math.max(0, cols - vis))}`;
}

function padRightAnsi(str: string, cols: number, bgAnsi = ''): string {
  const vis = visibleLength(str);
  const pad = Math.max(0, cols - vis);
  return `${bgAnsi}${str}${bgAnsi}${' '.repeat(pad)}\x1b[0m`;
}

/** Format inline Markdown (**bold**, `code`, [label](url), ~~strike~~) into ANSI spans. */
export function formatInlineMarkdown(raw: string, baseSgr = '\x1b[0m'): string {
  let out = raw;
  // [Image #N] and [Pasted text #N ...] attachment pills
  out = out.replace(/(\[(?:Image #\d+|Pasted text #\d+)[^\]]*\])/g, `${THEME.brandBold}$1${baseSgr}`);
  // Inline code `...`
  out = out.replace(/`([^`\n]+)`/g, `${THEME.info}$1${baseSgr}`);
  // Markdown links [label](url)
  out = out.replace(/\[([^\]]+)\]\(([^)]+)\)/g, `\x1b[4m${THEME.info}$1\x1b[24m${THEME.faint} ($2)${baseSgr}`);
  // Bold **...**
  out = out.replace(/\*\*([^*]+)\*\*/g, `\x1b[1m$1\x1b[22m${baseSgr}`);
  // Strikethrough ~~...~~
  out = out.replace(/~~([^~]+)~~/g, `\x1b[9m$1\x1b[29m${baseSgr}`);
  return out;
}

/**
 * Wraps plain text into chunks that fit `firstAvail` columns on the first row and `contAvail` after.
 * With `words`, breaks at the last space when the carried tail still fits; otherwise hard-wraps.
 */
function wrapChunks(text: string, firstAvail: number, contAvail: number, words: boolean): string[] {
  const clean = terminalText(text).replace(/\t/g, '  ');
  const contLimit = Math.max(1, contAvail);
  const out: string[] = [];
  let avail = Math.max(1, firstAvail);
  let cur = '';
  let used = 0;
  for (const c of clean) {
    const w = width(c);
    if (used + w > avail && cur) {
      if (words && c === ' ') {
        out.push(cur);
        cur = '';
        used = 0;
        avail = contLimit;
        continue;
      }
      const cut = words ? cur.lastIndexOf(' ') : -1;
      if (cut > 0 && visibleLength(cur.slice(cut + 1)) + w <= contLimit) {
        out.push(cur.slice(0, cut));
        cur = cur.slice(cut + 1);
      } else {
        out.push(cur);
        cur = '';
      }
      used = visibleLength(cur);
      avail = contLimit;
    }
    cur += c;
    used += w;
  }
  out.push(cur);
  return out;
}

const TABLE_SEPARATOR_RE = /^\|[\s:|-]+\|$/;
const isTableRow = (line: string) => line.trimStart().startsWith('|') && line.trimEnd().endsWith('|') && line.trim().length > 1;
const splitTableCells = (trimmedRow: string) => trimmedRow.slice(1, -1).split('|').map(c => c.trim());
/** Text a cell displays after inline Markdown markers are consumed (used for column alignment). */
const displayedCell = (cell: string) =>
  cell
    .replace(/\*\*([^*]+)\*\*/g, '$1')
    .replace(/`([^`\n]+)`/g, '$1')
    .replace(/~~([^~]+)~~/g, '$1')
    .replace(/\[([^\]]+)\]\(([^)]+)\)/g, '$1 ($2)');

/** Unstyled row for tiny terminals (< 48x12). */
function plainRow(row: Row, cols: number): string {
  if (row.kind === 'blank') return '';
  if (row.kind === 'codeHead') return sliceVisible(`  ─ ${row.text}`, cols);
  if (row.kind === 'codeFoot') return '  ─';
  return `${row.pre}${row.text}`;
}

/** Paints welcome-banner / panel box rows: faint frame, brand mark, faint labels, bright values. */
function styleBoxLine(line: string): string {
  const T = THEME;
  const R = T.reset;
  if (line.startsWith('╭') || line.startsWith('╰')) {
    return `${T.faint}${line.replace(/[^─╭╮╰╯]+/g, m => `${R}${T.brandBold}${m}${R}${T.faint}`)}${R}`;
  }
  const m = /^│(.*?)(│?)$/.exec(line);
  if (!m) return line;
  const content = m[1];
  const close = m[2] ? `${T.faint}│${R}` : '';
  let styled: string;
  const title = /^(\s*)(◆ goon)(\s+)(v\S+)(\s+)(.*?)(\s*)$/.exec(content);
  const field = /^(\s{2,4})([a-z]+)(\s{2,})(.*)$/.exec(content);
  if (title) {
    styled = `${title[1]}${T.brandBold}${title[2]}${R}${title[3]}${T.faint}${title[4]}${R}${title[5]}${T.muted}${title[6]}${R}${title[7]}`;
  } else if (field) {
    styled = `${field[1]}${T.faint}${field[2]}${R}${field[3]}${T.text}${field[4]}${R}`;
  } else {
    styled = `${T.faint}${content.replace(/(\/help|shift\+tab|[@$!](?= ))/g, tok => `${R}${T.info}${tok}${R}${T.faint}`)}${R}`;
  }
  return `${T.faint}│${R}${styled}${close}`;
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

function formatCompactDiff(diffText: string, maxLines = 20): string[] {
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
      oldLine = Math.max(1, Number(hunkMatch[1]));
      newLine = Math.max(1, Number(hunkMatch[2]));
      continue;
    }
    const moreMatch = /^\.\.\. \([+-](\d+) more (?:added|removed) lines\)$/.exec(line);
    if (moreMatch) {
      omitted += Number(moreMatch[1]);
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

const LARGE_PASTE_CHAR_THRESHOLD = 150;
const ATOMIC_PLACEHOLDER_END_RE = /(\[(?:Image #\d+|Pasted text #\d+[^\]]*)\]\s?)$/;
const ATOMIC_PLACEHOLDER_START_RE = /^(\[(?:Image #\d+|Pasted text #\d+[^\]]*)\]\s?)/;

/** Rejection reason when the user presses Esc on a question opened with `escCancels`. */
export class InputCancelledError extends Error {
  constructor() {
    super('Cancelled by user.');
    this.name = 'InputCancelledError';
  }
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
  private lastFrameRows: string[] = [];
  private lastCols = 0;
  private lastRows = 0;
  private lastCursorSeq = '';
  private lastBodyTotalLines = 0;
  private needsFullClear = true;
  private inBracketedPaste = false;
  private bracketedPasteBuffer = '';
  private suppressKeypressChunk = false;
  private pendingRawPasteChunk = '';
  private lastPasteToken?: string;
  private lastPasteAt = 0;
  private inSgrMouse = false;
  private sgrMouseBuf = '';
  private attachedImages = new Map<string, string>();
  private imageArchive = new Map<string, string>();
  private nextImageIdx = 1;
  private pastedTexts = new Map<string, string>();
  private pasteArchive = new Map<string, string>();
  private recentExpandedPastes: Array<{ placeholder: string; content: string }> = [];
  private nextPasteIdx = 1;
  private queuedPrompts: string[] = [];
  private popupIndex = 0;
  private popupFor = '';
  private escCancels = false;
  private escHint = 'skip';
  private turnSnapshots: string[] = [];
  private inputRestoredFromRevert = false;
  private escPrimed?: 'backtrack' | 'clear';
  private escPrimeTimer?: NodeJS.Timeout;
  private statusNotice?: string;
  private viewMode: 'normal' | 'verbose' | 'transcript' = 'verbose';
  private inTranscriptOverlay = false;
  private workspaceName = '';
  private branch = '';
  private contextWindow = 0;
  private contextUsed?: number;
  onToggleMode?: () => void;
  onCycleEffort?: (direction: 1 | -1) => void;
  onToggleTranscript?: (active: boolean) => void;
  onImmediateCommand?: (cmd: string) => void | Promise<void>;
  onCopy?: () => void | Promise<void>;

  constructor(
    public title: string,
    readonly cancel: () => void,
    readonly source: ReadStream = process.stdin,
    readonly target: WriteStream = process.stdout,
  ) {
    this.oldRaw = source.isRaw;
    source.on('data', this.beforeRawData);
    emitKeypressEvents(source);
    source.setRawMode(true);
    source.resume();
    source.on('keypress', this.keypress);
    source.on('data', this.afterRawData);
    source.on('end', this.ended);
    target.on('resize', this.resize);
    // Enter Alternate Screen (\x1b[?1049h), ensure mouse click capture is OFF (\x1b[?1000l\x1b[?1006l) so native
    // mouse text selection/copy works, enable DEC Alternate Scroll (\x1b[?1007h) for wheel scrolling, and enable Bracketed Paste (\x1b[?2004h).
    target.write('\x1b[?1049h\x1b[?1000l\x1b[?1006l\x1b[?1007h\x1b[?2004h\x1b[?25l');
    this.render();
  }

  renderedFrame(): string {
    return this.lastFrameRows.join('\r\n');
  }

  clear() {
    this.transcript = '';
    this.scroll = 0;
    this.maxScroll = 0;
    this.exploreGroup = [];
    this.turnSnapshots = [];
    this.inputRestoredFromRevert = false;
    this.escPrimed = undefined;
    if (this.escPrimeTimer) {
      clearTimeout(this.escPrimeTimer);
      this.escPrimeTimer = undefined;
    }
    this.statusNotice = undefined;
    this.reasoningBuffer = '';
    this.reasoningHeader = undefined;
    this.reasoningDetail = undefined;
    this.activeToolStream = undefined;
    this.lastFrameRows = [];
    this.lastBodyTotalLines = 0;
    this.needsFullClear = true;
    this.schedule();
  }

  /**
   * Captures the exact TUI transcript state before a user turn begins so `/revert` or `Esc Esc`
   * can cleanly remove the reverted turn(s) from the visible screen (Codex backtrack pattern).
   */
  markTurnStart() {
    this.flushExploreGroup();
    this.outputChannel = undefined;
    this.turnSnapshots.push(this.transcript);
    if (this.turnSnapshots.length > 100) this.turnSnapshots.shift();
  }

  /**
   * Rewinds the visible TUI transcript back `steps` turn(s).
   * Returns true if restored from live turn snapshots; false if caller should rebuild from session messages.
   */
  rewindTurns(steps: number): boolean {
    this.flushExploreGroup();
    this.outputChannel = undefined;
    if (steps > 0 && this.turnSnapshots.length >= steps) {
      const targetIdx = this.turnSnapshots.length - steps;
      const restored = this.turnSnapshots[targetIdx];
      this.turnSnapshots.length = targetIdx;
      this.transcript = restored;
      this.scroll = 0;
      this.maxScroll = 0;
      this.lastFrameRows = [];
      this.lastBodyTotalLines = 0;
      this.needsFullClear = true;
      this.schedule(0);
      return true;
    }
    this.turnSnapshots = [];
    return false;
  }

  /**
   * Restores a user prompt into the Composer Input Box for in-place editing or immediate Enter resend (Codex pattern).
   */
  setInput(text: string, fromRevert = false) {
    this.input = [];
    this.cursor = 0;
    this.inputRestoredFromRevert = fromRevert;
    this.attachedImages.clear();
    this.nextImageIdx = 1;
    this.pastedTexts.clear();
    this.nextPasteIdx = 1;
    let restored = text;
    for (const [placeholder, content] of this.pasteArchive.entries()) {
      if (content && restored.includes(content)) {
        restored = restored.replace(content, () => placeholder);
        this.pastedTexts.set(placeholder, content);
        const m = /^\[Pasted text #(\d+)/.exec(placeholder);
        if (m) this.nextPasteIdx = Math.max(this.nextPasteIdx, Number(m[1]) + 1);
      }
    }
    const normalized = terminalText(restored.replace(/\r\n/g, '\n').replace(/\r/g, '\n'));
    if (normalized.includes('\n') || [...normalized].length > LARGE_PASTE_CHAR_THRESHOLD) {
      this.handlePaste(normalized);
      this.inputRestoredFromRevert = fromRevert;
      return;
    }
    this.input = [...normalized];
    this.cursor = this.input.length;
    this.rehydratePlaceholdersInInput();
    this.collapsePastedImagePathsInInput();
    this.schedule(0);
  }

  insertText(text: string) {
    this.handlePaste(text);
  }

  setNotice(notice: string | undefined) {
    this.statusNotice = notice ? terminalText(notice) : undefined;
    this.schedule(0);
  }

  setTitle(title: string) {
    this.title = terminalText(title);
    this.schedule();
  }

  setMode(mode: string) {
    this.modeLabel = terminalText(mode);
    // If switched to 'auto' while waiting at a tool approval prompt, auto-approve it immediately!
    if (mode === 'auto' && this.pending && !this.remember && this.label.includes('y/N')) {
      const pending = this.pending;
      this.pending = undefined;
      pending.cleanup();
      pending.resolve('a');
    }
    this.schedule();
  }

  setViewMode(mode: 'normal' | 'verbose' | 'transcript') {
    this.viewMode = mode;
    this.inTranscriptOverlay = mode === 'transcript';
    this.scroll = 0;
    this.needsFullClear = true;
    this.schedule(0);
  }

  getViewMode(): 'normal' | 'verbose' | 'transcript' {
    return this.inTranscriptOverlay ? 'transcript' : this.viewMode;
  }

  setTranscriptOverlay(active: boolean) {
    this.inTranscriptOverlay = active;
    if (active) this.flushExploreGroup();
    this.needsFullClear = true;
    this.schedule(0);
    this.onToggleTranscript?.(active);
  }

  isTranscriptOverlay(): boolean {
    return this.inTranscriptOverlay;
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

  /** Header identity: workspace folder name and current git branch (or short detached SHA). */
  setWorkspace(cwd: string, branch?: string) {
    const name = terminalText(cwd).replace(/[\\/]+$/, '').split(/[\\/]/).pop() ?? '';
    this.workspaceName = name || terminalText(cwd);
    this.branch = branch ? terminalText(branch) : '';
    this.schedule();
  }

  setContextWindow(tokens: number) {
    this.contextWindow = Number.isFinite(tokens) && tokens > 0 ? tokens : 0;
    this.schedule();
  }

  /** Context occupancy in tokens; `undefined` hides the meter until the next provider usage report. */
  setContextUsage(tokens: number | undefined) {
    this.contextUsed = tokens !== undefined && Number.isFinite(tokens) && tokens >= 0 ? tokens : undefined;
    this.schedule();
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
    this.exploreGroup = [];
    this.appendCell(`\n${GLYPH.explored} Explored\n${formatted}\n`);
  }

  private appendTranscript(text: string, forceBottom = false) {
    const wasAtBottom = this.scroll === 0;
    this.transcript = (this.transcript + terminalText(text)).slice(-120_000);
    if (forceBottom || wasAtBottom) this.scroll = 0;
    this.schedule(16);
  }

  /**
   * A `[normal]` marker closes any open reasoning/answer block so the following cell or CLI output
   * can never inherit answer/thinking formatting. The marker itself renders as nothing.
   */
  private blockReset(): string {
    return !this.transcript || this.transcript.endsWith('\n') ? '[normal]\n' : '\n[normal]\n';
  }

  /** Appends a glyph-prefixed transcript cell (tool result, todo, agent, notice) after closing any open block. */
  private appendCell(text: string) {
    this.outputChannel = undefined;
    this.appendTranscript(this.blockReset() + text);
  }

  write(text: string) {
    let cleaned = text;
    // Clean up raw @"C:\...\image.png" in user prompt echo so transcript shows [Image #1: name.png]
    cleaned = cleaned.replace(/@"([^"]+\.(?:png|jpe?g|webp|gif))"/gi, (_, p: string) => `[Image: ${basename(p)}]`);
    const trimmedStart = cleaned.trimStart();
    const isUserPrompt = trimmedStart.startsWith('> ') || trimmedStart.startsWith('› ');
    if (isUserPrompt) {
      this.flushExploreGroup();
      // Collapse any expanded large/multiline pastes back to their [Pasted text #N ...] pills in the transcript echo
      for (const { placeholder, content } of this.recentExpandedPastes) {
        if (content && cleaned.includes(content)) {
          cleaned = cleaned.replace(content, () => placeholder);
        }
      }
      // Only a CLI write that starts with "> " is a user turn. Mark every line of it with a private-use
      // code point so streamed Markdown blockquotes ("> quote") in answers can never render as user turns.
      const currentTrimmedStart = cleaned.trimStart();
      const lead = cleaned.slice(0, cleaned.length - currentTrimmedStart.length);
      const body = currentTrimmedStart
        .slice(2)
        .split('\n')
        .map(line => (line.trim() ? `${USER_MARK}${line}` : line))
        .join('\n');
      cleaned = lead + body;
    }
    this.outputChannel = undefined;
    this.appendTranscript(this.blockReset() + cleaned, isUserPrompt);
  }

  async askTransient(
    previewBlock: string,
    label = '[y/N/a(lways)] ',
    signal?: AbortSignal,
    opts: { escCancels?: boolean; escHint?: string } = {},
  ): Promise<string> {
    this.flushExploreGroup();
    this.outputChannel = undefined;
    const snapshot = this.transcript;
    const prevLabel = this.label;
    const savedInput = [...this.input];
    const savedCursor = this.cursor;
    const savedImages = new Map(this.attachedImages);
    const savedNextImageIdx = this.nextImageIdx;
    const savedPastes = new Map(this.pastedTexts);
    const savedNextPasteIdx = this.nextPasteIdx;
    this.input = [];
    this.cursor = 0;
    this.attachedImages.clear();
    this.pastedTexts.clear();
    this.appendTranscript(this.blockReset() + previewBlock, true);
    try {
      return await this.question(label, signal, false, opts);
    } finally {
      this.transcript = snapshot;
      this.label = prevLabel;
      this.input = savedInput;
      this.cursor = savedCursor;
      this.attachedImages = savedImages;
      this.nextImageIdx = savedNextImageIdx;
      this.pastedTexts = savedPastes;
      this.nextPasteIdx = savedNextPasteIdx;
      this.schedule(16);
    }
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
        this.appendCell(`\n${GLYPH.agent} Agent ${event.id.slice(0, 8)} · ${event.task.slice(0, 160)}\n`);
      } else if (event.state === 'done' || event.state === 'error') {
        this.flushExploreGroup();
        this.appendCell(
          event.state === 'done'
            ? `\n${GLYPH.agent} Agent ${event.id.slice(0, 8)} finished\n`
            : `\n${GLYPH.failed} Agent ${event.id.slice(0, 8)} failed\n`,
        );
      }
    } else if (event.type === 'tool_stream') {
      // Do not clutter the status bar with noisy per-delta file/line counters
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
      } else if (event.type === 'text' && this.outputChannel !== 'text') {
        this.appendTranscript('\n[answer]\n');
      }
      if (event.type === 'reasoning') this.reasoningSource = event.source;
      this.outputChannel = event.type;
      this.appendTranscript(event.text);
    } else if (event.type === 'tool') {
      this.activeToolStream = undefined;
      this.outputChannel = undefined;
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
      this.todoLabel = event.todos.length ? `${done}/${event.todos.length}` : '';
      const lines = event.todos.map(t => {
        const icon = t.status === 'completed' ? '✓' : t.status === 'in_progress' ? '▸' : '○';
        return `  ${icon} ${t.content}`;
      });
      if (lines.length) {
        this.appendCell(`\n${GLYPH.todo} Todo ${done}/${event.todos.length}\n${lines.join('\n')}\n`);
      }
    } else if (event.type === 'plan_exit') {
      this.flushExploreGroup();
      this.appendCell(`\n${GLYPH.todo} Plan approved — exited plan mode\n`);
    } else if (event.type === 'usage') {
      this.tokens += event.total;
      // Prompt + completion of the latest request is the provider's own measure of context occupancy.
      if (event.input > 0) this.contextUsed = event.input + event.output;
    } else if (event.type === 'compaction') {
      this.flushExploreGroup();
      this.contextUsed = undefined;
      this.appendCell(`\n${GLYPH.compact} Context compacted #${event.count}\n`);
    } else if (event.type === 'stopped') {
      this.flushExploreGroup();
      this.appendCell(`\n${GLYPH.allow} ${event.reason}\n`);
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
    this.outputChannel = undefined;
    const isFullTranscript = this.viewMode === 'transcript' || this.inTranscriptOverlay;
    if (
      state === 'done' &&
      !isFullTranscript &&
      (meta.kind === 'read' || meta.kind === 'list' || meta.kind === 'search')
    ) {
      this.exploreGroup.push({ kind: meta.kind, summary: meta.summary });
      if (this.exploreGroup.length >= 6) this.flushExploreGroup();
      return;
    }
    this.flushExploreGroup();
    if (isFullTranscript && (meta.kind === 'read' || meta.kind === 'list' || meta.kind === 'search')) {
      const verb = meta.kind === 'read' ? 'Read' : meta.kind === 'list' ? 'Listed' : 'Searched';
      const dur = meta.durationMs !== undefined ? ` (${formatCompactDuration(meta.durationMs)})` : '';
      this.appendCell(`\n${state === 'error' ? `${GLYPH.failed} Failed` : `${GLYPH.explored} ${verb}`} ${meta.summary}${dur}\n`);
      return;
    }
    if (meta.kind === 'exec') {
      const dur = meta.durationMs !== undefined ? ` (${formatCompactDuration(meta.durationMs)})` : '';
      const header = state === 'error' ? `${GLYPH.failed} Failed ${meta.summary}${dur}` : `${GLYPH.ran} Ran ${meta.summary}${dur}`;
      const previewLines: string[] = [];
      if (meta.outputPreview?.length) {
        const maxExecLines = isFullTranscript ? 80 : this.viewMode === 'verbose' ? 15 : 5;
        const shown = meta.outputPreview.slice(0, maxExecLines);
        for (const [idx, line] of shown.entries()) {
          previewLines.push(`  ${idx === 0 ? '└' : ' '} ${line}`);
        }
        const totalOmitted = (meta.omittedLines ?? 0) + Math.max(0, meta.outputPreview.length - shown.length);
        if (totalOmitted > 0) {
          previewLines.push(`    … +${totalOmitted} lines`);
        }
      } else {
        previewLines.push('  └ (no output)');
      }
      this.appendCell(`\n${header}\n${previewLines.join('\n')}\n`);
      return;
    }
    if (meta.kind === 'edit') {
      const verb = meta.verb ?? 'Edited';
      const stats =
        meta.added !== undefined || meta.removed !== undefined
          ? ` (+${meta.added ?? 0} -${meta.removed ?? 0})`
          : '';
      const maxDiffLines = isFullTranscript ? 500 : this.viewMode === 'verbose' ? 120 : 30;
      const diffLines = meta.diff ? formatCompactDiff(meta.diff, maxDiffLines) : [];
      const diffBlock = diffLines.length ? `\n${diffLines.join('\n')}` : '';
      this.appendCell(`\n${state === 'error' ? GLYPH.failed : GLYPH.edit} ${verb} ${meta.summary}${stats}${diffBlock}\n`);
      return;
    }
    if (name !== 'todo_write' && name !== 'update_plan') {
      const dur = meta.durationMs !== undefined ? ` (${formatCompactDuration(meta.durationMs)})` : '';
      this.appendCell(`\n${state === 'error' ? `${GLYPH.failed} Failed` : `${GLYPH.done} Completed`} ${meta.summary}${dur}\n`);
    }
  }

  setStatus(status: string) {
    if (this.closed) return;
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

  getStatus(): string {
    return this.status;
  }

  /**
   * Runs `work` under a temporary status and always restores the previous one afterwards (even on
   * throw), so transient phases such as MCP reconnects can never leave the UI stuck on "Working".
   */
  async withStatus<T>(status: string, work: () => Promise<T>): Promise<T> {
    const previous = this.status;
    this.setStatus(status);
    try {
      return await work();
    } finally {
      if (!this.closed) this.setStatus(previous === status ? 'Ready' : previous);
    }
  }

  question(
    label = 'goon> ',
    signal?: AbortSignal,
    remember = false,
    opts: { escCancels?: boolean; escHint?: string } = {},
  ): Promise<string> {
    if (this.closed) return Promise.reject(new Error('Terminal closed.'));
    if (this.pending) return Promise.reject(new Error('A terminal question is already active.'));
    signal?.throwIfAborted();
    this.label = terminalText(label);
    this.escCancels = Boolean(opts.escCancels);
    this.escHint = opts.escHint ?? (opts.escCancels ? 'cancel' : /y\/N/.test(label) ? 'deny' : 'skip');
    if (remember && this.queuedPrompts.length > 0) {
      const nextQueued = this.expandInputPlaceholders(this.queuedPrompts.shift()!);
      this.schedule(0);
      return Promise.resolve(nextQueued);
    }
    if (!remember) {
      this.input = [];
      this.cursor = 0;
      this.attachedImages.clear();
      this.nextImageIdx = 1;
      this.pastedTexts.clear();
      this.nextPasteIdx = 1;
    }
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

  private recordImageArchive(placeholder: string, fullPath: string) {
    this.imageArchive.set(placeholder, fullPath);
    if (this.imageArchive.size > 100) {
      const firstKey = this.imageArchive.keys().next().value;
      if (firstKey !== undefined) this.imageArchive.delete(firstKey);
    }
  }

  private recordPasteArchive(placeholder: string, content: string) {
    this.pasteArchive.set(placeholder, content);
    if (this.pasteArchive.size > 100) {
      const firstKey = this.pasteArchive.keys().next().value;
      if (firstKey !== undefined) this.pasteArchive.delete(firstKey);
    }
  }

  /**
   * Rehydrates `[Image #N]` and `[Pasted text #N ...]` maps when recalling history (`↑`/`↓`)
   * or popping a queued prompt (`Alt+↑`) so atomic Backspace and Enter expansion still work.
   */
  private rehydratePlaceholdersInInput() {
    const current = this.input.join('');
    this.attachedImages.clear();
    this.pastedTexts.clear();
    for (const m of current.matchAll(/\[Image #(\d+)\]/g)) {
      const token = m[0];
      const archived = this.imageArchive.get(token);
      if (archived) {
        this.attachedImages.set(token, archived);
        this.nextImageIdx = Math.max(this.nextImageIdx, Number(m[1]) + 1);
      }
    }
    for (const m of current.matchAll(/\[Pasted text #(\d+)[^\]]*\]/g)) {
      const token = m[0];
      const archived = this.pasteArchive.get(token);
      if (archived !== undefined) {
        this.pastedTexts.set(token, archived);
        this.nextPasteIdx = Math.max(this.nextPasteIdx, Number(m[1]) + 1);
      }
    }
  }

  /**
   * Expands any `[Image #N]` and `[Pasted text #N ...]` placeholders in `rawInput` back to their
   * full file mentions / multi-line text payloads in a single pass (Codex `expand_pending_pastes` pattern).
   */
  private expandInputPlaceholders(rawInput: string): string {
    let value = rawInput;
    value = value.replace(/\[Image #\d+\]/g, token => {
      const fullPath = this.attachedImages.get(token) ?? this.imageArchive.get(token);
      return fullPath ? `@"${fullPath}"` : token;
    });
    // Strip the auto-appended trailing space if the input ends right after a paste placeholder
    value = value.replace(/(\[Pasted text #\d+[^\]]*\]) $/, '$1');
    value = value.replace(/(\[Pasted text #\d+[^\]]*\])( ?)/g, (full, token: string, trailingSpace: string) => {
      const content = this.pastedTexts.get(token) ?? this.pasteArchive.get(token);
      if (content === undefined) return full;
      this.recentExpandedPastes.push({ placeholder: token, content });
      if (this.recentExpandedPastes.length > 20) this.recentExpandedPastes.shift();
      if (trailingSpace && content.includes('\n') && !content.endsWith('\n')) {
        return `${content}\n`;
      }
      return `${content}${trailingSpace}`;
    });
    return value;
  }

  /**
   * Integrates pasted text (from bracketed paste, OS clipboard, or non-bracketed burst) into the composer.
   * Collapses multi-line or long pastes into atomic `[Pasted text #1 +N lines]` / `[Pasted text #1 (N chars)]`
   * placeholders (Codex `apply_paste` pattern) so the composer never floods or lags.
   */
  private handlePaste(rawText: string, allowCoalesce = false) {
    const normalized = terminalText(rawText.replace(/\r\n/g, '\n').replace(/\r/g, '\n'));
    const cleaned =
      !normalized.slice(0, -1).includes('\n') && normalized.endsWith('\n')
        ? normalized.slice(0, -1)
        : normalized;
    if (!cleaned) return;
    this.inputRestoredFromRevert = false;
    this.scroll = 0;

    // 1. Check if the pasted text is a single existing local image path
    const trimmed = cleaned.trim();
    const unquoted =
      (trimmed.startsWith('"') && trimmed.endsWith('"')) || (trimmed.startsWith("'") && trimmed.endsWith("'"))
        ? trimmed.slice(1, -1).trim()
        : trimmed;
    if (
      !unquoted.includes('\n') &&
      /\.(?:png|jpe?g|webp|gif)$/i.test(unquoted) &&
      (/^[A-Za-z]:[\\/]/.test(unquoted) || unquoted.startsWith('\\\\') || unquoted.startsWith('/')) &&
      existsSync(unquoted)
    ) {
      if (this.attachedImages.size === 0 && this.queuedPrompts.length === 0) this.nextImageIdx = 1;
      const placeholder = `[Image #${this.nextImageIdx++}]`;
      this.attachedImages.set(placeholder, unquoted);
      this.recordImageArchive(placeholder, unquoted);
      const chars = [...`${placeholder} `];
      this.input.splice(this.cursor, 0, ...chars);
      this.cursor += chars.length;
      this.schedule(0);
      return;
    }

    // 2. Coalesce consecutive non-bracketed paste chunks arriving within 25ms
    if (
      allowCoalesce &&
      this.lastPasteToken &&
      Date.now() - this.lastPasteAt <= 25 &&
      this.pastedTexts.has(this.lastPasteToken)
    ) {
      const prevContent = this.pastedTexts.get(this.lastPasteToken)!;
      const merged = `${prevContent}${cleaned}`;
      const matchIdx = /^\[Pasted text #(\d+)/.exec(this.lastPasteToken);
      const idx = matchIdx ? Number(matchIdx[1]) : 1;
      const mergedLines = merged.split('\n').length;
      const mergedChars = [...merged].length;
      const newToken =
        mergedLines >= 2
          ? `[Pasted text #${idx} +${mergedLines} lines]`
          : `[Pasted text #${idx} (${mergedChars} chars)]`;
      const currentStr = this.input.join('');
      if (currentStr.includes(this.lastPasteToken)) {
        this.pastedTexts.delete(this.lastPasteToken);
        this.pasteArchive.delete(this.lastPasteToken);
        this.pastedTexts.set(newToken, merged);
        this.recordPasteArchive(newToken, merged);
        const nextStr = currentStr.replace(this.lastPasteToken, () => newToken);
        const delta = [...newToken].length - [...this.lastPasteToken].length;
        this.input = [...nextStr];
        this.cursor = Math.max(0, Math.min(this.input.length, this.cursor + delta));
        this.lastPasteToken = newToken;
        this.lastPasteAt = Date.now();
        this.schedule(0);
        return;
      }
    }

    // 3. Collapse multi-line pastes (>= 2 lines) or long single-line pastes (> 150 chars) into atomic placeholder
    const lineCount = cleaned.split('\n').length;
    const charCount = [...cleaned].length;
    const isTransientPrompt = Boolean(this.pending && !this.remember);
    const shouldCollapse =
      lineCount >= 2 ||
      (charCount > LARGE_PASTE_CHAR_THRESHOLD && (!isTransientPrompt || charCount > 500));

    if (shouldCollapse) {
      if (this.pastedTexts.size === 0 && this.queuedPrompts.length === 0) this.nextPasteIdx = 1;
      const idx = this.nextPasteIdx++;
      const placeholder =
        lineCount >= 2
          ? `[Pasted text #${idx} +${lineCount} lines]`
          : `[Pasted text #${idx} (${charCount} chars)]`;
      this.pastedTexts.set(placeholder, cleaned);
      this.recordPasteArchive(placeholder, cleaned);
      const suffix = this.input[this.cursor] === ' ' ? '' : ' ';
      const chars = [...`${placeholder}${suffix}`];
      this.input.splice(this.cursor, 0, ...chars);
      this.cursor += chars.length;
      this.lastPasteToken = placeholder;
      this.lastPasteAt = Date.now();
      this.schedule(0);
      return;
    }

    // 4. Short single-line paste: insert inline and check for embedded image paths
    const chars = [...cleaned];
    this.input.splice(this.cursor, 0, ...chars);
    this.cursor += chars.length;
    this.collapsePastedImagePathsInInput();
    this.schedule(0);
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
      if (this.attachedImages.size === 0 && this.queuedPrompts.length === 0) this.nextImageIdx = 1;
      const placeholder = `[Image #${this.nextImageIdx++}]`;
      this.attachedImages.set(placeholder, unquoted);
      this.recordImageArchive(placeholder, unquoted);
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
      if (this.attachedImages.size === 0 && this.queuedPrompts.length === 0) this.nextImageIdx = 1;
      const placeholder = `[Image #${this.nextImageIdx++}]`;
      this.attachedImages.set(placeholder, candidate);
      this.recordImageArchive(placeholder, candidate);
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
    if (process.platform !== 'win32' || this.closed) return;
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
      if (res.code !== 0 || !res.output.trim() || this.closed) return;
      const out = res.output.replace(/\r?\n$/, '');
      if (out.startsWith('IMG:') || out.startsWith('FILE:')) {
        const filePath = out.replace(/^(?:IMG|FILE):/, '').trim();
        if (/\.(?:png|jpe?g|webp|gif)$/i.test(filePath) && existsSync(filePath)) {
          if (this.attachedImages.size === 0 && this.queuedPrompts.length === 0) this.nextImageIdx = 1;
          const placeholder = `[Image #${this.nextImageIdx++}]`;
          this.attachedImages.set(placeholder, filePath);
          this.recordImageArchive(placeholder, filePath);
          const chars = [...`${placeholder} `];
          this.input.splice(this.cursor, 0, ...chars);
          this.cursor += chars.length;
          this.schedule(0);
          return;
        }
      }
      if (out.startsWith('TXT:')) {
        this.handlePaste(out.slice(4));
      }
    } catch {
      // Ignore clipboard helper errors
    }
  }

  /**
   * Slash popup state for the main composer: commands matching the typed `/prefix` (exact match first)
   * and the selected index, which resets to 0 whenever the typed text changes.
   */
  private slashPopup(): { matches: string[]; index: number } {
    const typed = this.input.join('');
    if (this.label !== 'goon> ' || !typed.startsWith('/') || /\s/.test(typed)) {
      this.popupFor = '';
      this.popupIndex = 0;
      return { matches: [], index: 0 };
    }
    const matches = SLASH_COMMANDS.filter(c => c.startsWith(typed)).sort((a, b) => Number(b === typed) - Number(a === typed));
    if (this.popupFor !== typed) {
      this.popupFor = typed;
      this.popupIndex = 0;
    }
    const index = matches.length ? Math.min(Math.max(0, this.popupIndex), matches.length - 1) : 0;
    return { matches, index };
  }

  private scrollBy(deltaLines: number) {
    if (deltaLines === 0) return;
    this.scroll = Math.max(0, Math.min(this.maxScroll, this.scroll + deltaLines));
    this.schedule(0);
  }

  /**
   * Intercepts non-bracketed multi-line or large paste bursts on raw `source` data chunks BEFORE
   * Node's `emitKeypressEvents` splits them into hundreds of per-character keypress/return events.
   */
  private beforeRawData = (chunk: Buffer | string) => {
    if (this.closed || this.inBracketedPaste) return;
    const raw = typeof chunk === 'string' ? chunk : chunk.toString('utf8');
    if (raw.includes('\x1b')) return;
    const withoutTrailingNewline = raw.replace(/\r?\n$/, '').replace(/\r$/, '');
    const isMultiLine = /[\r\n]/.test(withoutTrailingNewline);
    const isLargeChunk = [...withoutTrailingNewline].length > LARGE_PASTE_CHAR_THRESHOLD;
    const isBurstContinuation =
      this.lastPasteToken !== undefined &&
      Date.now() - this.lastPasteAt <= 25 &&
      withoutTrailingNewline.length > 1;
    if (isMultiLine || isLargeChunk || isBurstContinuation) {
      this.suppressKeypressChunk = true;
      this.pendingRawPasteChunk = raw;
    }
  };

  private afterRawData = () => {
    if (!this.suppressKeypressChunk) return;
    this.suppressKeypressChunk = false;
    const raw = this.pendingRawPasteChunk;
    this.pendingRawPasteChunk = '';
    if (raw) {
      this.handlePaste(raw, true);
    }
  };

  private keypress = (text: string | undefined, key: Key = {}) => {
    if (this.suppressKeypressChunk) return;
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
      this.bracketedPasteBuffer = '';
      return;
    }
    if (key.name === 'paste-end') {
      this.inBracketedPaste = false;
      const pasted = this.bracketedPasteBuffer;
      this.bracketedPasteBuffer = '';
      this.handlePaste(pasted);
      return;
    }

    if (!this.inBracketedPaste) {
      if (key.name !== 'escape' && this.escPrimed) {
        this.escPrimed = undefined;
        if (this.escPrimeTimer) {
          clearTimeout(this.escPrimeTimer);
          this.escPrimeTimer = undefined;
        }
      }
      if (this.statusNotice && key.name !== 'escape') {
        this.statusNotice = undefined;
      }
      if (key.ctrl && key.name === 'c') {
        this.cancel();
        return;
      }
      // Codex Ctrl+T: toggle full-screen Transcript Mode at any time (idle or working)
      if ((key.ctrl && key.name === 't') || key.sequence === '\x14') {
        this.setTranscriptOverlay(!this.inTranscriptOverlay);
        this.onToggleTranscript?.(this.inTranscriptOverlay);
        return;
      }
      // Codex Ctrl+O: copy last assistant response or code block to clipboard
      if ((key.ctrl && key.name === 'o') || key.sequence === '\x0f') {
        void this.onCopy?.();
        return;
      }
      if (this.inTranscriptOverlay) {
        if (key.name === 'escape' || (!key.ctrl && !key.meta && text === 'q' && this.input.length === 0)) {
          this.setTranscriptOverlay(false);
          this.onToggleTranscript?.(false);
          return;
        }
        if (key.name === 'up' || key.name === 'down') {
          this.scrollBy(key.name === 'up' ? 3 : -3);
          return;
        }
        if (key.name === 'home' || (!key.ctrl && !key.meta && text === 'g' && this.input.length === 0)) {
          this.scroll = this.maxScroll;
          this.schedule(0);
          return;
        }
        if (key.name === 'end' || (!key.ctrl && !key.meta && text === 'G' && this.input.length === 0)) {
          this.scroll = 0;
          this.schedule(0);
          return;
        }
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
        // Codex Esc Esc backtrack / clear input flow when in main composer prompt
        if (this.pending && this.remember) {
          const wantBacktrack = this.input.length === 0 || this.inputRestoredFromRevert;
          if (wantBacktrack) {
            if (this.escPrimed === 'backtrack') {
              this.escPrimed = undefined;
              if (this.escPrimeTimer) {
                clearTimeout(this.escPrimeTimer);
                this.escPrimeTimer = undefined;
              }
              const pending = this.pending;
              this.pending = undefined;
              pending.cleanup();
              this.input = [];
              this.cursor = 0;
              this.inputRestoredFromRevert = false;
              this.attachedImages.clear();
              this.pastedTexts.clear();
              pending.resolve('/revert');
              this.schedule(0);
              return;
            }
            this.escPrimed = 'backtrack';
            if (this.escPrimeTimer) clearTimeout(this.escPrimeTimer);
            this.escPrimeTimer = setTimeout(() => {
              this.escPrimed = undefined;
              this.escPrimeTimer = undefined;
              this.schedule(0);
            }, 2500);
            this.escPrimeTimer.unref?.();
            this.schedule(0);
            return;
          } else {
            if (this.escPrimed === 'clear') {
              this.escPrimed = undefined;
              if (this.escPrimeTimer) {
                clearTimeout(this.escPrimeTimer);
                this.escPrimeTimer = undefined;
              }
              this.input = [];
              this.cursor = 0;
              this.inputRestoredFromRevert = false;
              this.attachedImages.clear();
              this.nextImageIdx = 1;
              this.pastedTexts.clear();
              this.nextPasteIdx = 1;
              this.schedule(0);
              return;
            }
            this.escPrimed = 'clear';
            if (this.escPrimeTimer) clearTimeout(this.escPrimeTimer);
            this.escPrimeTimer = setTimeout(() => {
              this.escPrimed = undefined;
              this.escPrimeTimer = undefined;
              this.schedule(0);
            }, 2500);
            this.escPrimeTimer.unref?.();
            this.schedule(0);
            return;
          }
        } else if (this.pending && !this.remember) {
          const pending = this.pending;
          this.pending = undefined;
          pending.cleanup();
          this.input = [];
          this.cursor = 0;
          this.attachedImages.clear();
          this.pastedTexts.clear();
          if (this.escCancels) pending.reject(new InputCancelledError());
          else pending.resolve('');
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
      // Alt+Up pops the last queued prompt back into the composer box for editing (Codex pattern)
      if (key.meta && key.name === 'up' && this.queuedPrompts.length > 0) {
        const popped = this.queuedPrompts.pop()!;
        this.input = [...popped];
        this.cursor = this.input.length;
        this.inputRestoredFromRevert = false;
        this.rehydratePlaceholdersInInput();
        this.schedule(0);
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
        // While the slash popup is open, arrows move its selection (never history recall).
        const popup = this.slashPopup();
        if (popup.matches.length > 0) {
          const delta = key.name === 'up' ? -1 : 1;
          this.popupIndex = (popup.index + delta + popup.matches.length) % popup.matches.length;
          this.schedule(0);
          return;
        }
        const dir = key.name === 'up' ? 2 : -2;
        if (!this.pending && this.input.length === 0) {
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

    if (!this.inBracketedPaste && key.name === 'tab') {
      const current = this.input.join('');
      if (current.startsWith('/')) {
        const popup = this.slashPopup();
        const match = popup.matches[popup.index] ?? SLASH_COMMANDS.find(cmd => cmd.startsWith(current.trim()));
        if (match) {
          this.input = [...(match + ' ')];
          this.cursor = this.input.length;
          this.inputRestoredFromRevert = false;
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
          this.inputRestoredFromRevert = false;
          this.schedule();
        }
      }
      return;
    }

    if (!this.inBracketedPaste && (key.name === 'return' || key.name === 'enter')) {
      const popup = this.slashPopup();
      const selected = popup.matches[popup.index];
      if (selected && selected !== this.input.join('')) {
        this.input = [...selected];
        this.cursor = this.input.length;
      }
      this.collapsePastedImagePathsInInput();
      const rawJoined = this.input.join('');
      const value = this.expandInputPlaceholders(rawJoined);
      if (!this.pending) {
        const trimmed = value.trim();
        if (trimmed) {
          this.history.push(rawJoined.trimEnd());
          if (this.history.length > 100) this.history.shift();
          this.input = [];
          this.cursor = 0;
          this.inputRestoredFromRevert = false;
          this.attachedImages.clear();
          this.pastedTexts.clear();
          // Execute slash commands (/auto, /mode, /view, /transcript, /status, etc.) and !<cmd> immediately even while working!
          if ((trimmed.startsWith('/') || trimmed.startsWith('!')) && this.onImmediateCommand) {
            this.schedule(0);
            void this.onImmediateCommand(trimmed);
            return;
          }
          // Otherwise queue user prompts for the next turn (keeping compact placeholders for queue preview & Alt+Up)
          this.queuedPrompts.push(rawJoined.trimEnd());
          this.schedule(0);
        }
        return;
      }
      const pending = this.pending;
      this.pending = undefined;
      pending.cleanup();
      if (this.remember && value.trim()) {
        this.history.push(rawJoined.trimEnd());
        if (this.history.length > 100) this.history.shift();
      }
      this.input = [];
      this.cursor = 0;
      this.inputRestoredFromRevert = false;
      this.scroll = 0;
      this.attachedImages.clear();
      this.pastedTexts.clear();
      if (this.queuedPrompts.length === 0) {
        this.nextImageIdx = 1;
        this.nextPasteIdx = 1;
      }
      pending.resolve(value);
      this.schedule();
      return;
    }

    if (!this.inBracketedPaste) {
      if (key.name === 'left') {
        if (this.cursor > 0) {
          const before = this.input.slice(0, this.cursor).join('');
          const tokenMatch = ATOMIC_PLACEHOLDER_END_RE.exec(before);
          if (tokenMatch) {
            this.cursor = Math.max(0, this.cursor - [...tokenMatch[1]].length);
          } else {
            this.cursor = Math.max(0, this.cursor - 1);
          }
        }
      } else if (key.name === 'right') {
        if (this.cursor < this.input.length) {
          const after = this.input.slice(this.cursor).join('');
          const tokenMatch = ATOMIC_PLACEHOLDER_START_RE.exec(after);
          if (tokenMatch) {
            this.cursor = Math.min(this.input.length, this.cursor + [...tokenMatch[1]].length);
          } else {
            this.cursor = Math.min(this.input.length, this.cursor + 1);
          }
        }
      } else if (key.name === 'home' || (key.ctrl && key.name === 'a')) {
        this.cursor = 0;
      } else if (key.name === 'end' || (key.ctrl && key.name === 'e')) {
        if (this.scroll > 0 && this.cursor === this.input.length) {
          this.scroll = 0;
        } else {
          this.cursor = this.input.length;
        }
      } else if (key.name === 'backspace') {
        this.inputRestoredFromRevert = false;
        if (this.cursor > 0) {
          // Atomic deletion of [Image #N] or [Pasted text #N ...] placeholder if cursor is right after it
          const before = this.input.slice(0, this.cursor).join('');
          const tokenMatch = ATOMIC_PLACEHOLDER_END_RE.exec(before);
          if (tokenMatch) {
            const token = tokenMatch[1].trim();
            this.attachedImages.delete(token);
            this.pastedTexts.delete(token);
            if (this.attachedImages.size === 0 && this.queuedPrompts.length === 0) this.nextImageIdx = 1;
            if (this.pastedTexts.size === 0 && this.queuedPrompts.length === 0) this.nextPasteIdx = 1;
            const delLen = [...tokenMatch[1]].length;
            this.input.splice(this.cursor - delLen, delLen);
            this.cursor -= delLen;
          } else {
            this.input.splice(--this.cursor, 1);
          }
        }
      } else if (key.name === 'delete') {
        this.inputRestoredFromRevert = false;
        if (this.cursor < this.input.length) {
          const after = this.input.slice(this.cursor).join('');
          const tokenMatch = ATOMIC_PLACEHOLDER_START_RE.exec(after);
          if (tokenMatch) {
            const token = tokenMatch[1].trim();
            this.attachedImages.delete(token);
            this.pastedTexts.delete(token);
            if (this.attachedImages.size === 0 && this.queuedPrompts.length === 0) this.nextImageIdx = 1;
            if (this.pastedTexts.size === 0 && this.queuedPrompts.length === 0) this.nextPasteIdx = 1;
            const delLen = [...tokenMatch[1]].length;
            this.input.splice(this.cursor, delLen);
          } else {
            this.input.splice(this.cursor, 1);
          }
        }
      } else if (key.ctrl && key.name === 'w') {
        this.inputRestoredFromRevert = false;
        const before = this.input.slice(0, this.cursor).join('');
        const tokenMatch = ATOMIC_PLACEHOLDER_END_RE.exec(before);
        if (tokenMatch) {
          const token = tokenMatch[1].trim();
          this.attachedImages.delete(token);
          this.pastedTexts.delete(token);
          if (this.attachedImages.size === 0 && this.queuedPrompts.length === 0) this.nextImageIdx = 1;
          if (this.pastedTexts.size === 0 && this.queuedPrompts.length === 0) this.nextPasteIdx = 1;
          const delLen = [...tokenMatch[1]].length;
          this.input.splice(this.cursor - delLen, delLen);
          this.cursor -= delLen;
        } else {
          while (this.cursor > 0 && this.input[this.cursor - 1] === ' ') this.input.splice(--this.cursor, 1);
          while (this.cursor > 0 && this.input[this.cursor - 1] !== ' ') this.input.splice(--this.cursor, 1);
        }
      } else if (
        (key.name === 'up' || key.name === 'down' || (key.ctrl && (key.name === 'p' || key.name === 'n'))) &&
        this.remember
      ) {
        this.inputRestoredFromRevert = false;
        const delta = key.name === 'up' || key.name === 'p' ? -1 : 1;
        this.historyIndex = Math.min(this.history.length, Math.max(0, this.historyIndex + delta));
        this.input = [...(this.history[this.historyIndex] ?? '')];
        this.cursor = this.input.length;
        this.rehydratePlaceholdersInInput();
      } else if (key.ctrl && key.name === 'u') {
        this.inputRestoredFromRevert = false;
        if (this.input.length > 0) {
          this.input = [];
          this.cursor = 0;
          this.attachedImages.clear();
          this.pastedTexts.clear();
          if (this.queuedPrompts.length === 0) {
            this.nextImageIdx = 1;
            this.nextPasteIdx = 1;
          }
        } else {
          this.scrollBy(5);
        }
      } else if (text && !key.ctrl && !key.meta && this.input.length < 16_000) {
        this.inputRestoredFromRevert = false;
        const normalizedText = text.replace(/\r\n/g, '\n').replace(/\r/g, '\n');
        const trimmedTrailing =
          !normalizedText.slice(0, -1).includes('\n') && normalizedText.endsWith('\n')
            ? normalizedText.slice(0, -1)
            : normalizedText;
        if (trimmedTrailing.includes('\n') || [...trimmedTrailing].length > LARGE_PASTE_CHAR_THRESHOLD) {
          this.handlePaste(text);
          return;
        }
        const chars = [...terminalText(trimmedTrailing).replace(/\n/g, ' ')];
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

    // Inside bracketed paste: accumulate raw text & newlines in O(1) without mutating this.input until paste-end
    if (this.bracketedPasteBuffer.length < 512_000) {
      if (text) {
        this.bracketedPasteBuffer += text;
      } else if (key.name === 'return') {
        this.bracketedPasteBuffer += '\r';
      } else if (key.name === 'enter') {
        this.bracketedPasteBuffer += '\n';
      } else if (key.name === 'tab') {
        this.bracketedPasteBuffer += '\t';
      }
    }
  };

  private ended = () => {
    this.cancel();
    this.close();
  };

  private resize = () => {
    this.lastRenderedFrame = '';
    this.lastFrameRows = [];
    this.needsFullClear = true;
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
   * Transcript renderer. Parses the plain-text transcript (markers + glyph-prefixed cells) into typed
   * rows, wraps every row to `columns` BEFORE styling (prefixes included), then paints each row.
   * No emitted row can exceed the terminal width, so soft-wrap cursor drift is impossible.
   */
  private renderStyledBodyLines(columns: number, rich: boolean): string[] {
    const cols = Math.max(4, columns);
    const showReasoning = this.viewMode !== 'normal' || this.inTranscriptOverlay;
    const rows: Row[] = [];
    const lastKind = (): RowKind | undefined => rows[rows.length - 1]?.kind;
    const lastContentKind = (): RowKind | undefined => {
      for (let i = rows.length - 1; i >= 0; i--) if (rows[i].kind !== 'blank') return rows[i].kind;
      return undefined;
    };
    const blank = () => {
      if (rows.length && lastKind() !== 'blank') rows.push({ kind: 'blank', pre: '', text: '' });
    };
    const wrapInto = (kind: RowKind, text: string, first: string, cont: string, extra: Partial<Row> = {}, words = true) => {
      const chunks = wrapChunks(text, cols - visibleLength(first), cols - visibleLength(cont), words);
      chunks.forEach((chunk, i) => rows.push({ kind, pre: i === 0 ? first : cont, text: chunk, ...extra }));
    };

    /** Markdown-aware paragraph row. `base` is the indent every row starts with; `lead` replaces it on the first row. */
    const pushMarkdown = (rawLine: string, base: string, lead: string | undefined, paraKind: RowKind) => {
      const firstPre = (pre: string) => (lead !== undefined ? lead + pre.slice(base.length) : pre);
      const heading = /^\s*(#{1,4})\s+(.*)$/.exec(rawLine);
      if (heading) {
        wrapInto('heading', heading[2], firstPre(base), base, { level: heading[1].length });
        return;
      }
      if (/^\s*(?:-{3,}|\*{3,}|_{3,})\s*$/.test(rawLine)) {
        rows.push({ kind: 'rule', pre: firstPre(base), text: '─'.repeat(Math.max(1, Math.min(48, cols - base.length - 2))) });
        return;
      }
      const bullet = /^(\s*)[-*+]\s+(.*)$/.exec(rawLine);
      if (bullet) {
        const pre = `${base}${bullet[1]}• `;
        wrapInto(paraKind, bullet[2], firstPre(pre), ' '.repeat(visibleLength(pre)));
        return;
      }
      const numbered = /^(\s*)(\d{1,3})[.)]\s+(.*)$/.exec(rawLine);
      if (numbered) {
        const pre = `${base}${numbered[1]}${numbered[2]}. `;
        wrapInto(paraKind, numbered[3], firstPre(pre), ' '.repeat(visibleLength(pre)));
        return;
      }
      const quote = /^\s*>\s?(.*)$/.exec(rawLine);
      if (quote) {
        wrapInto('quote', quote[1], firstPre(`${base}▎ `), `${base}▎ `);
        return;
      }
      if (isTableRow(rawLine)) {
        const trimmedRow = rawLine.trim();
        const widths = tableWidths.get(lineIndex) ?? [];
        if (TABLE_SEPARATOR_RE.test(trimmedRow)) {
          const grid = `├${widths.map(w => '─'.repeat(w + 2)).join('┼')}┤`;
          rows.push({ kind: 'table', pre: firstPre(base), text: sliceVisible(grid, Math.max(1, cols - base.length)) });
        } else {
          const cells = splitTableCells(trimmedRow).map((c, i) => c + ' '.repeat(Math.max(0, (widths[i] ?? 0) - visibleLength(displayedCell(c)))));
          rows.push({ kind: 'table', pre: firstPre(base), text: sliceVisible(`│ ${cells.join(' │ ')} │`, Math.max(1, cols - base.length)) });
        }
        return;
      }
      const indent = /^\s*/.exec(rawLine)?.[0] ?? '';
      wrapInto(paraKind, rawLine.slice(indent.length), firstPre(base + indent), base + indent);
    };

    // Pre-scan contiguous Markdown table blocks so every row of a table shares column widths.
    const rawLines = this.transcript.split('\n');
    const tableWidths = new Map<number, number[]>();
    for (let i = 0; i < rawLines.length; i++) {
      if (!isTableRow(rawLines[i])) continue;
      let j = i;
      const widths: number[] = [];
      while (j < rawLines.length && isTableRow(rawLines[j])) {
        const trimmedRow = rawLines[j].trim();
        if (!TABLE_SEPARATOR_RE.test(trimmedRow)) {
          splitTableCells(trimmedRow).forEach((c, k) => {
            widths[k] = Math.max(widths[k] ?? 0, visibleLength(displayedCell(c)));
          });
        }
        j++;
      }
      for (let k = i; k < j; k++) tableWidths.set(k, widths);
      i = j - 1;
    }

    let block: 'normal' | 'answer' | 'reasoning' = 'normal';
    let inCode = false;
    let inDiff = false;
    let answerFirst = false;
    let lastWasUser = false;
    let lineIndex = -1;

    for (const rawLine of rawLines) {
      lineIndex++;
      const trimmed = rawLine.trim();
      if (rawLine === '[normal]') {
        block = 'normal';
        inCode = false;
        inDiff = false;
        answerFirst = false;
        continue;
      }
      if (rawLine.startsWith('[reasoning')) {
        block = 'reasoning';
        inCode = false;
        inDiff = false;
        if (showReasoning) {
          blank();
          rows.push({ kind: 'reasoningHead', pre: '', text: `${GLYPH.thinking} Thinking` });
        }
        continue;
      }
      if (rawLine.startsWith('[answer]')) {
        blank();
        block = 'answer';
        inCode = false;
        inDiff = false;
        answerFirst = true;
        continue;
      }
      if (rawLine.startsWith(USER_MARK)) {
        if (!lastWasUser) blank();
        wrapInto('user', rawLine.slice(USER_MARK.length), lastWasUser ? '   ' : ` ${GLYPH.user} `, '   ');
        lastWasUser = true;
        block = 'normal';
        inCode = false;
        inDiff = false;
        continue;
      }
      if (lastWasUser) {
        lastWasUser = false;
        blank();
      }
      if (block === 'reasoning' && !showReasoning) continue;

      if (trimmed.startsWith('```')) {
        if (inCode) {
          inCode = false;
          rows.push({ kind: 'codeFoot', pre: '', text: '' });
        } else {
          inCode = true;
          inDiff = false;
          answerFirst = false;
          rows.push({ kind: 'codeHead', pre: '', text: trimmed.slice(3).trim() || 'code' });
        }
        continue;
      }
      if (inCode) {
        wrapInto('code', rawLine.replace(/\t/g, '  ').trimEnd(), '   ', '   ', {}, false);
        continue;
      }

      if (!trimmed) {
        if (block === 'reasoning') continue;
        if (block === 'answer' && answerFirst) continue;
        blank();
        continue;
      }

      if (block === 'reasoning') {
        wrapInto('reasoning', rawLine.trimEnd(), '  ', '  ');
        continue;
      }

      if (block === 'answer') {
        const lead = answerFirst ? `${GLYPH.answer} ` : undefined;
        answerFirst = false;
        pushMarkdown(rawLine.trimEnd(), '  ', lead, 'answer');
        continue;
      }

      // ── normal block: tool cells, diffs, CLI output, banner ──
      const cell = CELL_RE.exec(rawLine);
      if (cell) {
        blank();
        inDiff = cell[1] === GLYPH.edit || /^(?:Edited|Added|Deleted|Wrote|Created|Updated)\b/.test(cell[2]);
        wrapInto('cell', cell[2].trimEnd(), `${cell[1]} `, '  ', { glyph: cell[1] });
        continue;
      }
      if (/^(?:--- |\+\+\+ |@@ )/.test(rawLine)) {
        inDiff = true;
        rows.push({ kind: 'diffMeta', pre: '  ', text: sliceVisible(rawLine.trimEnd(), Math.max(1, cols - 2)) });
        continue;
      }
      if (inDiff) {
        const numbered = /^ {4}([ \d]{4}) ([+\- ])(.*)$/.exec(rawLine);
        const unified = numbered ? undefined : /^([+\- ])(.*)$/.exec(rawLine);
        if (numbered || unified) {
          const sign = (numbered ? numbered[2] : unified![1]) as '+' | '-' | ' ';
          const code = (numbered ? numbered[3] : unified![2]).replace(/\t/g, '  ').trimEnd();
          const pre = numbered ? `    ${numbered[1]} ${sign}` : `  ${sign}`;
          wrapInto('diff', code, pre, ' '.repeat(visibleLength(pre)), { sign }, false);
          continue;
        }
        if (/^\s*(?:⋮|… \+\d+ more|\.\.\. \(\+\d+ more)/.test(rawLine) || rawLine.startsWith('\\ No newline')) {
          rows.push({ kind: 'diffMeta', pre: '    ', text: sliceVisible(trimmed, Math.max(1, cols - 4)) });
          continue;
        }
        inDiff = false;
      }

      const prevKind = lastContentKind();
      if ((prevKind === 'cell' || prevKind === 'child') && rawLine.startsWith('  ')) {
        const child = /^ {2}(└ | {2})?(.*)$/.exec(rawLine.trimEnd());
        const connector = child?.[1] === '└ ' ? '  └ ' : child?.[1] ? '    ' : '  ';
        wrapInto('child', child?.[2] ?? trimmed, connector, '    ');
        continue;
      }

      if (/^[╭│╰]/.test(rawLine)) {
        rows.push({ kind: 'box', pre: '', text: sliceVisible(rawLine.trimEnd(), cols) });
        continue;
      }
      if (rawLine.startsWith('Type /help')) {
        wrapInto('hint', rawLine.trimEnd(), '', '');
        continue;
      }
      pushMarkdown(rawLine.trimEnd(), '', undefined, 'normal');
    }

    return rows.map(row => (rich ? this.styleRow(row, cols) : plainRow(row, cols)));
  }

  /** Paints one typed transcript row. Every branch preserves the row's visible width. */
  private styleRow(row: Row, cols: number): string {
    const T = THEME;
    const R = T.reset;
    const md = (text: string, base: string) => `${base}${formatInlineMarkdown(text, `${R}${base}`)}${R}`;
    const prefix = (pre: string, tone: string) => (pre.trim() ? `${tone}${pre}${R}` : pre);
    const answerPre = (pre: string) =>
      pre.startsWith(GLYPH.answer) ? `${T.brandBold}${GLYPH.answer}${R}${prefix(pre.slice(1), T.bullet)}` : prefix(pre, T.bullet);

    switch (row.kind) {
      case 'blank':
        return '';
      case 'user': {
        const base = `${T.userBg}${T.strong}`;
        const pre = row.pre.trim() ? `${T.userBg}${T.brandBold}${row.pre}${R}` : `${T.userBg}${row.pre}`;
        return padRightAnsi(`${pre}${base}${formatInlineMarkdown(row.text, `${R}${base}`)}`, cols, T.userBg);
      }
      case 'answer':
        return `${answerPre(row.pre)}${md(row.text, T.text)}`;
      case 'normal':
        return `${prefix(row.pre, T.bullet)}${formatInlineMarkdown(row.text)}`;
      case 'heading': {
        const tone = row.level === 1 ? `${T.brandBold}\x1b[4m` : T.brandBold;
        return `${answerPre(row.pre)}${md(row.text, tone)}`;
      }
      case 'quote': {
        const pre = row.pre.startsWith(GLYPH.answer)
          ? `${T.brandBold}${GLYPH.answer}${R}${T.faint}${row.pre.slice(1)}${R}`
          : `${T.faint}${row.pre}${R}`;
        return `${pre}${md(row.text, `${T.muted}\x1b[3m`)}`;
      }
      case 'rule':
        return `${answerPre(row.pre)}${T.faint}${row.text}${R}`;
      case 'table':
        if (row.text.startsWith('├')) return `${answerPre(row.pre)}${T.faint}${row.text}${R}`;
        return `${answerPre(row.pre)}${row.text
          .split('│')
          .map(cell => formatInlineMarkdown(cell))
          .join(`${T.faint}│${R}`)}`;
      case 'reasoningHead':
        return `${T.thinking}\x1b[3m${row.text}${R}`;
      case 'reasoning':
        return `${row.pre}${md(row.text, `${T.faint}\x1b[3m`)}`;
      case 'codeHead':
        return `  ${padRightAnsi(`${T.faint} ${sliceVisible(row.text, Math.max(1, cols - 4))}`, cols - 2, T.codeBg)}`;
      case 'code':
        return `  ${padRightAnsi(`${T.codeText} ${row.text}`, cols - 2, T.codeBg)}`;
      case 'codeFoot':
        return `  ${padRightAnsi('', cols - 2, T.codeBg)}`;
      case 'cell':
        return this.styleCell(row);
      case 'child':
        return this.styleChild(row);
      case 'diff': {
        const bg = row.sign === '+' ? T.addBg : row.sign === '-' ? T.delBg : '';
        const fg = row.sign === '+' ? T.addFg : row.sign === '-' ? T.delFg : T.muted;
        const num = /^(\s*\d*)/.exec(row.pre)?.[1] ?? '';
        const content = `${T.faint}${num}${R}${bg}${fg}${row.pre.slice(num.length)}${row.text}`;
        return bg ? padRightAnsi(content, cols, bg) : `${content}${R}`;
      }
      case 'diffMeta':
        return `${row.pre}${row.text.startsWith('@@') ? T.info : T.faint}${row.text}${R}`;
      case 'box':
        return styleBoxLine(row.text);
      case 'hint':
        return `${T.faint}${row.text}${R}`;
    }
  }

  private styleCell(row: Row): string {
    const T = THEME;
    const R = T.reset;
    const glyph = row.glyph ?? '•';
    const tone = CELL_TONES[glyph] ?? T.muted;
    const withDurations = (text: string, base: string) =>
      text
        .replace(/\((?:\d+m )?\d+s\)/g, m => `${T.faint}${m}${R}${base}`)
        .replace(/\(\+(\d+) -(\d+)\)/g, (_, a: string, d: string) => `${T.faint}(${R}${T.ok}+${a}${R} ${T.err}-${d}${R}${T.faint})${R}${base}`);
    if (!row.pre.trim()) return `${row.pre}${T.muted}${withDurations(row.text, T.muted)}${R}`;
    const match = /^(\S+)(.*)$/.exec(row.text);
    const verb = match?.[1] ?? row.text;
    const rest = match?.[2] ?? '';
    const glyphOut = `${tone}\x1b[1m${glyph}${R} `;
    if (glyph === GLYPH.allow) return `${glyphOut}${T.warn}\x1b[1m${row.text}${R}`;
    if (glyph === GLYPH.compact) return `${glyphOut}${T.faint}${row.text}${R}`;
    if (glyph === GLYPH.prompt) return `${glyphOut}${T.brandBold}${row.text}${R}`;
    const verbTone = glyph === GLYPH.failed ? `${T.err}\x1b[1m` : T.strong;
    const restBase = glyph === GLYPH.ran ? T.info : glyph === GLYPH.edit ? T.text : T.muted;
    return `${glyphOut}${verbTone}${verb}${R}${restBase}${withDurations(rest, restBase)}${R}`;
  }

  private styleChild(row: Row): string {
    const T = THEME;
    const R = T.reset;
    const pre = row.pre.includes('└') ? `${T.faint}${row.pre}${R}` : row.pre;
    const text = row.text;
    const explored = /^(Read|List|Search)\b(.*)$/.exec(text);
    if (explored) return `${pre}${T.info}${explored[1]}${R}${T.text}${explored[2]}${R}`;
    if (text.startsWith('✓ ')) return `${pre}${T.ok}✓${R} ${T.faint}${text.slice(2)}${R}`;
    if (text.startsWith('▸ ')) return `${pre}${T.brandBold}▸ ${text.slice(2)}${R}`;
    if (text.startsWith('○ ')) return `${pre}${T.faint}○${R} ${T.text}${text.slice(2)}${R}`;
    if (text.startsWith('$ ')) return `${pre}${T.faint}$${R} ${T.info}\x1b[1m${text.slice(2)}${R}`;
    if (/^… \+\d+/.test(text)) return `${pre}${T.faint}${text}${R}`;
    const option = /^(\d{1,2})\. (.*)$/.exec(text);
    if (option) return `${pre}${T.brandBold}${option[1]}.${R} ${formatInlineMarkdown(option[2])}`;
    return `${pre}${T.muted}${text}${R}`;
  }

  /**
   * Flushes `screenLines` to the terminal using Ratatui/Codex-style Row-Level Differential Rendering.
   * Never emits `\x1b[2J` after the initial frame (unless `clear()` or terminal resize occurred),
   * and NEVER touches unchanged rows so native mouse text selection (`bôi đen`) is never reset while working.
   */
  private flushScreenLines(screenLines: string[], columns: number, rows: number, cursorSeq: string) {
    if (
      this.needsFullClear ||
      columns !== this.lastCols ||
      rows !== this.lastRows ||
      this.lastFrameRows.length !== screenLines.length
    ) {
      this.needsFullClear = false;
      this.lastCols = columns;
      this.lastRows = rows;
      this.lastFrameRows = [...screenLines];
      this.lastCursorSeq = cursorSeq;
      const fullFrame =
        `\x1b[?2026h\x1b[?25l\x1b[H\x1b[2J${screenLines.map(l => `${l}\x1b[K`).join('\r\n')}` +
        cursorSeq +
        `\x1b[?2026l`;
      this.lastRenderedFrame = fullFrame;
      this.target.write(fullFrame);
      return;
    }

    const patches: string[] = [];
    for (let i = 0; i < screenLines.length; i++) {
      if (screenLines[i] !== this.lastFrameRows[i]) {
        patches.push(`\x1b[${i + 1};1H\x1b[0m${screenLines[i]}\x1b[K`);
        this.lastFrameRows[i] = screenLines[i];
      }
    }

    if (patches.length === 0 && cursorSeq === this.lastCursorSeq) {
      return;
    }
    this.lastCursorSeq = cursorSeq;
    const diffFrame = `\x1b[?2026h\x1b[?25l${patches.join('')}${cursorSeq}\x1b[?2026l`;
    this.lastRenderedFrame = this.lastFrameRows.join('\r\n');
    this.target.write(diffFrame);
  }

  /**
   * Builds a full-width header/footer band: left segments are clipped last, right groups are dropped
   * (front first) when the terminal is too narrow, so the band never exceeds `cols` visible columns.
   */
  private band(left: Seg[], right: Seg[][], cols: number, rich: boolean): string {
    const T = THEME;
    const sep: Seg = [' · ', T.faint];
    const flatten = (groups: Seg[][]): Seg[] => groups.flatMap((g, i) => (i === 0 ? g : [sep, ...g]));
    const plainWidth = (segs: Seg[]) => visibleLength(segs.map(s => s[0]).join(''));
    const leftMin = Math.min(plainWidth(left), 12);
    const groups = right.filter(g => g.length > 0);
    while (groups.length && leftMin + plainWidth(flatten(groups)) + 2 > cols) groups.shift();
    const rightSegs = flatten(groups);
    if (rightSegs.length) rightSegs.push([' ', '']);
    const rightWidth = plainWidth(rightSegs);
    const leftAvail = Math.max(0, cols - rightWidth - (rightWidth ? 1 : 0));
    const paint = (text: string, sgr: string) => (rich && sgr ? `${sgr}${text}${T.reset}${T.bandBg}` : text);
    let used = 0;
    let leftOut = '';
    for (const [text, sgr] of left) {
      const room = leftAvail - used;
      if (room <= 0) break;
      const piece = visibleLength(text) > room ? sliceVisible(text, room) : text;
      leftOut += paint(piece, sgr);
      used += visibleLength(piece);
    }
    const gap = ' '.repeat(Math.max(0, cols - used - rightWidth));
    const rightOut = rightSegs.map(([text, sgr]) => paint(text, sgr)).join('');
    return rich ? `${T.bandBg}${leftOut}${gap}${rightOut}${T.reset}` : `${leftOut}${gap}${rightOut}`.trimEnd();
  }

  private contextGroup(): Seg[] {
    if (!this.contextWindow || this.contextUsed === undefined) return [];
    const T = THEME;
    const pct = Math.max(0, Math.min(100, Math.round((this.contextUsed / this.contextWindow) * 100)));
    const cells = 8;
    const filled = Math.min(cells, Math.round((pct / 100) * cells));
    const tone = pct >= 90 ? T.err : pct >= 70 ? T.warn : T.brand;
    return [
      ['ctx ', T.faint],
      ['▰'.repeat(filled), tone],
      ['▱'.repeat(cells - filled), T.faint],
      [` ${pct}%`, pct >= 70 ? tone : T.muted],
    ];
  }

  private render() {
    if (this.closed) return;
    const T = THEME;
    const columns = Math.max(4, this.target.columns || 80);
    const rows = Math.max(8, this.target.rows || 24);
    const safeCols = Math.max(3, columns - 1);
    const clip = (s: string) => wrapTerminal(s, safeCols)[0];
    const currentInput = this.input.join('');
    const busy = this.status === 'Running' || this.status === 'Connecting MCP' || this.status === 'Compacting';
    const indicator = busy ? SPINNER[this.spinTick % SPINNER.length] : '●';
    const rich = columns >= 48 && rows >= 12;
    const asking = this.label !== 'goon> ';

    const titleParts = this.title.split(' · ');
    const modelName = titleParts[0] || this.title;
    const sessionLabel = titleParts.slice(1).join(' · ') || 'session';
    const elapsedStr = this.turnStartedAt ? `${formatCompactDuration(Date.now() - this.turnStartedAt)} • ` : '';

    // Body lines with strict width bound & scroll-anchor lock when scrolled up
    const all = this.renderStyledBodyLines(safeCols, rich);
    if (this.scroll > 0 && this.lastBodyTotalLines > 0 && all.length > this.lastBodyTotalLines) {
      this.scroll += all.length - this.lastBodyTotalLines;
    }
    this.lastBodyTotalLines = all.length;

    const todoGroup: Seg[] = this.todoLabel ? [[`${GLYPH.todo} `, T.brand], [this.todoLabel, T.muted]] : [];
    const brandSegs: Seg[] = [[' ', ''], [GLYPH.brand, T.brandBold], [' goon', T.strong]];

    // Full-screen Transcript overlay (Ctrl+T or /transcript)
    if (this.inTranscriptOverlay) {
      const txHeader = this.band(
        [...brandSegs, [' · ', T.faint], ['TRANSCRIPT', T.brandBold], [' · ', T.faint], [sessionLabel, T.muted]],
        [todoGroup],
        safeCols,
        rich,
      );
      const height = Math.max(2, rows - 2);
      this.maxScroll = Math.max(0, all.length - height);
      this.scroll = Math.min(this.scroll, this.maxScroll);
      const end = all.length - this.scroll;
      const body = all.slice(Math.max(0, end - height), end);
      while (body.length < height) body.push('');

      const live: Seg[] = busy
        ? [[`${indicator} `, T.brand], ['Working', T.strong], [` (${elapsedStr}esc to interrupt)`, T.faint], [' · ', T.faint]]
        : [];
      const txFooter = this.band(
        [[' ', ''], ...live, [`line ${Math.max(1, end)}/${Math.max(1, all.length)}`, T.muted]],
        [[['↑↓ PgUp/PgDn scroll · Home/End · q / Esc / Ctrl+T back', T.faint]]],
        safeCols,
        rich,
      );
      this.flushScreenLines([txHeader, ...body, txFooter], columns, rows, '');
      return;
    }

    // Header band: brand · workspace · branch · session  ……  todo · context meter
    const leftHeader: Seg[] = [...brandSegs];
    if (this.workspaceName) leftHeader.push([' · ', T.faint], [this.workspaceName, T.text]);
    if (this.branch) leftHeader.push([' · ', T.faint], [this.branch, T.info]);
    leftHeader.push([' · ', T.faint], [sessionLabel, T.muted]);
    const header = rich ? this.band(leftHeader, [todoGroup, this.contextGroup()], safeCols, rich) : clip(`goon · ${sessionLabel}`);

    // Live status row (Working / scroll / esc hints / attachments / notices)
    let statusLine = '';
    if (this.scroll > 0) {
      const text = clip(`↑ Scrolled up ${this.scroll}/${this.maxScroll} lines · wheel/↑↓ scroll · ctrl+t transcript · End/Esc bottom`);
      statusLine = rich ? `${T.info}${text}${T.reset}` : text;
    } else if (this.escPrimed === 'backtrack' || this.escPrimed === 'clear') {
      const text = clip(this.escPrimed === 'backtrack' ? 'Press Esc again to rewind & edit previous prompt' : 'Press Esc again to clear input');
      statusLine = rich ? `${T.warn}${text}${T.reset}` : text;
    } else if ((this.attachedImages.size > 0 || this.pastedTexts.size > 0) && !busy) {
      const imgItems = [...this.attachedImages.entries()].map(([k, p]) => `${k}: ${basename(p)}`);
      const pasteItems = [...this.pastedTexts.keys()];
      const items = [...imgItems, ...pasteItems].join(' · ');
      const text = clip(`📎 Attached ${items} (Enter to send · Backspace to remove)`);
      statusLine = rich ? `${T.info}${text}${T.reset}` : text;
    } else if (busy) {
      const toolsText = `• Working (${elapsedStr}esc to interrupt · ctrl+t transcript)`;
      const body = wrapTerminal(toolsText.slice(2), Math.max(1, safeCols - 2))[0];
      statusLine = rich
        ? `${T.brandBold}${indicator}${T.reset} ${T.strong}${body.slice(0, 7)}${T.reset}${T.faint}${body.slice(7)}${T.reset}`
        : clip(toolsText);
    } else if (this.statusNotice) {
      const text = clip(this.statusNotice);
      statusLine = rich ? `${T.muted}${text}${T.reset}` : text;
    } else if (!(currentInput.startsWith('/') && !/\s/.test(currentInput))) {
      const text = clip([...this.toolStates.values()].slice(-2).join(' | '));
      statusLine = rich && text ? `${T.faint}${text}${T.reset}` : text;
    }

    if (!rich) {
      // Compact fallback for tiny (<48x12) terminals
      const footer = clip(`${this.status} · ${this.tokens} tokens · Ctrl+C cancel · PgUp/PgDn scroll`);
      const height = Math.max(2, rows - 4);
      this.maxScroll = Math.max(0, all.length - height);
      this.scroll = Math.min(this.scroll, this.maxScroll);
      const end = all.length - this.scroll;
      const body = all.slice(Math.max(0, end - height), end);
      while (body.length < height) body.push('');
      const label = clip(this.label);
      const labelCols = visibleLength(label);
      const inputAvailCols = Math.max(1, safeCols - labelCols);
      const { visibleBeforeCursor, shownInput } = this.sliceInputWindow(inputAvailCols);
      const prompt = wrapTerminal(label + shownInput, safeCols)[0];
      const cursorColumn = Math.min(columns, labelCols + visibleLength(visibleBeforeCursor) + 1);
      const screenLines = [header, ...body, statusLine, footer, prompt];
      const cursorSeq = this.pending ? `\x1b[${screenLines.length};${cursorColumn}H\x1b[?25h` : '';
      this.flushScreenLines(screenLines, columns, rows, cursorSeq);
      return;
    }

    // Queued follow-ups (shown without a 'Queue' label)
    const queueLines =
      this.queuedPrompts.length > 0
        ? [`${T.faint}\x1b[3m${clip(`  ↳ ${this.queuedPrompts.join(' · ')} (Alt+↑ edit)`)}${T.reset}`]
        : [];

    // Slash-command popup: live-filtered list with descriptions above the composer (↑↓ select, Tab/Enter accept)
    const fixedRows = 1 + 1 + 1 + queueLines.length + 3; // header + gap + status + queue + composer box
    const popupLines: string[] = [];
    const popup = this.slashPopup();
    if (popup.matches.length) {
      const { matches, index: selected } = popup;
      const room = Math.max(0, Math.min(6, rows - fixedRows - 3));
      const shownCount = matches.length > room ? Math.max(0, room - 1) : matches.length;
      const start = Math.min(Math.max(0, selected - shownCount + 1), Math.max(0, matches.length - shownCount));
      const windowed = matches.slice(start, start + shownCount);
      const nameCols = Math.max(8, ...windowed.map(c => c.length)) + 2;
      for (const [offset, cmd] of windowed.entries()) {
        const isSelected = start + offset === selected;
        const desc = SLASH_DESCRIPTIONS[cmd] ?? '';
        const marker = isSelected ? '› ' : '  ';
        const plain = clip(`  ${marker}${cmd.padEnd(nameCols)}${desc}`);
        const nameEnd = 4 + nameCols;
        popupLines.push(
          isSelected
            ? `${T.brandBold}${plain.slice(0, nameEnd)}${T.reset}${T.muted}${plain.slice(nameEnd)}${T.reset}`
            : `${T.text}${plain.slice(0, nameEnd)}${T.reset}${T.faint}${plain.slice(nameEnd)}${T.reset}`,
        );
      }
      if (matches.length > shownCount && room > 0) {
        popupLines.push(`${T.faint}${clip(`    ${selected + 1}/${matches.length} · ↑↓ select · Tab/Enter accept`)}${T.reset}`);
      }
    }

    const chromeRows = fixedRows + popupLines.length;
    const height = Math.max(2, rows - chromeRows);
    this.maxScroll = Math.max(0, all.length - height);
    this.scroll = Math.min(this.scroll, this.maxScroll);
    const end = all.length - this.scroll;
    const body = all.slice(Math.max(0, end - height), end);
    while (body.length < height) body.push('');

    // Composer box ─ border color carries state: brand (idle), faint (working), amber (awaiting an answer)
    const borderColor = asking && this.pending ? T.warn : busy ? T.faint : T.brand;
    const promptLabel = asking ? clip(this.label) : '› ';
    const labelCols = visibleLength(promptLabel);
    const boxInnerCols = Math.max(1, safeCols - 4 - labelCols);

    const { visibleBeforeCursor, shownInput } = this.sliceInputWindow(boxInnerCols);
    const isEmptyInput = this.input.length === 0 && !asking;
    const placeholderText = busy
      ? 'Type to queue a follow-up · /commands run immediately'
      : 'Ask anything · / commands · @ files · $ skills · ! shell';
    const placeholder = isEmptyInput ? wrapTerminal(placeholderText, boxInnerCols)[0] : '';
    const innerContent = placeholder ? `${T.faint}${placeholder}${T.reset}` : formatInlineMarkdown(shownInput);
    const innerPad = ' '.repeat(Math.max(0, boxInnerCols - visibleLength(placeholder || shownInput)));

    const displayMode = this.modeLabel === 'auto' ? 'auto-approve' : this.modeLabel;
    const modeTone =
      displayMode === 'auto-approve' ? T.ok : displayMode === 'plan' ? T.warn : displayMode === 'readonly' ? T.muted : T.info;
    const viewBadge = this.viewMode === 'transcript' ? ' · view:transcript' : '';
    const metaPlain = sliceVisible(` ${displayMode} · ${modelName} · effort:${this.effortLabel}${viewBadge} `, Math.max(1, safeCols - 5));
    const topDashes = '─'.repeat(Math.max(1, safeCols - 3 - visibleLength(metaPlain)));
    let metaStyled = `${T.muted}${metaPlain}${T.reset}`;
    if (metaPlain.startsWith(` ${displayMode}`)) {
      const afterMode = metaPlain.slice(displayMode.length + 1);
      const modelPart = afterMode.startsWith(` · ${modelName}`) ? ` · ${modelName}` : '';
      metaStyled =
        ` ${modeTone}\x1b[1m${displayMode}${T.reset}` +
        (modelPart ? `${T.faint} · ${T.reset}${T.strong}${modelName}${T.reset}` : '') +
        `${T.muted}${afterMode.slice(modelPart.length)}${T.reset}`;
    }
    const boxTop = `${borderColor}╭─${T.reset}${metaStyled}${borderColor}${topDashes}╮${T.reset}`;

    const labelStyled = asking ? `${T.warn}\x1b[1m${promptLabel}${T.reset}` : `${T.brandBold}${promptLabel}${T.reset}`;
    const boxMid = `${borderColor}│${T.reset} ${labelStyled}${innerContent}${innerPad} ${borderColor}│${T.reset}`;

    const tokenBadge = ` ${formatTokenCount(this.tokens)} tokens `;
    const hintText = asking
      ? ` enter submit · esc ${this.escHint} `
      : busy
        ? ' enter queues · esc interrupt '
        : ' shift+tab mode · ctrl+o copy · ctrl+t transcript ';
    let hint = hintText;
    let badge = tokenBadge;
    if (safeCols - 4 - visibleLength(hint) - visibleLength(badge) < 1) hint = '';
    if (safeCols - 4 - visibleLength(hint) - visibleLength(badge) < 1) badge = '';
    const botDashes = '─'.repeat(Math.max(1, safeCols - 4 - visibleLength(hint) - visibleLength(badge)));
    const boxBot =
      `${borderColor}╰─${T.reset}${T.faint}${hint}${T.reset}${borderColor}${botDashes}${T.reset}` +
      `${T.muted}${badge}${T.reset}${borderColor}─╯${T.reset}`;

    const screenLines = [header, ...body, '', statusLine, ...popupLines, ...queueLines, boxTop, boxMid, boxBot];
    const boxMidRow = screenLines.length - 1;
    const boxCursorCol = Math.min(safeCols - 1, 2 + labelCols + visibleLength(visibleBeforeCursor) + 1);
    const cursorSeq = this.pending || this.input.length > 0 ? `\x1b[${boxMidRow};${boxCursorCol}H\x1b[?25h` : '';
    this.flushScreenLines(screenLines, columns, rows, cursorSeq);
  }

  /**
   * Computes the visible composer slice around `this.cursor` in O(availCols) time by walking backwards
   * from the cursor instead of O(input.length^2) prefix slicing.
   */
  private sliceInputWindow(availCols: number): { visibleBeforeCursor: string; shownInput: string } {
    const maxBeforeCols = Math.max(0, availCols - 1);
    let startIdx = this.cursor;
    let usedBefore = 0;
    while (startIdx > 0) {
      const ch = this.input[startIdx - 1];
      const w = width(ch);
      if (usedBefore + w > maxBeforeCols) break;
      usedBefore += w;
      startIdx--;
    }
    const visibleBeforeCursor = this.input.slice(startIdx, this.cursor).join('');
    const afterCursor = this.input.slice(this.cursor, this.cursor + availCols + 4).join('');
    const shownInput = wrapTerminal(visibleBeforeCursor + afterCursor, availCols)[0];
    return { visibleBeforeCursor, shownInput };
  }

  close() {
    if (this.closed) return;
    this.closed = true;
    clearTimeout(this.timer);
    if (this.pasteBurstTimer) clearTimeout(this.pasteBurstTimer);
    if (this.escPrimeTimer) clearTimeout(this.escPrimeTimer);
    if (this.spinTimer) {
      clearInterval(this.spinTimer);
      this.spinTimer = undefined;
    }
    this.reject(new Error('Terminal closed.'));
    this.source.removeListener('data', this.beforeRawData);
    this.source.removeListener('keypress', this.keypress);
    this.source.removeListener('data', this.afterRawData);
    this.source.removeListener('end', this.ended);
    this.target.removeListener('resize', this.resize);
    this.source.setRawMode(this.oldRaw);
    this.source.pause();
    this.target.write('\x1b[?2004l\x1b[?1007l\x1b[?1006l\x1b[?1000l\x1b[?25h\x1b[?1049l');
  }
}
