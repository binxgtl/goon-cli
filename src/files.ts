import { realpath, stat, readFile, readdir, mkdir, open, rename, unlink } from 'node:fs/promises';
import { realpathSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, resolve, relative, isAbsolute, join } from 'node:path';
import { createHash, randomUUID } from 'node:crypto';
import type { FileBackup } from './session.js';
import { validateSyntax } from './syntax.js';

const MAX_FILE_BYTES = 2 * 1024 * 1024;
export const WINDOWS_RESERVED_DEVICE_RE = /^(?:con|prn|aux|nul|com[1-9]|lpt[1-9])(?:\..*)?$/i;

export function toWinExtendedPath(absPath: string): string {
  if (process.platform !== 'win32' || absPath.startsWith('\\\\?\\')) return absPath;
  return `\\\\?\\${resolve(absPath)}`;
}

export async function unlinkSafe(absPath: string): Promise<void> {
  if (process.platform === 'win32') {
    const base = absPath.split(/[\\/]/).pop() ?? '';
    if (WINDOWS_RESERVED_DEVICE_RE.test(base)) {
      try {
        await unlink(toWinExtendedPath(absPath));
      } catch (err) {
        if ((err as { code?: string })?.code !== 'ENOENT') throw err;
      }
      return;
    }
    try {
      await unlink(absPath);
    } catch {
      try {
        await unlink(toWinExtendedPath(absPath));
      } catch (err) {
        if ((err as { code?: string })?.code !== 'ENOENT') throw err;
      }
    }
    return;
  }
  try {
    await unlink(absPath);
  } catch (err) {
    if ((err as { code?: string })?.code !== 'ENOENT') throw err;
  }
}

/**
 * Replaces the first literal occurrence of `search`. Unlike `String.prototype.replace`, the
 * replacement is inserted verbatim: `$&`, `$'`, `` $` `` and `$$` are NOT expanded, so edits to
 * shell scripts, regexes, PHP or jQuery code are never corrupted.
 */
export function replaceFirstLiteral(text: string, search: string, replacement: string): string {
  const index = text.indexOf(search);
  if (index < 0) return text;
  return text.slice(0, index) + replacement + text.slice(index + search.length);
}

/** True when `candidate` (an absolute, already-resolved path) lies inside `root`. */
export function isWithinRoot(root: string, candidate: string): boolean {
  const rel = relative(root, candidate);
  return !(rel === '..' || rel.startsWith(`..${process.platform === 'win32' ? '\\' : '/'}`) || isAbsolute(rel));
}

export const limitText = (text: string, limit = 32_000): string => {
  if (text.length <= limit) return text;
  const headLen = Math.floor(limit * 0.65);
  const tailLen = Math.max(0, limit - headLen);
  const omitted = text.length - headLen - tailLen;
  return `${text.slice(0, headLen)}\n...[truncated ${omitted} chars; narrow the request]...\n${text.slice(text.length - tailLen)}`;
};

/** Largest prefix/suffix whose UTF-8 encoding fits in `maxBytes` (never splits a surrogate pair). */
function sliceByBytes(text: string, maxBytes: number, fromEnd: boolean): string {
  if (maxBytes <= 0) return '';
  if (Buffer.byteLength(text, 'utf8') <= maxBytes) return text;
  let low = 0, high = text.length;
  while (low < high) {
    const mid = Math.ceil((low + high) / 2);
    const part = fromEnd ? text.slice(text.length - mid) : text.slice(0, mid);
    if (Buffer.byteLength(part, 'utf8') <= maxBytes) low = mid;
    else high = mid - 1;
  }
  if (!fromEnd) {
    if (low > 0 && low < text.length && /[\uD800-\uDBFF]/.test(text[low - 1]) && /[\uDC00-\uDFFF]/.test(text[low])) low--;
    return text.slice(0, low);
  }
  let start = text.length - low;
  if (start > 0 && start < text.length && /[\uDC00-\uDFFF]/.test(text[start]) && /[\uD800-\uDBFF]/.test(text[start - 1])) start++;
  return text.slice(start);
}

/**
 * Byte-bounded head/tail truncation (v0.5, P12). Used for MCP output where the limit is expressed
 * in bytes; the marker is ASCII and the result never exceeds `maxBytes`.
 */
