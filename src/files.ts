import { realpath, stat, readFile, readdir, mkdir, open, rename, unlink } from 'node:fs/promises';
import { dirname, resolve, relative, isAbsolute, join } from 'node:path';
import { createHash, randomUUID } from 'node:crypto';
import type { FileBackup } from './session.js';

const MAX_FILE_BYTES = 2 * 1024 * 1024;
export const limitText = (text: string, limit = 32_000): string => {
  if (text.length <= limit) return text;
  const headLen = Math.floor(limit * 0.65);
  const tailLen = Math.max(0, limit - headLen);
  const omitted = text.length - headLen - tailLen;
  return `${text.slice(0, headLen)}\n...[truncated ${omitted} chars; narrow the request]...\n${text.slice(text.length - tailLen)}`;
};

export function computeUnifiedDiff(path: string, before: string, after: string, contextLines = 3): string {
  const oldLines = before === '' ? [] : before.replace(/\r\n/g, '\n').split('\n');
  const newLines = after === '' ? [] : after.replace(/\r\n/g, '\n').split('\n');
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

  const oldStart = ctxStart + 1;
  const oldCount = beforeCtx.length + oldChanged.length + afterCtx.length;
  const newStart = ctxStart + 1;
  const newCount = beforeCtx.length + newChanged.length + afterCtx.length;

  const out: string[] = [
    `--- a/${path} (+${newChanged.length} -${oldChanged.length})`,
    `+++ b/${path}`,
    `@@ -${oldStart},${oldCount} +${newStart},${newCount} @@`,
    ...beforeCtx.map(l => ` ${l}`),
    ...oldChanged.slice(0, 60).map(l => `-${l}`),
    ...(oldChanged.length > 60 ? [`... (-${oldChanged.length - 60} more removed lines)`] : []),
    ...newChanged.slice(0, 60).map(l => `+${l}`),
    ...(newChanged.length > 60 ? [`... (+${newChanged.length - 60} more added lines)`] : []),
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
  | { kind: 'update'; path: string; hunks: PatchHunk[] };

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
        added.push(lines[i].startsWith('+') ? lines[i].slice(1) : lines[i]);
        i++;
      }
      ops.push({ kind: 'add', path, content: added.join('\n') });
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
      if (i < lines.length && lines[i].startsWith('*** Move to: ')) i++;
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
      ops.push({ kind: 'update', path, hunks });
      continue;
    }
    throw new Error(`Invalid patch header line: ${line}`);
  }
  if (!ops.length) throw new Error('Patch contained no file operations. Use "*** Begin Patch" with "*** Update File: <path>", "*** Add File: <path>", or "*** Delete File: <path>".');
  return ops;
}

function seekSubsequence(haystack: string[], needle: string[], startIdx: number): number {
  if (needle.length === 0) return startIdx;
  const passes: Array<(a: string, b: string) => boolean> = [
    (a, b) => a === b,
    (a, b) => a.trimEnd() === b.trimEnd(),
    (a, b) => a.trim() === b.trim(),
  ];
  for (const eq of passes) {
    for (let i = startIdx; i <= haystack.length - needle.length; i++) {
      let ok = true;
      for (let j = 0; j < needle.length; j++) {
        if (!eq(haystack[i + j], needle[j])) {
          ok = false;
          break;
        }
      }
      if (ok) return i;
    }
  }
  return -1;
}

export function applyHunksToContent(current: string, hunks: PatchHunk[], path: string): string {
  const eol = current.includes('\r\n') ? '\r\n' : '\n';
  const lines = current.replace(/\r\n/g, '\n').split('\n');
  let cursor = 0;
  for (const hunk of hunks) {
    const idx = seekSubsequence(lines, hunk.oldLines, cursor);
    if (idx === -1) {
      throw new Error(`Failed to match patch hunk in ${path} near: "${hunk.oldLines[0] ?? ''}"`);
    }
    lines.splice(idx, hunk.oldLines.length, ...hunk.newLines);
    cursor = idx + hunk.newLines.length;
  }
  return lines.join(eol);
}

