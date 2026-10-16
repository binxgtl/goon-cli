import { realpath, stat, readFile, readdir, mkdir, open, rename, unlink } from 'node:fs/promises';
import { dirname, resolve, relative, isAbsolute, join } from 'node:path';
import { createHash, randomUUID } from 'node:crypto';

const MAX_FILE_BYTES = 2 * 1024 * 1024;
export const limitText = (text: string, limit = 32_000): string => text.length > limit
  ? `${text.slice(0, limit)}\n...[truncated; narrow the request]` : text;

export class WorkspaceFiles {
  private revisions = new Map<string, string>();
  constructor(readonly root: string) {}
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
  async write(path: string, content: string) {
    const file = await this.path(path);
    if (Buffer.byteLength(content) > MAX_FILE_BYTES) throw new Error('Write exceeds 2 MiB.');
    let mode = 0o644;
    let existing = false;
    try {
      const current = await this.text(file);
      if (this.revisions.get(file) !== this.digest(current)) throw new Error('Read the current file before overwriting; it is unread or has changed.');
      mode = (await stat(file)).mode & 0o777;
      existing = true;
    } catch (error) { if ((error as NodeJS.ErrnoException).code !== 'ENOENT') throw error; }
    await mkdir(dirname(file), { recursive: true });
    // Re-resolve after creating parents to reject a pre-existing symlink escape.
    if (await this.path(path) !== file) throw new Error('Path changed while preparing the write.');
    const temporary = join(dirname(file), `.goon-${randomUUID()}.tmp`);
    const handle = await open(temporary, 'wx', mode);
    try {
      await handle.writeFile(content, 'utf8');
      // open(mode) is filtered by umask. Restore existing permissions exactly;
      // new files should still obey the caller's umask.
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
    return `Wrote ${path} (${Buffer.byteLength(content)} bytes).`;
  }
  async edit(path: string, oldText: string, newText: string, replaceAll = false) {
    const file = await this.path(path);
    const current = await this.text(file);
    if (this.revisions.get(file) !== this.digest(current)) throw new Error('Read the current file before editing; it is unread or has changed.');
    const count = current.split(oldText).length - 1;
    if (!count || (!replaceAll && count !== 1)) throw new Error(`Expected one exact match; found ${count}. Use replaceAll explicitly for multiple matches.`);
    return this.write(path, replaceAll ? current.split(oldText).join(newText) : current.replace(oldText, newText));
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