export function limitTextBytes(text: string, maxBytes = 64 * 1024): string {
  const total = Buffer.byteLength(text, 'utf8');
  if (total <= maxBytes) return text;
  const budget = Math.max(0, maxBytes - 128);
  let head = sliceByBytes(text, Math.floor(budget * 0.65), false);
  let tail = sliceByBytes(text, budget - Math.floor(budget * 0.65), true);
  const omitted = total - Buffer.byteLength(head, 'utf8') - Buffer.byteLength(tail, 'utf8');
  const marker = `\n...[truncated ${omitted} bytes; narrow the request]...\n`;
  // Defensive: if the marker pushed us over the limit (tiny maxBytes), shrink tail, then head.
  // Each pass removes a whole code point, so the loop always terminates.
  const size = () => Buffer.byteLength(head, 'utf8') + Buffer.byteLength(marker, 'utf8') + Buffer.byteLength(tail, 'utf8');
  while (size() > maxBytes && (tail.length > 0 || head.length > 0)) {
    if (tail.length > 0) tail = tail.slice(Array.from(tail)[0]!.length);
    else head = head.slice(0, head.length - Array.from(head).pop()!.length);
  }
  const result = `${head}${marker}${tail}`;
  return Buffer.byteLength(result, 'utf8') <= maxBytes ? result : marker.trim().slice(0, Math.max(0, maxBytes));
}

export function computeUnifiedDiff(path: string, before: string, after: string, contextLines = 3): string {
  const splitLines = (text: string): string[] => {
    if (text === '') return [];
    const normalized = text.replace(/\r\n/g, '\n');
    return normalized.endsWith('\n') ? normalized.slice(0, -1).split('\n') : normalized.split('\n');
  };
  let oldLines = splitLines(before);
  let newLines = splitLines(after);
  if (before !== after && oldLines.length === newLines.length && oldLines.every((l, i) => l === newLines[i])) {
    oldLines = before === '' ? [] : before.replace(/\r\n/g, '\n').split('\n');
    newLines = after === '' ? [] : after.replace(/\r\n/g, '\n').split('\n');
  }
  let prefix = 0;
  while (prefix < oldLines.length && prefix < newLines.length && oldLines[prefix] === newLines[prefix]) {
    prefix++;
  }
  if (prefix === oldLines.length && prefix === newLines.length) {
    return `No changes in ${path}.`;
  }
  let suffix = 0;
  while (
    suffix < oldLines.length - prefix &&
    suffix < newLines.length - prefix &&
    oldLines[oldLines.length - 1 - suffix] === newLines[newLines.length - 1 - suffix]
  ) {
    suffix++;
  }
  const oldChanged = oldLines.slice(prefix, oldLines.length - suffix);
  const newChanged = newLines.slice(prefix, newLines.length - suffix);
  const ctxStart = Math.max(0, prefix - contextLines);
  const ctxEndOld = Math.min(oldLines.length, oldLines.length - suffix + contextLines);
  const beforeCtx = oldLines.slice(ctxStart, prefix);
  const afterCtx = oldLines.slice(oldLines.length - suffix, ctxEndOld);

  const oldStart = oldLines.length === 0 ? 0 : ctxStart + 1;
  const oldCount = beforeCtx.length + oldChanged.length + afterCtx.length;
  const newStart = newLines.length === 0 ? 0 : ctxStart + 1;
  const newCount = beforeCtx.length + newChanged.length + afterCtx.length;
  const maxDiffLines = 5000;
  const kindPrefix = before === '' && after !== '' ? 'new ' : after === '' && before !== '' ? 'deleted ' : '';

  const out: string[] = [
    `--- a/${path} (${kindPrefix}+${newChanged.length} -${oldChanged.length})`,
    `+++ b/${path}`,
    `@@ -${oldStart},${oldCount} +${newStart},${newCount} @@`,
    ...beforeCtx.map(l => ` ${l}`),
    ...oldChanged.slice(0, maxDiffLines).map(l => `-${l}`),
    ...(oldChanged.length > maxDiffLines ? [`... (-${oldChanged.length - maxDiffLines} more removed lines)`] : []),
    ...newChanged.slice(0, maxDiffLines).map(l => `+${l}`),
    ...(newChanged.length > maxDiffLines ? [`... (+${newChanged.length - maxDiffLines} more added lines)`] : []),
    ...afterCtx.map(l => ` ${l}`),
  ];
  return out.join('\n');
}

function matchTrimmedLines(current: string, oldText: string, newText: string): { matched: boolean; count: number; result?: string } {
  const eol = current.includes('\r\n') ? '\r\n' : '\n';
  const fileLines = current.replace(/\r\n/g, '\n').split('\n');
  const needleLines = oldText.replace(/\r\n/g, '\n').split('\n');
  if (!needleLines.length || needleLines.length > fileLines.length) return { matched: false, count: 0 };
  const matches: number[] = [];
  for (let i = 0; i <= fileLines.length - needleLines.length; i++) {
    let ok = true;
    for (let j = 0; j < needleLines.length; j++) {
      if (fileLines[i + j].trim() !== needleLines[j].trim()) {
        ok = false;
        break;
      }
    }
    if (ok) matches.push(i);
  }
  if (matches.length !== 1) return { matched: false, count: matches.length };
  const idx = matches[0];
  const replacementLines = newText.replace(/\r\n/g, '\n').split('\n');
  const updated = [
    ...fileLines.slice(0, idx),
    ...replacementLines,
    ...fileLines.slice(idx + needleLines.length),
  ].join(eol);
  return { matched: true, count: 1, result: updated };
}


