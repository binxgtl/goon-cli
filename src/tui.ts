import { emitKeypressEvents, type Key } from 'node:readline';
import { stripVTControlCharacters } from 'node:util';
import type { ReadStream, WriteStream } from 'node:tty';
import type { HarnessEvent } from './harness.js';
import type { SubagentEvent } from './subagents.js';

// Strip whole escape sequences before removing remaining control bytes.
export function terminalText(value: string): string {
  // oxlint-disable-next-line eslint/no-control-regex
  return stripVTControlCharacters(value).replace(/[\u0000-\u0008\u000b-\u001f\u007f-\u009f]/g, '');
}
const width = (c: string) => /\p{Mark}/u.test(c) ? 0 : /[\p{Extended_Pictographic}\u1100-\u115f\u2e80-\ua4cf\uac00-\ud7a3\uf900-\ufaff\ufe10-\ufe6f\uff01-\uff60]/u.test(c) ? 2 : 1;
export function wrapTerminal(text: string, columns: number): string[] {
  const lines = ['']; let used = 0;
  for (const c of terminalText(text).replace(/\t/g, '  ')) {
    if (c === '\n') { lines.push(''); used = 0; continue; }
    const size = width(c);
    if (used + size > columns) { lines.push(''); used = 0; }
    lines[lines.length - 1] += c; used += size;
  }
  return lines;
}
interface Pending { resolve: (value: string) => void; reject: (error: Error) => void; cleanup: () => void }
export class TerminalUI {
  private transcript = '';
  private input: string[] = [];
  private cursor = 0;
  private scroll = 0;
  private label = 'goon> ';
  private status = 'Ready';
  private tokens = 0;
  private pending?: Pending;
  private closed = false;
  private timer?: NodeJS.Timeout;
  private oldRaw: boolean;
  private history: string[] = [];
  private historyIndex = 0;
  private remember = false;
  private toolStates = new Map<string, string>();
  private outputChannel?: 'text' | 'reasoning';
  private reasoningSource?: 'inline' | 'provider';
  constructor(readonly title: string, readonly cancel: () => void,
    readonly source: ReadStream = process.stdin, readonly target: WriteStream = process.stdout) {
    this.oldRaw = source.isRaw;
    emitKeypressEvents(source); source.setRawMode(true); source.resume();
    source.on('keypress', this.keypress); source.on('end', this.ended); target.on('resize', this.resize);
    target.write('\x1b[?1049h\x1b[?25l'); this.render();
  }
  write(text: string) {
    this.transcript = (this.transcript + terminalText(text)).slice(-100_000);
    this.scroll = 0; this.schedule();
  }
  event(event: HarnessEvent | SubagentEvent) {
    if (event.type === 'subagent') {
      if (event.state === 'progress' && event.event.type === 'usage') this.tokens += event.event.total;
      const key = `agent:${event.id}`;
      const state = event.state === 'progress' ? event.event.type === 'tool' ? `${event.event.name}: ${event.event.state}` : 'running' : event.state;
      this.toolStates.set(key, `agent ${event.id.slice(0, 8)}: ${state}`);
      if (event.state === 'start') this.write(`\n[agent ${event.id.slice(0, 8)}] ${event.task.slice(0, 160)}\n`);
    } else if (event.type === 'text' || event.type === 'reasoning') {
      if (event.type === 'reasoning' && (this.outputChannel !== 'reasoning' || this.reasoningSource !== event.source)) this.write(`\n[reasoning · ${event.source}]\n`);
      else if (event.type === 'text' && this.outputChannel === 'reasoning') this.write('\n[answer]\n');
      if (event.type === 'reasoning') this.reasoningSource = event.source;
      this.outputChannel = event.type; this.write(event.text);
    }
    else if (event.type === 'tool') {
      this.toolStates.set(event.id, `${event.name}: ${event.state}`);
    } else if (event.type === 'usage') this.tokens += event.total;
    else if (event.type === 'compaction') this.write('\n[Context compacted]\n');
    else if (event.type === 'stopped') this.write(`\n${event.reason}\n`);
    while (this.toolStates.size > 6) this.toolStates.delete(this.toolStates.keys().next().value!);
    this.schedule();
  }
  setStatus(status: string) { this.status = status; if (status === 'Running') this.outputChannel = undefined; this.schedule(); }
  question(label = 'goon> ', signal?: AbortSignal, remember = false): Promise<string> {
    if (this.closed) return Promise.reject(new Error('Terminal closed.'));
    if (this.pending) return Promise.reject(new Error('A terminal question is already active.'));
    signal?.throwIfAborted();
    this.label = terminalText(label); this.input = []; this.cursor = 0;
    this.historyIndex = this.history.length; this.remember = remember;
    const result = new Promise<string>((resolve, reject) => {
      const abort = () => this.reject(new Error('Input cancelled.'));
      this.pending = { resolve, reject, cleanup: () => signal?.removeEventListener('abort', abort) };
      signal?.addEventListener('abort', abort, { once: true });
    });
    this.schedule(); return result;
  }
  private reject(error: Error) { const pending = this.pending; this.pending = undefined; pending?.cleanup(); pending?.reject(error); this.schedule(); }
  private keypress = (text: string | undefined, key: Key = {}) => {
    if (key.ctrl && key.name === 'c') { this.cancel(); return; }
    if (key.ctrl && key.name === 'd' && !this.input.length) { this.ended(); return; }
    if (key.name === 'pageup') { this.scroll += Math.max(1, (this.target.rows || 24) - 8); this.schedule(); return; }
    if (key.name === 'pagedown') { this.scroll = Math.max(0, this.scroll - Math.max(1, (this.target.rows || 24) - 8)); this.schedule(); return; }
    if (!this.pending) return;
    if (key.name === 'return' || key.name === 'enter') {
      const value = this.input.join(''), pending = this.pending; this.pending = undefined;
      pending.cleanup();
      if (this.remember && value.trim()) { this.history.push(value); if (this.history.length > 100) this.history.shift(); }
      this.input = []; this.cursor = 0; pending.resolve(value); this.schedule(); return;
    }
    if (key.name === 'left') this.cursor = Math.max(0, this.cursor - 1);
    else if (key.name === 'right') this.cursor = Math.min(this.input.length, this.cursor + 1);
    else if (key.name === 'home' || (key.ctrl && key.name === 'a')) this.cursor = 0;
    else if (key.name === 'end' || (key.ctrl && key.name === 'e')) this.cursor = this.input.length;
    else if (key.name === 'backspace') { if (this.cursor) this.input.splice(--this.cursor, 1); }
    else if (key.name === 'delete') this.input.splice(this.cursor, 1);
    else if ((key.name === 'up' || key.name === 'down') && this.remember) {
      this.historyIndex = Math.min(this.history.length, Math.max(0, this.historyIndex + (key.name === 'up' ? -1 : 1)));
      this.input = [...(this.history[this.historyIndex] ?? '')]; this.cursor = this.input.length;
    } else if (key.ctrl && key.name === 'u') { this.input = []; this.cursor = 0; }
    else if (text && !key.ctrl && !key.meta && this.input.length < 16_000) {
      const chars = [...terminalText(text).replace(/\n/g, ' ')]; this.input.splice(this.cursor, 0, ...chars); this.cursor += chars.length;
    }
    this.schedule();
  };
  private ended = () => { this.cancel(); this.close(); };
  private resize = () => this.schedule();
  private schedule() {
    if (!this.closed && !this.timer) this.timer = setTimeout(() => { this.timer = undefined; this.render(); }, 20);
  }
  private render() {
    if (this.closed) return;
    const columns = Math.max(4, this.target.columns || 80), rows = Math.max(8, this.target.rows || 24);
    const clip = (s: string) => wrapTerminal(s, columns - 1)[0];
    const tools = [...this.toolStates.values()].slice(-2).join(' | ');
    const header = clip(`goon · ${this.title}`);
    const footer = clip(`${this.status} · ${this.tokens} tokens · Ctrl+C cancel · PgUp/PgDn scroll`);
    const all = wrapTerminal(this.transcript, columns - 1), height = rows - 5;
    this.scroll = Math.min(this.scroll, Math.max(0, all.length - height));
    const end = all.length - this.scroll;
    const body = all.slice(Math.max(0, end - height), end);
    while (body.length < height) body.push('');
    const label = clip(this.label);
    let visible = this.input.slice(0, this.cursor).join('');
    while (visible && wrapTerminal(label + visible, columns - 1).length > 1) visible = [...visible].slice(1).join('');
    const prompt = clip(label + visible + this.input.slice(this.cursor).join(''));
    const cursorColumn = Math.min(columns, [...label + visible].reduce((n, c) => n + width(c), 0) + 1);
    this.target.write(`\x1b[?25l\x1b[H\x1b[2J${[header, ...body, clip(tools), footer, prompt].join('\r\n')}` +
      (this.pending ? `\x1b[${rows - 1};${cursorColumn}H\x1b[?25h` : ''));
  }
  close() {
    if (this.closed) return;
    this.closed = true; clearTimeout(this.timer);
    this.reject(new Error('Terminal closed.'));
    this.source.removeListener('keypress', this.keypress); this.source.removeListener('end', this.ended); this.target.removeListener('resize', this.resize);
    this.source.setRawMode(this.oldRaw); this.source.pause();
    this.target.write('\x1b[?25h\x1b[?1049l');
  }
}
