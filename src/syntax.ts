/**
 * Cheap, dependency-free syntax guard for the files the agent writes.
 *
 * This is deliberately a heuristic, not a parser: it catches the common failure mode of an
 * edit that drops a closing brace, without pulling in a native parser. It is safe to be
 * permissive (a broken file may slip through) but it must not reject valid code, so strings,
 * comments, template literals and regular-expression literals are skipped. Known limitations:
 * JSX text containing unbalanced braces, and regex-vs-division detection in exotic code, may
 * produce false positives; both are documented in docs/PLAN-yolo-port.md §5.2.
 */

const JS_EXTENSIONS = new Set(['.ts', '.tsx', '.mts', '.cts', '.js', '.jsx', '.mjs', '.cjs']);

const REGEX_PRECEDING_CHARS = new Set(['(', ',', '=', ':', '[', '!', '&', '|', '?', '{', '}', ';', '+', '-', '*', '%', '^', '~', '<', '>']);
const REGEX_PRECEDING_WORDS = new Set([
  'return', 'typeof', 'instanceof', 'in', 'of', 'new', 'delete', 'void', 'throw', 'case',
  'do', 'else', 'yield', 'await',
]);

type Opener = '{' | '(' | '[';
type StackEntry = { char: Opener | '${'; line: number };

const CLOSERS: Record<string, Opener> = { '}': '{', ')': '(', ']': '[' };

/** JSON files that legitimately allow comments/trailing commas (JSONC). */
export function isJsoncPath(path: string): boolean {
  const posix = path.replace(/\\/g, '/');
  const base = posix.split('/').pop() ?? '';
  if (/^(?:ts|js)config[^/]*\.json$/i.test(base)) return true;
  if (/\.jsonc$/i.test(base)) return true;
  if (base.toLowerCase() === 'devcontainer.json') return true;
  return /(^|\/)\.vscode\//.test(posix);
}

/**
 * Validates `content` as if it were written to `path`. Throws an Error describing the first
 * problem; returns silently for unknown extensions and JSONC files.
 */
export function validateSyntax(path: string, content: string): void {
  const base = path.replace(/\\/g, '/').split('/').pop() ?? '';
  const dot = base.lastIndexOf('.');
  const ext = dot > 0 ? base.slice(dot).toLowerCase() : '';
  if (ext === '.json') {
    if (isJsoncPath(path)) return;
    const text = content.charCodeAt(0) === 0xfeff ? content.slice(1) : content;
    try {
      JSON.parse(text);
    } catch (error) {
      throw new Error(`invalid JSON: ${(error as Error).message}`);
    }
    return;
  }
  if (!JS_EXTENSIONS.has(ext)) return;
  validateBrackets(content);
}

