import { readdir, readFile } from 'node:fs/promises';
import { basename, join } from 'node:path';
import { configDir } from './config.js';

/**
 * Reusable prompt files ("custom slash commands"). A Markdown file `review-pr.md` in one of the
 * prompt folders becomes `/prompts:review-pr [args]`. `$ARGUMENTS` is replaced by everything typed
 * after the command and `$1`..`$9` by the individual whitespace-separated words. An optional YAML
 * front matter block may carry `description:`.
 *
 * Workspace folders are checked first, so a project can ship its own commands:
 *   <workspace>/.goon/prompts, <workspace>/.agents/prompts, ~/.config/goon/prompts, ~/.codex/prompts
 */
export interface PromptFile {
  name: string;
  description: string;
  path: string;
  scope: 'workspace' | 'user';
  body: string;
}

const NAME_RE = /^[A-Za-z0-9][A-Za-z0-9_.-]{0,63}$/;
const MAX_PROMPT_BYTES = 64 * 1024;

export function promptDirs(cwd: string, home = process.env.HOME ?? process.env.USERPROFILE ?? ''): Array<{ dir: string; scope: 'workspace' | 'user' }> {
  return [
    { dir: join(cwd, '.goon', 'prompts'), scope: 'workspace' },
    { dir: join(cwd, '.agents', 'prompts'), scope: 'workspace' },
    { dir: join(configDir(), 'prompts'), scope: 'user' },
    ...(home ? [{ dir: join(home, '.codex', 'prompts'), scope: 'user' as const }] : []),
  ];
}

function parseFrontMatter(text: string): { description?: string; body: string } {
  const match = /^---\r?\n([\s\S]*?)\r?\n---\r?\n?/.exec(text);
  if (!match) return { body: text };
  const description = /^description\s*:\s*(.+)$/m.exec(match[1])?.[1]?.trim().replace(/^["']|["']$/g, '');
  return { ...(description ? { description } : {}), body: text.slice(match[0].length) };
}

export async function discoverPrompts(cwd: string): Promise<PromptFile[]> {
  const seen = new Set<string>();
  const prompts: PromptFile[] = [];
  for (const { dir, scope } of promptDirs(cwd)) {
    let entries;
    try { entries = await readdir(dir, { withFileTypes: true }); } catch { continue; }
    for (const entry of entries.sort((a, b) => a.name.localeCompare(b.name))) {
      if (!entry.isFile() || !entry.name.toLowerCase().endsWith('.md')) continue;
      const name = basename(entry.name, entry.name.slice(entry.name.lastIndexOf('.')));
      if (!NAME_RE.test(name) || seen.has(name.toLowerCase())) continue;
      const path = join(dir, entry.name);
      let text: string;
      try { text = await readFile(path, 'utf8'); } catch { continue; }
      if (Buffer.byteLength(text) > MAX_PROMPT_BYTES) continue;
      const { description, body } = parseFrontMatter(text);
      const firstLine = body.split(/\r?\n/).find(line => line.trim())?.trim() ?? '';
      seen.add(name.toLowerCase());
      prompts.push({ name, description: description ?? firstLine.slice(0, 100), path, scope, body: body.trim() });
    }
  }
  return prompts;
}

/** Substitutes `$ARGUMENTS` and `$1`..`$9`; `$$` is a literal dollar sign. */
export function expandPromptTemplate(body: string, args: string): string {
  const words = args.trim() ? args.trim().split(/\s+/) : [];
  const expanded = body.replace(/\$(\$|ARGUMENTS|[1-9])/g, (_, token: string) => {
    if (token === '$') return '$';
    if (token === 'ARGUMENTS') return args.trim();
    return words[Number(token) - 1] ?? '';
  });
  // A template without placeholders still receives what the user typed after the command.
  return !/\$(?:ARGUMENTS|[1-9])/.test(body) && args.trim() ? `${expanded}\n\n${args.trim()}` : expanded;
}
