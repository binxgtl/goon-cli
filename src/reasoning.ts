import type { ModelMessage } from 'ai';

export type InlineReasoning = 'leading-think' | 'off';
export type AnswerPart = { type: 'text' | 'reasoning'; text: string };
const OPEN = '<think>', CLOSE = '</think>';

/** Presentation projection only. Provider messages and signed replay data stay intact. */
export class AnswerProjection {
  private mode: 'prefix' | 'reasoning' | 'answer' = 'prefix';
  private pending = '';
  private whitespace = '';
  private depth = 0;
  private ended = false;
  sawReasoning = false;
  constructor(readonly policy: InlineReasoning = 'leading-think') {}
  push(text: string): AnswerPart[] {
    if (this.ended) throw new Error('Answer projection is already finished.');
    if (this.policy === 'off') return text ? [{ type: 'text', text }] : [];
    const parts: AnswerPart[] = [];
    const append = (type: AnswerPart['type'], value: string) => {
      if (!value) return;
      const last = parts.at(-1);
      if (last?.type === type) last.text += value;
      else parts.push({ type, text: value });
    };
    for (let i = 0; i < text.length; i++) {
      if (this.mode === 'answer') { append('text', text.slice(i)); break; }
      const char = text[i];
      if (this.mode === 'prefix') {
        if (!this.pending && /\s/.test(char)) { this.whitespace += char; continue; }
        this.pending += char;
        if (OPEN.startsWith(this.pending.toLowerCase())) {
          if (this.pending.length === OPEN.length) {
            this.pending = ''; this.whitespace = ''; this.depth = 1;
            this.mode = 'reasoning'; this.sawReasoning = true;
          }
        } else {
          append('text', this.whitespace + this.pending);
          this.pending = ''; this.whitespace = ''; this.mode = 'answer';
        }
        continue;
      }
      // Hold only a possible marker suffix. Never rescan accumulated reasoning.
      this.pending += char;
      const lower = this.pending.toLowerCase();
      if (lower === OPEN) { this.depth++; this.pending = ''; }
      else if (lower === CLOSE) {
        this.pending = '';
        if (--this.depth === 0) this.mode = 'prefix';
      } else {
        while (this.pending && !OPEN.startsWith(this.pending.toLowerCase()) && !CLOSE.startsWith(this.pending.toLowerCase())) {
          append('reasoning', this.pending[0]); this.pending = this.pending.slice(1);
        }
      }
    }
    return parts;
  }
  finish(): AnswerPart[] {
    if (this.ended) return [];
    this.ended = true;
    // An unclosed think block stays reasoning, including an incomplete close tag.
    // A merely partial opening tag is literal text on a successful end.
    const text = this.whitespace + this.pending;
    this.whitespace = ''; this.pending = '';
    return text ? [{ type: this.mode === 'reasoning' ? 'reasoning' : 'text', text }] : [];
  }
}

export function visibleAnswer(text: string, policy?: InlineReasoning): string {
  const projection = new AnswerProjection(policy);
  return [...projection.push(text), ...projection.finish()].filter(p => p.type === 'text').map(p => p.text).join('');
}

/** A child's report is its last visible assistant answer, not all tool preambles. */
export function lastAssistantAnswer(messages: ModelMessage[], policy?: InlineReasoning): string {
  for (let i = messages.length - 1; i >= 0; i--) {
    const message = messages[i];
    if (message.role !== 'assistant') continue;
    const raw = typeof message.content === 'string' ? message.content : message.content
      .filter(part => part.type === 'text').map(part => part.text).join('');
    const answer = visibleAnswer(raw, policy);
    if (answer.trim()) return answer;
  }
  return '';
}