function validateBrackets(content: string): void {
  const stack: StackEntry[] = [];
  let line = 1;
  let i = 0;
  let state: 'code' | 'template' | 'single' | 'double' | 'block' | 'line' | 'regex' = 'code';
  // Last significant character/word in code state, used to tell `/` division from a regex.
  let lastChar = '';
  let lastWord = '';

  const fail: (message: string) => never = message => {
    throw new Error(message);
  };

  while (i < content.length) {
    const ch = content[i];

    if (state === 'line') {
      if (ch === '\n') { line++; state = 'code'; }
      i++;
      continue;
    }
    if (state === 'block') {
      if (ch === '\n') line++;
      else if (ch === '*' && content[i + 1] === '/') { state = 'code'; i += 2; continue; }
      i++;
      continue;
    }
    if (state === 'single' || state === 'double') {
      if (ch === '\\') {
        if (content[i + 1] === '\n') line++;
        i += 2;
        continue;
      }
      if (ch === '\n') line++;
      if ((state === 'single' && ch === "'") || (state === 'double' && ch === '"')) state = 'code';
      i++;
      continue;
    }
    if (state === 'template') {
      if (ch === '\\') {
        if (content[i + 1] === '\n') line++;
        i += 2;
        continue;
      }
      if (ch === '\n') line++;
      if (ch === '`') { state = 'code'; lastChar = '`'; lastWord = ''; i++; continue; }
      if (ch === '$' && content[i + 1] === '{') {
        stack.push({ char: '${', line });
        state = 'code';
        lastChar = '';
        lastWord = '';
        i += 2;
        continue;
      }
      i++;
      continue;
    }
    if (state === 'regex') {
      if (ch === '\\') { i += 2; continue; }
      if (ch === '[') {
        // Character class: `]` inside is the only terminator to respect.
        i++;
        while (i < content.length) {
          if (content[i] === '\\') { i += 2; continue; }
          if (content[i] === ']') { i++; break; }
          if (content[i] === '\n') { line++; break; }
          i++;
        }
        continue;
      }
      if (ch === '/') { state = 'code'; lastChar = 'a'; lastWord = ''; i++; continue; }
      if (ch === '\n') { line++; state = 'code'; lastChar = 'a'; i++; continue; }
      i++;
      continue;
    }

    // state === 'code'
    if (ch === '\n') { line++; i++; continue; }
    if (ch === '/' && content[i + 1] === '/') { state = 'line'; i += 2; continue; }
    if (ch === '/' && content[i + 1] === '*') { state = 'block'; i += 2; continue; }
    if (ch === '/') {
      if (regexAllowed(lastChar, lastWord)) { state = 'regex'; i++; continue; }
      lastChar = '/';
      lastWord = '';
      i++;
      continue;
    }
    if (ch === '"' || ch === "'") {
      state = ch === '"' ? 'double' : 'single';
      lastChar = ch;
      lastWord = '';
      i++;
      continue;
    }
    if (ch === '`') {
      state = 'template';
      lastChar = '`';
      lastWord = '';
      i++;
      continue;
    }
    if (/[A-Za-z_$]/.test(ch)) {
      let end = i + 1;
      while (end < content.length && /[A-Za-z0-9_$]/.test(content[end])) end++;
      lastWord = content.slice(i, end);
      lastChar = 'a';
      i = end;
      continue;
    }
    if (/[0-9]/.test(ch)) {
      let end = i + 1;
      while (end < content.length && /[0-9._a-fA-FxXoObBeE+-]/.test(content[end])) end++;
      lastChar = '0';
      lastWord = '';
      i = end;
      continue;
    }
    if (ch === '{' || ch === '(' || ch === '[') {
      stack.push({ char: ch, line });
      lastChar = ch;
      lastWord = '';
      i++;
      continue;
    }
    if (ch === '}' || ch === ')' || ch === ']') {
      const top = stack[stack.length - 1];
      if (ch === '}' && top?.char === '${') {
        stack.pop();
        state = 'template';
        lastChar = '}';
        lastWord = '';
        i++;
        continue;
      }
      const expected = CLOSERS[ch];
      if (!top) fail(`Unexpected '${ch}' at line ${line}.`);
      if (top.char !== expected) {
        fail(`Mismatched '${ch}' at line ${line}; expected '${closerFor(top.char)}' to close '${top.char}' from line ${top.line}.`);
      }
      stack.pop();
      lastChar = ch;
      lastWord = '';
      i++;
      continue;
    }
    if (!/\s/.test(ch)) {
      lastChar = ch;
      lastWord = '';
    }
    i++;
  }

  const unclosed = stack[stack.length - 1];
  if (unclosed) {
    if (unclosed.char === '${') {
      throw new Error(`Unclosed template expression at line ${unclosed.line}; expected '}'.`);
    }
    throw new Error(`Unclosed '${unclosed.char}' at line ${unclosed.line}; expected '${closerFor(unclosed.char)}'.`);
  }
}

function closerFor(opener: StackEntry['char']): string {
  return opener === '${' ? '}' : opener === '{' ? '}' : opener === '(' ? ')' : ']';
}

function regexAllowed(lastChar: string, lastWord: string): boolean {
  if (!lastChar) return true;
  if (REGEX_PRECEDING_CHARS.has(lastChar)) return true;
  return REGEX_PRECEDING_WORDS.has(lastWord);
}
