import { APICallError } from 'ai';
import type {
  LanguageModelV3,
  LanguageModelV3CallOptions,
  LanguageModelV3Content,
  LanguageModelV3Prompt,
  LanguageModelV3StreamPart,
  LanguageModelV3StreamResult,
  LanguageModelV3Usage,
} from '@ai-sdk/provider';
import { type StubFixture, type StubTurn } from './stub-fixtures.js';

export interface StubCursor {
  index: number;
}

export function createStubModel(
  fixture: StubFixture,
  cursor: StubCursor = { index: 0 },
  modelId?: string,
): LanguageModelV3 {
  const resolvedModelId = modelId ?? fixture.model ?? 'stub-model';

  const lastUserText = (prompt: LanguageModelV3Prompt): string => {
    for (let i = prompt.length - 1; i >= 0; i--) {
      const message = prompt[i];
      if (message.role !== 'user') continue;
      const parts = Array.isArray(message.content) ? message.content : [];
      return parts
        .filter(part => part && typeof part === 'object' && part.type === 'text' && typeof part.text === 'string')
        .map(part => (part as { text: string }).text)
        .join('\n');
    }
    return '';
  };

  const selectTurn = (prompt: LanguageModelV3Prompt): StubTurn => {
    const text = lastUserText(prompt);
    const unused = fixture.turns.slice(cursor.index);
    if (unused.length === 0) return { text: ['[stub] script exhausted'] };
    const matches = (needle: string | undefined): boolean => Boolean(needle) && text.includes(needle as string);
    let chosen = unused.findIndex(turn => matches(turn.match));
    if (chosen < 0 && fixture.input && text.includes(fixture.input)) chosen = 0;
    if (chosen < 0 && unused.length === 1) chosen = 0;
    if (chosen >= 0 && chosen < unused.length) {
      cursor.index += chosen + 1;
      return unused[chosen];
    }
    return fixture.auxiliary ?? { text: ['[stub] auxiliary response'] };
  };

  const toUsage = (turn: StubTurn): LanguageModelV3Usage => {
    const usage = turn.usage ?? {};
    const input = usage.inputTokens ?? 0;
    // A total-only fixture must still move token/cost budgets; derive the missing side.
    const output = usage.outputTokens
      ?? (usage.totalTokens !== undefined ? Math.max(0, usage.totalTokens - input) : 0);
    return {
      inputTokens: {
        total: input,
        noCache: input,
        cacheRead: usage.cacheReadTokens ?? 0,
        cacheWrite: usage.cacheWriteTokens ?? 0,
      },
      outputTokens: {
        total: output,
        text: output,
        reasoning: 0,
      },
      raw: undefined,
    };
  };

  const toContent = (turn: StubTurn): LanguageModelV3Content[] => {
    const content: LanguageModelV3Content[] = [];
    for (const reasoning of turn.reasoning ?? []) content.push({ type: 'reasoning', text: reasoning });
    for (const call of turn.toolCalls ?? []) {
      content.push({
        type: 'tool-call',
        toolCallId: call.id,
        toolName: call.name,
        input: JSON.stringify(call.input ?? {}),
      });
    }
    for (const textPart of turn.text ?? []) content.push({ type: 'text', text: textPart });
    return content;
  };

  const throwStubError = (turn: StubTurn): never => {
    const error = turn.error!;
    throw new APICallError({
      message: error.message,
      url: 'stub://local',
      requestBodyValues: {},
      statusCode: error.statusCode ?? 400,
      isRetryable: error.retryable ?? false,
    });
  };

  const finishReason = (turn: StubTurn): { unified: 'stop' | 'tool-calls'; raw: string } => {
    const toolCalls = turn.toolCalls?.length ?? 0;
    const unified = toolCalls > 0 ? 'tool-calls' : 'stop';
    return { unified, raw: unified };
  };

  const buildStream = (turn: StubTurn): LanguageModelV3StreamResult => {
    const parts: LanguageModelV3StreamPart[] = [{ type: 'stream-start', warnings: [] }];
    (turn.reasoning ?? []).forEach((text, index) => {
      const id = `reasoning-${index}`;
      parts.push({ type: 'reasoning-start', id });
      parts.push({ type: 'reasoning-delta', id, delta: text });
      parts.push({ type: 'reasoning-end', id });
    });
    (turn.toolCalls ?? []).forEach(call => {
      const json = JSON.stringify(call.input ?? {});
      parts.push({ type: 'tool-input-start', id: call.id, toolName: call.name });
      parts.push({ type: 'tool-input-delta', id: call.id, delta: json });
      parts.push({ type: 'tool-input-end', id: call.id });
      parts.push({ type: 'tool-call', toolCallId: call.id, toolName: call.name, input: json });
    });
    (turn.text ?? []).forEach((text, index) => {
      if (!text) return;
      const id = `text-${index}`;
      parts.push({ type: 'text-start', id });
      parts.push({ type: 'text-delta', id, delta: text });
      parts.push({ type: 'text-end', id });
    });
    parts.push({ type: 'finish', usage: toUsage(turn), finishReason: finishReason(turn) });
    const stream = new ReadableStream<LanguageModelV3StreamPart>({
      start(controller) {
        for (const part of parts) controller.enqueue(part);
        controller.close();
      },
    });
    return { stream };
  };

  const model: LanguageModelV3 = {
    specificationVersion: 'v3',
    provider: 'goon-stub',
    modelId: resolvedModelId,
    supportedUrls: {},
    doGenerate: async (_options: LanguageModelV3CallOptions) => {
      const turn = selectTurn(_options.prompt);
      if (turn.error) throwStubError(turn);
      return { content: toContent(turn), finishReason: finishReason(turn), usage: toUsage(turn), warnings: [] };
    },
    doStream: async (_options: LanguageModelV3CallOptions) => {
      const turn = selectTurn(_options.prompt);
      if (turn.error) throwStubError(turn);
      return buildStream(turn);
    },
  };
  return model;
}
