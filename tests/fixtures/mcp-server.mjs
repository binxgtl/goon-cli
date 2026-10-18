import { Server } from '@modelcontextprotocol/sdk/server/index.js';
import { StdioServerTransport } from '@modelcontextprotocol/sdk/server/stdio.js';
import { CallToolRequestSchema, ListToolsRequestSchema } from '@modelcontextprotocol/sdk/types.js';
import { setTimeout } from 'node:timers/promises';

export function mockMcpServer() {
  const server = new Server({ name: 'goon-test-server', version: '1.0.0' }, { capabilities: { tools: {} } });
  server.setRequestHandler(ListToolsRequestSchema, request => ({ tools: request.params?.cursor ? [
    { name: 'danger-tool', description: 'Claims read-only but must require approval', inputSchema: { type: 'object', properties: {} }, annotations: { readOnlyHint: true } },
  ] : [
    { name: 'echo', description: 'Echo input', inputSchema: { type: 'object', properties: { text: { type: 'string' } }, required: ['text'], additionalProperties: false } },
    { name: 'wait', description: 'Wait until cancelled', inputSchema: { type: 'object', properties: {} } },
    { name: 'fail', description: 'Return an error', inputSchema: { type: 'object', properties: {} } },
  ], ...(request.params?.cursor ? {} : { nextCursor: 'page2' }) }));
  server.setRequestHandler(CallToolRequestSchema, async (request, { signal }) => {
    if (request.params.name === 'wait') await setTimeout(30_000, undefined, { signal });
    if (request.params.name === 'fail') return { content: [{ type: 'text', text: 'Tool failed as requested' }], isError: true };
    if (request.params.name === 'echo' && request.params.arguments?.text === 'env') return { content: [{ type: 'text', text: JSON.stringify({
      forwarded: process.env.FORWARDED, apiKey: process.env.GOON_API_KEY ?? null,
    }) }] };
    return { content: [{ type: 'text', text: String(request.params.arguments?.text ?? 'done') }] };
  });
  return server;
}
export function mockReaScaleServer(count = 207) {
  const server = new Server({ name: 'rea-scale-test-server', version: '6.7.0' }, { capabilities: { tools: {} } });
  const bigDesc = 'X'.repeat(13_000);
  const tools = Array.from({ length: count }, (_, idx) => ({
    name: `rea_tool_${idx + 1}`,
    description: `REA tool #${idx + 1} ${bigDesc}`,
    inputSchema: {
      $schema: 'https://json-schema.org/draft/2020-12/schema',
      $id: `https://rea.example/schemas/tool_${idx + 1}.json`,
      type: 'object',
      properties: {
        query: {
          type: 'string',
          description: idx === 0 ? 'Y'.repeat(22_000) : 'Query parameter',
        },
        mode: {
          const: 'fast',
        },
      },
      required: ['query'],
    },
  }));
  server.setRequestHandler(ListToolsRequestSchema, () => ({ tools }));
  server.setRequestHandler(CallToolRequestSchema, async request => ({
    content: [{ type: 'text', text: `called:${request.params.name}:${request.params.arguments?.query ?? ''}` }],
  }));
  return server;
}
if (process.argv.includes('--stdio') || process.argv.includes('--rea-scale')) {
  process.stdin.on('end', () => process.exit(0));
  process.stdin.on('close', () => process.exit(0));
}
if (process.argv.includes('--stdio')) await mockMcpServer().connect(new StdioServerTransport());
if (process.argv.includes('--rea-scale')) await mockReaScaleServer().connect(new StdioServerTransport());
