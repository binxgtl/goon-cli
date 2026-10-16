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
if (process.argv.includes('--stdio')) await mockMcpServer().connect(new StdioServerTransport());