export interface PatchHunk {
  oldLines: string[];
  newLines: string[];
}

export type PatchOperation =
  | { kind: 'add'; path: string; content: string }
  | { kind: 'delete'; path: string }
  | { kind: 'update'; path: string; hunks: PatchHunk[]; moveTo?: string };

export function parsePatchText(rawPatch: string): PatchOperation[] {
  const lines = rawPatch.replace(/\r\n/g, '\n').split('\n');
  const ops: PatchOperation[] = [];
  let i = 0;
  while (i < lines.length && lines[i].trim() === '') i++;
  if (i < lines.length && lines[i].trim() === '*** Begin Patch') i++;

  while (i < lines.length) {
    const line = lines[i];
    if (line.trim() === '*** End Patch' || line.trim() === '') {
      if (line.trim() === '*** End Patch') break;
      i++;
      continue;
    }
    if (line.startsWith('*** Add File: ')) {
      const path = line.slice('*** Add File: '.length).trim();
      i++;
      const added: string[] = [];
      while (i < lines.length && !lines[i].startsWith('*** ')) {
        added.push(lines[i]);
        i++;
      }
      // Bare blank lines between the last "+" line and the next header are patch formatting,
      // not file content. Every "+" line is a complete line (Codex apply_patch semantics), so a
      // non-empty added file always ends with a newline.
      while (added.length && added[added.length - 1] === '') added.pop();
      const content = added.map(l => `${l.startsWith('+') ? l.slice(1) : l}\n`).join('');
      ops.push({ kind: 'add', path, content });
      continue;
    }
    if (line.startsWith('*** Delete File: ')) {
      const path = line.slice('*** Delete File: '.length).trim();
      i++;
      ops.push({ kind: 'delete', path });
      continue;
    }
    if (line.startsWith('*** Update File: ')) {
      const path = line.slice('*** Update File: '.length).trim();
      i++;
      let moveTo: string | undefined;
      if (i < lines.length && lines[i].startsWith('*** Move to: ')) {
        moveTo = lines[i].slice('*** Move to: '.length).trim() || undefined;
        i++;
      }
      const hunks: PatchHunk[] = [];
      let oldLines: string[] = [];
      let newLines: string[] = [];
      let inHunk = false;
      const flushHunk = () => {
        if (inHunk && (oldLines.length > 0 || newLines.length > 0)) {
          hunks.push({ oldLines, newLines });
        }
        oldLines = [];
        newLines = [];
      };
      while (i < lines.length && !lines[i].startsWith('*** ')) {
        const hl = lines[i];
        if (hl.startsWith('@@')) {
          flushHunk();
          inHunk = true;
          i++;
          continue;
        }
        if (hl === '\\ No newline at end of file') {
          i++;
          continue;
        }
        inHunk = true;
        if (hl.startsWith('-')) {
          oldLines.push(hl.slice(1));
        } else if (hl.startsWith('+')) {
          newLines.push(hl.slice(1));
        } else if (hl.startsWith(' ') || hl === '') {
          const ctx = hl.startsWith(' ') ? hl.slice(1) : '';
          oldLines.push(ctx);
          newLines.push(ctx);
        } else {
          oldLines.push(hl);
          newLines.push(hl);
        }
        i++;
      }
      flushHunk();
      ops.push({ kind: 'update', path, hunks, ...(moveTo && moveTo !== path ? { moveTo } : {}) });
      continue;
    }
    throw new Error(`Invalid patch header line: ${line}`);
  }
  if (!ops.length) throw new Error('Patch contained no file operations. Use "*** Begin Patch" with "*** Update File: <path>", "*** Add File: <path>", or "*** Delete File: <path>".');
  return ops;
}

/**
 * Finds the single occurrence of `needle` at or after `startIdx`, trying exact, right-trimmed
 * and fully trimmed comparisons in that order. More than one match in the chosen pass is
 * ambiguous: silently picking the first one can corrupt a file, so the caller must add context.
 */
export function findHunkIndex(haystack: string[], needle: string[], startIdx: number, path: string): number {
  if (needle.length === 0) return startIdx; // insertion-only hunk: apply at the cursor
  const passes: Array<(a: string, b: string) => boolean> = [
    (a, b) => a === b,
    (a, b) => a.trimEnd() === b.trimEnd(),
    (a, b) => a.trim() === b.trim(),
  ];
  for (const eq of passes) {
    const hits: number[] = [];
    for (let i = startIdx; i <= haystack.length - needle.length; i++) {
      let ok = true;
      for (let j = 0; j < needle.length; j++) {
        if (!eq(haystack[i + j], needle[j])) {
          ok = false;
          break;
        }
      }
      if (ok) hits.push(i);
    }
    if (hits.length > 1) {
      throw new Error(`Patch hunk is ambiguous in ${path} (matched ${hits.length} times); add more surrounding context.`);
    }
    if (hits.length === 1) return hits[0];
  }
  return -1;
}