export interface WorkspaceSnapEntry {
  content: string;
  mode: number;
  digest: string;
}

export class WorkspaceFiles {
  private revisions = new Map<string, string>();
  private activeBackups = new Map<string, FileBackup>();
  constructor(readonly root: string) {}
  beginTurn() {
    this.activeBackups.clear();
  }
  takeTurnBackups(): FileBackup[] {
    const list = [...this.activeBackups.values()];
    this.activeBackups.clear();
    return list;
  }
  private recordBackup(relPath: string, existed: boolean, content?: string, mode?: number, afterContent?: string) {
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
    entry.afterHash = afterContent !== undefined ? this.digest(afterContent) : undefined;
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
    const rel = relative(this.root, file);
    if (rel === '..' || rel.startsWith(`..${process.platform === 'win32' ? '\\' : '/'}`) || isAbsolute(rel)) {
      throw new Error('File path escapes the workspace.');
    }
  }
  private digest(text: string) { return createHash('sha256').update(text).digest('hex'); }
  async text(file: string) {
    const info = await stat(file);
    if (!info.isFile() || info.size > MAX_FILE_BYTES) throw new Error('Expected a regular text file no larger than 2 MiB.');
    const text = await readFile(file, 'utf8');
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
    await mkdir(dirname(file), { recursive: true });
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
    return this.write(path, replaceAll ? current.split(targetOld).join(targetNew) : current.replace(targetOld, targetNew));
  }
  async applyPatch(patchText: string): Promise<string> {
    const ops = parsePatchText(patchText);
    const planned: Array<{
      op: PatchOperation;
      file: string;
      relPath: string;
      existing: boolean;
      beforeContent?: string;
      mode: number;
      afterContent?: string;
    }> = [];

    for (const op of ops) {
      const file = await this.path(op.path);
      const relPath = relative(this.root, file);
      if (op.kind === 'add') {
        let existing = false;
        let beforeContent: string | undefined;
        let mode = 0o644;
        try {
          beforeContent = await this.text(file);
          mode = (await stat(file)).mode & 0o777;
          existing = true;
        } catch (error) {
          if ((error as NodeJS.ErrnoException).code !== 'ENOENT') throw error;
        }
        planned.push({ op, file, relPath, existing, beforeContent, mode, afterContent: op.content });
      } else if (op.kind === 'delete') {
        const beforeContent = await this.text(file);
        const mode = (await stat(file)).mode & 0o777;
        planned.push({ op, file, relPath, existing: true, beforeContent, mode });
      } else {
        const beforeContent = await this.text(file);
        const mode = (await stat(file)).mode & 0o777;
        const afterContent = applyHunksToContent(beforeContent, op.hunks, op.path);
        planned.push({ op, file, relPath, existing: true, beforeContent, mode, afterContent });
      }
    }

    const summary: string[] = [];
    for (const item of planned) {
      this.recordBackup(item.relPath, item.existing, item.beforeContent, item.existing ? item.mode : undefined, item.afterContent);
      if (item.op.kind === 'delete') {
        await unlink(item.file);
        this.revisions.delete(item.file);
        summary.push(`Deleted ${item.op.path}`);
      } else {
        await this.writeRaw(item.file, item.afterContent!, item.mode, item.existing);
        summary.push(`${item.op.kind === 'add' && !item.existing ? 'Added' : 'Updated'} ${item.op.path}`);
      }
    }
    return summary.join('\n');
  }
  async restoreBackups(backups: FileBackup[]): Promise<{ restored: string[]; deleted: string[] }> {
    const restored: string[] = [];
    const deleted: string[] = [];
    for (const b of backups) {
      const file = await this.path(b.path);
      if (!b.existed) {
        await unlink(file).catch(() => {});
        this.revisions.delete(file);
        deleted.push(b.path);
      } else if (b.content !== undefined) {
        await this.writeRaw(file, b.content, b.mode ?? 0o644, true);
        restored.push(b.path);
      }
    }
    return { restored, deleted };
  }
  async snapshotWorkspace(maxFiles = 400): Promise<Map<string, WorkspaceSnapEntry>> {
    const snap = new Map<string, WorkspaceSnapEntry>();
    const queue = [this.root];
    while (queue.length && snap.size < maxFiles) {
      const dir = queue.shift()!;
      let entries;
      try { entries = await readdir(dir, { withFileTypes: true }); } catch { continue; }
      for (const entry of entries) {
        if (snap.size >= maxFiles) break;
        if (entry.name === '.git' || entry.name === 'node_modules' || entry.name === 'dist' || entry.name === '.goon' || entry.name === '.refs' || entry.isSymbolicLink()) continue;
        const full = join(dir, entry.name);
        if (entry.isDirectory()) { queue.push(full); continue; }
        if (!entry.isFile()) continue;
        try {
          const info = await stat(full);
          if (info.size > 256 * 1024) continue;
          const content = await readFile(full, 'utf8');
          if (content.includes('\0')) continue;
          const rel = relative(this.root, full).replace(/\\/g, '/');
          snap.set(rel, { content, mode: info.mode & 0o777, digest: this.digest(content) });
        } catch { continue; }
      }
    }
    return snap;
  }
  async captureWorkspaceDiff(before: Map<string, WorkspaceSnapEntry>): Promise<void> {
    const after = await this.snapshotWorkspace();
    for (const [rel, prev] of before) {
      const curr = after.get(rel);
      if (!curr) {
        this.recordBackup(rel, true, prev.content, prev.mode, undefined);
        const abs = resolve(this.root, rel);
        this.revisions.delete(abs);
      } else if (curr.digest !== prev.digest) {
        this.recordBackup(rel, true, prev.content, prev.mode, curr.content);
        const abs = resolve(this.root, rel);
        if (this.revisions.has(abs)) this.revisions.set(abs, curr.digest);
      }
    }
    for (const [rel, curr] of after) {
      if (!before.has(rel)) {
        this.recordBackup(rel, false, undefined, undefined, curr.content);
      }
    }
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
  async previewDiff(tool: string, input: unknown): Promise<string | undefined> {
    try {
      if (!input || typeof input !== 'object') return undefined;
      const rec = input as Record<string, unknown>;
      if (tool === 'apply_patch' && typeof rec.patch === 'string') {
        const ops = parsePatchText(rec.patch);
        const diffs: string[] = [];
        for (const op of ops) {
          const file = await this.path(op.path);
          let before = '';
          try { before = await this.text(file); } catch { before = ''; }
          if (op.kind === 'add') {
            diffs.push(computeUnifiedDiff(op.path, before, op.content));
          } else if (op.kind === 'delete') {
            diffs.push(computeUnifiedDiff(op.path, before, ''));
          } else {
            const after = applyHunksToContent(before, op.hunks, op.path);
            diffs.push(computeUnifiedDiff(op.path, before, after));
          }
        }
        return diffs.join('\n');
      }
      if (typeof rec.path !== 'string') return undefined;
      const file = await this.path(rec.path);
      let before = '';
      try { before = await this.text(file); } catch { before = ''; }
      if (tool === 'write_file' && typeof rec.content === 'string') {
        return computeUnifiedDiff(rec.path, before, rec.content);
      }
      if (tool === 'edit_file' && typeof rec.oldText === 'string' && typeof rec.newText === 'string') {
        const replaceAll = rec.replaceAll === true;
        let targetOld = rec.oldText;
        let targetNew = rec.newText;
        if (!before.includes(targetOld) && before.includes('\r\n') && !targetOld.includes('\r\n')) {
          targetOld = targetOld.replace(/\n/g, '\r\n');
          targetNew = targetNew.replace(/\r?\n/g, '\r\n');
        }
        if (before.includes(targetOld)) {
          const after = replaceAll ? before.split(targetOld).join(targetNew) : before.replace(targetOld, targetNew);
          return computeUnifiedDiff(rec.path, before, after);
        }
        const trimmed = matchTrimmedLines(before, rec.oldText, rec.newText);
        if (trimmed.matched && trimmed.result !== undefined) {
          return computeUnifiedDiff(rec.path, before, trimmed.result);
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

