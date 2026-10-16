export function safeError(error: unknown, apiKey?: string): string {
  let message = error instanceof Error ? error.message : String(error);
  if (apiKey) message = message.split(apiKey).join('[REDACTED]');
  return message.slice(0, 2000);
}