export function applyHunksToContent(current: string, hunks: PatchHunk[], path: string): string {
  const eol = current.includes('\r\n') ? '\r\n' : '\n';
  const lines = current.replace(/\r\n/g, '\n').split('\n');
  let cursor = 0;
  for (const hunk of hunks) {
    const idx = findHunkIndex(lines, hunk.oldLines, cursor, path);
    if (idx === -1) {
      throw new Error(`Failed to match patch hunk in ${path} near: "${hunk.oldLines[0] ?? ''}"`);
    }
    lines.splice(idx, hunk.oldLines.length, ...hunk.newLines);
    cursor = idx + hunk.newLines.length;
  }
  return lines.join(eol);
}

export class WorkspaceFiles {
  private revisions = new Map<string, string>();
  private activeBackups = new Map<string, FileBackup>();
  /** Canonical (symlink-free) workspace root, so containment checks agree with realpath results. */
  readonly root: string;
  constructor(root: string) {
    const absolute = resolve(root);
    let canonical = absolute;
    try { canonical = realpathSync(absolute); } catch { /* not created yet: keep the lexical path */ }
    this.root = canonical;
  }
  beginTurn() {
    this.activeBackups.clear();
  }
  /** Backups collected so far without consuming them (rollback keeps them for the checkpoint). */
  peekTurnBackups(): FileBackup[] {
    return [...this.activeBackups.values()];
  }
  /** Workspace-relative posix paths the agent touched (write/edit/apply_patch) this turn. */
  turnChangedFiles(): string[] {
    return [...this.activeBackups.keys()];
  }
  hasTurnChanges(): boolean {
    return this.activeBackups.size > 0;
  }
  clearTurnBackups(): void {
    this.activeBackups.clear();
  }
  replaceTurnBackups(backups: FileBackup[]): void {
    this.activeBackups.clear();
    for (const b of backups) {
      const key = b.path.replace(/\\/g, '/');
      this.activeBackups.set(key, { ...b, path: key });
    }
  }
  takeTurnBackups(): FileBackup[] {
    const list = this.peekTurnBackups();
    this.activeBackups.clear();
    return list;
  }
  mergeBackups(backups: FileBackup[]) {
    for (const b of backups) {
      const key = b.path.replace(/\\/g, '/');
      const existing = this.activeBackups.get(key);
      if (!existing) {
        this.activeBackups.set(key, { ...b, path: key });
        continue;
      }
      // `existing` is the earliest pre-turn state and stays authoritative. Only the follow-on
      // state (what the file looks like now) is merged, so a child deletion is not lost.
      existing.afterHash = b.afterHash;
      if (b.afterDeleted) {
        existing.afterDeleted = true;
        delete existing.afterHash;
      } else {
        delete existing.afterDeleted;
      }
      if (existing.content === undefined && b.content !== undefined) existing.content = b.content;
      if (existing.mode === undefined && b.mode !== undefined) existing.mode = b.mode;
    }
  }
  private recordBackup(relPath: string, existed: boolean, content?: string, mode?: number, afterContent?: string, afterDeleted = false) {
    const key = relPath.replace(/\\/g, '/');
    let entry = this.activeBackups.get(key);
    if (!entry) {
      entry = {
        path: key,
        existed,
        ...(content !== undefined ? { content } : {}),
        ...(mode !== undefined ? { mode } : {}),
      };
      this.activeBackups.set(key, entry);
    }
    if (afterDeleted) {
      entry.afterDeleted = true;
      delete entry.afterHash;
    } else {
      entry.afterHash = afterContent !== undefined ? this.digest(afterContent) : undefined;
      delete entry.afterDeleted;
    }
  }
  async path(requested: string): Promise<string> {
    const lexical = resolve(this.root, requested);
    this.assertWithin(lexical);
    let parent = lexical;
    const suffix: string[] = [];
    for (;;) {
      try {
        const resolved = await realpath(parent);
        this.assertWithin(resolved);
        return resolve(resolved, ...suffix.reverse());
      } catch (error) {
        if ((error as NodeJS.ErrnoException).code !== 'ENOENT') throw error;
        if (parent === dirname(parent)) throw error;
        suffix.push(parent.slice(dirname(parent).length + 1));
        parent = dirname(parent);
      }
    }
  }
  private assertWithin(file: string) {
    if (!isWithinRoot(this.root, file)) {
      throw new Error('File path escapes the workspace.');
    }
  }
  private assertNotWindowsReserved(file: string) {
    const rel = relative(this.root, file);
    for (const seg of rel.split(/[\\/]/)) {
      if (WINDOWS_RESERVED_DEVICE_RE.test(seg)) {
        throw new Error(`Refusing to create Windows reserved device name "${seg}" (use /dev/null instead of nul).`);
      }
    }
  }
  private digest(text: string) { return createHash('sha256').update(text).digest('hex'); }
  async text(file: string) {
    const target = process.platform === 'win32' && WINDOWS_RESERVED_DEVICE_RE.test(file.split(/[\\/]/).pop() ?? '')
      ? toWinExtendedPath(file)
      : file;
    const info = await stat(target);
    if (!info.isFile() || info.size > MAX_FILE_BYTES) throw new Error('Expected a regular text file no larger than 2 MiB.');
    const text = await readFile(target, 'utf8');
    if (text.includes('\0')) throw new Error('Binary files are not supported.');
    return text;
  }
  async read(path: string, offset = 1, lines = 200) {
    const file = await this.path(path);
    const text = await this.text(file);
    this.revisions.set(file, this.digest(text));
    const all = text.split('\n');
    return limitText(`${path} (${all.length} lines)\n${all.slice(offset - 1, offset - 1 + lines).map((line, i) => `${offset + i}: ${line}`).join('\n')}`);
  }
  private async writeRaw(file: string, content: string, mode: number, existing: boolean) {
    this.assertNotWindowsReserved(file);
    await mkdir(dirname(file), { recursive: true });
    // Re-check containment after creating parents: a directory swapped for a symlink between the
    // path check and this write must never redirect the write outside the workspace.
    this.assertWithin(await realpath(dirname(file)));
    const temporary = join(dirname(file), `.goon-${randomUUID()}.tmp`);
    const handle = await open(temporary, 'wx', mode);
    try {
      await handle.writeFile(content, 'utf8');
      if (existing) await handle.chmod(mode);
      await handle.sync();
      await handle.close();
      await rename(temporary, file);
    } catch (error) {
      await handle.close().catch(() => {});
      await unlink(temporary).catch(() => {});
      throw error;
    }
    this.revisions.set(file, this.digest(content));
  }
  async write(path: string, content: string) {
    const file = await this.path(path);
    this.assertNotWindowsReserved(file);
    if (Buffer.byteLength(content) > MAX_FILE_BYTES) throw new Error('Write exceeds 2 MiB.');
    let mode = 0o644;
    let existing = false;
    let current = '';
    try {
      current = await this.text(file);
      if (this.revisions.get(file) !== this.digest(current)) throw new Error('Read the current file before overwriting; it is unread or has changed.');
      mode = (await stat(file)).mode & 0o777;
      existing = true;
    } catch (error) { if ((error as NodeJS.ErrnoException).code !== 'ENOENT') throw error; }
    await mkdir(dirname(file), { recursive: true });
    // Re-resolve after creating parents to reject a pre-existing symlink escape.
    if (await this.path(path) !== file) throw new Error('Path changed while preparing the write.');
    this.recordBackup(relative(this.root, file), existing, existing ? current : undefined, existing ? mode : undefined, content);
    await this.writeRaw(file, content, mode, existing);
    return `Wrote ${path} (${Buffer.byteLength(content)} bytes).`;
  }
  async edit(path: string, oldText: string, newText: string, replaceAll = false) {
    const file = await this.path(path);
    const current = await this.text(file);
    if (this.revisions.get(file) !== this.digest(current)) throw new Error('Read the current file before editing; it is unread or has changed.');
    let targetOld = oldText;
    let targetNew = newText;
    let count = current.split(targetOld).length - 1;
    // Stage 2 (DeepSeek Harness): CRLF/LF normalization when file uses \r\n and model emitted \n
    if (count === 0 && current.includes('\r\n') && !oldText.includes('\r\n')) {
      const crlfOld = oldText.replace(/\n/g, '\r\n');
      const crlfCount = current.split(crlfOld).length - 1;
      if (crlfCount > 0) {
        targetOld = crlfOld;
        targetNew = newText.replace(/\r?\n/g, '\r\n');
        count = crlfCount;
      }
    }
    // Stage 3 (ZCode): Line-trimmed fallback for unique multi-line blocks when whitespace drifts
    if (count === 0 && !replaceAll && oldText.includes('\n')) {
      const trimmed = matchTrimmedLines(current, oldText, newText);
      if (trimmed.matched && trimmed.result !== undefined) {
        return this.write(path, trimmed.result);
      }
      if (trimmed.count > 1) count = trimmed.count;
    }
    if (!count || (!replaceAll && count !== 1)) throw new Error(`Expected one exact match; found ${count}. Use replaceAll explicitly for multiple matches.`);
    return this.write(path, replaceAll ? current.split(targetOld).join(targetNew) : replaceFirstLiteral(current, targetOld, targetNew));
  }
  /**
   * Plans every operation against a virtual view of the files it touches (so a later operation sees
   * the result of an earlier one, e.g. two Update blocks for one file, or Update + Move to), validates
   * the whole patch, and only then writes. A patch that fails validation changes nothing.
   */
  async applyPatch(patchText: string): Promise<string> {
    const ops = parsePatchText(patchText);
    interface Slot {
      file: string;
      relPath: string;
      existed: boolean;
      before?: string;
      mode: number;
      /** undefined = untouched, null = deleted by this patch, string = new content. */
      after?: string | null;
    }
    const slots = new Map<string, Slot>();
    const order: Slot[] = [];
	    const slotFor = async (requested: string, forWrite: boolean, checkRevision = true): Promise<Slot> => {
	      const file = await this.path(requested);
	      if (forWrite) this.assertNotWindowsReserved(file);
	      let slot = slots.get(file);
	      if (!slot) {
	        slot = { file, relPath: relative(this.root, file), existed: false, mode: 0o644 };
	        try {
	          slot.before = await this.text(file);
	          slot.mode = (await stat(this.statTarget(file))).mode & 0o777;
	          slot.existed = true;
	          if (checkRevision && this.revisions.get(file) !== this.digest(slot.before!)) {
	            throw new Error('Read the current file before patching; it is unread or has changed.');
	          }
	        } catch (error) {
	          if ((error as NodeJS.ErrnoException).code !== 'ENOENT') throw error;
	        }
	        slots.set(file, slot);
	        order.push(slot);
	      }
	      return slot;
	    };
    const currentOf = (slot: Slot): string | undefined =>
      slot.after === undefined ? slot.before : slot.after ?? undefined;

    const summary: string[] = [];
    for (const op of ops) {
      if (op.kind === 'add') {
        const slot = await slotFor(op.path, true);
        summary.push(`${currentOf(slot) === undefined ? 'Added' : 'Updated'} ${op.path}`);
        slot.after = op.content;
      } else if (op.kind === 'delete') {
        const slot = await slotFor(op.path, false);
        if (currentOf(slot) === undefined) throw new Error(`Cannot delete ${op.path}: file does not exist.`);
        slot.after = null;
        summary.push(`Deleted ${op.path}`);
      } else {
        const slot = await slotFor(op.path, true);
        const current = currentOf(slot);
        if (current === undefined) throw new Error(`Cannot update ${op.path}: file does not exist.`);
        const next = applyHunksToContent(current, op.hunks, op.path);
	        if (op.moveTo) {
	          const destination = await slotFor(op.moveTo, true, false);
	          if (destination !== slot) {
            if (currentOf(destination) !== undefined) throw new Error(`Cannot move ${op.path} to ${op.moveTo}: destination exists.`);
            destination.mode = slot.mode;
            destination.after = next;
            slot.after = null;
            summary.push(`Moved ${op.path} -> ${op.moveTo}`);
            continue;
          }
        }
        slot.after = next;
        summary.push(`Updated ${op.path}`);
      }
    }

	    // Validate syntax/AST of every resulting file before touching disk, so a broken patch
	    // changes nothing (the whole patch is atomic from the caller's point of view).
	    for (const slot of order) {
	      if (typeof slot.after === 'string') {
	        try {
	          validateSyntax(slot.relPath, slot.after);
	        } catch (error) {
	          throw new Error(`Syntax validation failed for ${slot.relPath}: ${(error as Error).message}`);
	        }
	      }
	    }
	    for (const slot of order) {
	      if (slot.after === undefined) continue;
	      if (slot.after === null && !slot.existed) continue; // created and removed within this patch
	      this.recordBackup(slot.relPath, slot.existed, slot.before, slot.existed ? slot.mode : undefined, slot.after ?? undefined, slot.after === null);
	      if (slot.after === null) {
	        await unlinkSafe(slot.file);
	        this.revisions.delete(slot.file);
	      } else {
	        await this.writeRaw(slot.file, slot.after, slot.mode, slot.existed);
	      }
	    }
    return summary.join('\n');
  }
  private statTarget(file: string): string {
    return process.platform === 'win32' && WINDOWS_RESERVED_DEVICE_RE.test(file.split(/[\\/]/).pop() ?? '')
      ? toWinExtendedPath(file)
      : file;
  }
  /**
   * Restores turn backups. A file whose current content no longer matches what the agent left
   * behind (`afterHash`) was edited afterwards, by the user or another tool; it is reported as a
   * conflict and left untouched unless `force` is set, so a rewind never destroys newer work.
   */
  async restoreBackups(
    backups: FileBackup[],
    options: { force?: boolean } = {},
  ): Promise<{ restored: string[]; deleted: string[]; conflicts: string[]; pruned: string[] }> {
    const restored: string[] = [];
    const deleted: string[] = [];
    const conflicts: string[] = [];
    const pruned: string[] = [];
	    for (const b of backups) {
	      const file = await this.path(b.path);
	      if (b.afterDeleted) {
	        let exists = true;
	        try { await this.text(file); }
	        catch (error) {
	          if ((error as NodeJS.ErrnoException).code === 'ENOENT') exists = false;
	        }
	        if (!options.force && exists) {
	          conflicts.push(b.path);
	          continue;
	        }
	        if (!b.existed) {
	          await unlinkSafe(file).catch(() => {});
	          this.revisions.delete(file);
	          deleted.push(b.path);
	        } else if (b.content !== undefined) {
	          await this.writeRaw(file, b.content, b.mode ?? 0o644, true);
	          restored.push(b.path);
	        } else if (b.pruned) {
	          pruned.push(b.path);
	        }
	        continue;
	      }
	      if (!options.force) {
	        let current2: string | undefined;
	        let readable2 = true;
	        try { current2 = await this.text(file); }
	        catch (error) {
	          if ((error as NodeJS.ErrnoException).code !== 'ENOENT') readable2 = false;
	        }
	        // Without an afterHash (deletions, legacy backups) there is nothing to compare against.
	        const conflict = b.afterHash !== undefined
	          && (!readable2 || (current2 !== undefined && this.digest(current2) !== b.afterHash));
	        if (conflict) {
	          conflicts.push(b.path);
	          continue;
	        }
	      }
	      if (!b.existed) {
	        await unlinkSafe(file).catch(() => {});
	        this.revisions.delete(file);
	        deleted.push(b.path);
	      } else if (b.content !== undefined) {
	        await this.writeRaw(file, b.content, b.mode ?? 0o644, true);
	        restored.push(b.path);
	      } else if (b.pruned) {
	        pruned.push(b.path);
	      }
	    }
    return { restored, deleted, conflicts, pruned };
  }
  async listWorkspaceFiles(maxFiles = 500): Promise<string[]> {
    const files: string[] = [];
    const queue = [this.root];
    while (queue.length && files.length < maxFiles) {
      const dir = queue.shift()!;
      let entries;
      try { entries = await readdir(dir, { withFileTypes: true }); } catch { continue; }
      for (const entry of entries) {
        if (files.length >= maxFiles) break;
        if (entry.name === '.git' || entry.name === 'node_modules' || entry.name === 'dist' || entry.name === '.refs' || entry.isSymbolicLink()) continue;
        const full = join(dir, entry.name);
        if (entry.isDirectory()) { queue.push(full); continue; }
        if (entry.isFile()) files.push(relative(this.root, full).replace(/\\/g, '/'));
      }
    }
    return files.sort();
  }
  /** Resolves an image path the way `readImage` does and reports whether it lies inside the workspace. */
  async locateImage(userPath: string): Promise<{ resolvedPath: string; inside: boolean }> {
    const { file } = await this.resolveImagePath(userPath, true);
    return { resolvedPath: file, inside: isWithinRoot(this.root, file) };
  }
  private async resolveImagePath(userPath: string, allowOutside: boolean): Promise<{ file: string; mediaType: string }> {
    let cleaned = userPath.trim();
    if ((cleaned.startsWith('"') && cleaned.endsWith('"')) || (cleaned.startsWith("'") && cleaned.endsWith("'"))) {
      cleaned = cleaned.slice(1, -1).trim();
    }
    if (cleaned.startsWith('file://')) {
      try { cleaned = fileURLToPath(cleaned); } catch { /* keep the raw value */ }
    }
    const ext = cleaned.toLowerCase().match(/\.(png|jpe?g|webp|gif)$/)?.[1];
    if (!ext) throw new Error('Unsupported image extension.');
    const mediaType =
      ext === 'png' ? 'image/png'
      : ext === 'webp' ? 'image/webp'
      : ext === 'gif' ? 'image/gif'
      : 'image/jpeg';
    const isAbsWinOrPosix = isAbsolute(cleaned) || /^[A-Za-z]:[\\/]/.test(cleaned) || cleaned.startsWith('\\\\');
    let file: string;
    if (isAbsWinOrPosix) {
      file = await realpath(resolve(cleaned));
      if (!allowOutside) this.assertWithin(file);
    } else if (!allowOutside) {
      file = await this.path(cleaned);
    } else {
      try { file = await this.path(cleaned); }
      catch { file = await realpath(resolve(this.root, cleaned)); }
    }
    // The extension check above applies to the requested name; a symlink must also point at an image.
    if (!/\.(png|jpe?g|webp|gif)$/i.test(file)) throw new Error('Unsupported image extension.');
    return { file, mediaType };
  }
  /**
   * Reads an image. User-typed attachments may come from anywhere on disk; model-initiated reads
   * (`allowOutside: false`) are confined to the workspace like every other file tool.
   */
  async readImage(
    userPath: string,
    maxBytes = 5 * 1024 * 1024,
    options: { allowOutside?: boolean } = {},
  ): Promise<{ buffer: Buffer; mediaType: string; resolvedPath: string }> {
    const { file, mediaType } = await this.resolveImagePath(userPath, options.allowOutside ?? true);
    const info = await stat(file);
    if (!info.isFile() || info.size > maxBytes) {
      throw new Error('Expected a regular image file no larger than 5 MiB.');
    }
    const buffer = await readFile(file);
    return { buffer, mediaType, resolvedPath: file };
  }
  async previewDiff(tool: string, input: unknown): Promise<string | undefined> {
    try {
      if (!input || typeof input !== 'object') return undefined;
      const raw = input as Record<string, unknown>;
      const normalizedTool = tool === 'write' ? 'write_file' : tool === 'edit' ? 'edit_file' : tool;
      const pathVal = raw.path ?? raw.filePath ?? raw.file_path ?? raw.directory;
      const oldVal = raw.oldText ?? raw.old_string ?? raw.old_str;
      const newVal = raw.newText ?? raw.new_string ?? raw.new_str;
      if (normalizedTool === 'apply_patch' && typeof raw.patch === 'string') {
        const ops = parsePatchText(raw.patch);
        const diffs: string[] = [];
        // Mirror applyPatch: later operations see the content produced by earlier ones.
        const virtual = new Map<string, string>();
        const contentOf = async (path: string) => {
          const file = await this.path(path);
          if (virtual.has(file)) return { file, text: virtual.get(file)! };
          let text = '';
          try { text = await this.text(file); } catch { text = ''; }
          return { file, text };
        };
        for (const op of ops) {
          const { file, text: before } = await contentOf(op.path);
          if (op.kind === 'add') {
            diffs.push(computeUnifiedDiff(op.path, before, op.content));
            virtual.set(file, op.content);
          } else if (op.kind === 'delete') {
            diffs.push(computeUnifiedDiff(op.path, before, ''));
            virtual.set(file, '');
          } else {
            const after = applyHunksToContent(before, op.hunks, op.path);
            if (op.moveTo) {
              const destination = await this.path(op.moveTo);
              diffs.push(computeUnifiedDiff(`${op.path} -> ${op.moveTo}`, before, after));
              virtual.set(file, '');
              virtual.set(destination, after);
            } else {
              diffs.push(computeUnifiedDiff(op.path, before, after));
              virtual.set(file, after);
            }
          }
        }
        return diffs.join('\n');
      }
      if (typeof pathVal !== 'string') return undefined;
      const file = await this.path(pathVal);
      let before = '';
      try { before = await this.text(file); } catch { before = ''; }
      if (normalizedTool === 'write_file' && typeof raw.content === 'string') {
        return computeUnifiedDiff(pathVal, before, raw.content);
      }
      if (normalizedTool === 'edit_file' && typeof oldVal === 'string' && typeof newVal === 'string') {
        const replaceAll = raw.replaceAll === true;
        let targetOld = oldVal;
        let targetNew = newVal;
        if (!before.includes(targetOld) && before.includes('\r\n') && !targetOld.includes('\r\n')) {
          targetOld = targetOld.replace(/\n/g, '\r\n');
          targetNew = targetNew.replace(/\r?\n/g, '\r\n');
        }
        if (before.includes(targetOld)) {
          const after = replaceAll ? before.split(targetOld).join(targetNew) : replaceFirstLiteral(before, targetOld, targetNew);
          return computeUnifiedDiff(pathVal, before, after);
        }
        const trimmed = matchTrimmedLines(before, oldVal, newVal);
        if (trimmed.matched && trimmed.result !== undefined) {
          return computeUnifiedDiff(pathVal, before, trimmed.result);
        }
      }
      return undefined;
    } catch {
      return undefined;
    }
  }
  async list(path = '.') {
    const file = await this.path(path);
    const entries = await readdir(file, { withFileTypes: true });
    return limitText(entries.slice(0, 300).map(e => `${e.name}${e.isDirectory() ? '/' : e.isSymbolicLink() ? '@' : ''}`).join('\n') +
      (entries.length > 300 ? '\n...[listing limited to 300 entries]' : ''));
  }
  async fallbackSearch(query: string, path = '.', signal?: AbortSignal) {
    const start = await this.path(path);
    const queue = [start], hits: string[] = [];
    let visited = 0;
    while (queue.length && visited < 3000 && hits.length < 100) {
      signal?.throwIfAborted();
      const dir = queue.shift()!;
      for (const entry of await readdir(dir, { withFileTypes: true })) {
        signal?.throwIfAborted();
        if (++visited > 3000 || hits.length >= 100) break;
        if (entry.name === '.git' || entry.name === 'node_modules' || entry.isSymbolicLink()) continue;
        const file = join(dir, entry.name);
        if (entry.isDirectory()) { queue.push(file); continue; }
        if (!entry.isFile()) continue;
        let text: string;
        try { text = await this.text(file); } catch { continue; }
        text.split('\n').forEach((line, i) => {
          if (hits.length < 100 && line.includes(query)) hits.push(`${relative(this.root, file)}:${i + 1}:${line.slice(0, 1000)}`);
        });
      }
    }
    return limitText(hits.join('\n') || 'No matches found.') + (queue.length || visited >= 3000 || hits.length >= 100 ? '\n[Search reached its limit; narrow the path.]' : '');
  }
}

