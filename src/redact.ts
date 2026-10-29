/**
 * Shared redaction helpers. Event logs, memory indexes and RAG chunks all pass through here so a
 * credential never reaches disk or another process. The patterns are intentionally conservative:
 * false positives only cost a `[REDACTED]` token, false negatives would leak a secret.
 */

/** Values of environment variables that look like credentials, longest first. */
const SECRET_ENV_KEY = /(?:^|_)(?:API_KEY|APIKEY|TOKEN|SECRET|PASSWORD)$/i;
const MIN_SECRET_LENGTH = 6;

/** Object keys whose whole value is a credential, regardless of the value's shape. */
const SENSITIVE_KEY = /^(api[_-]?key|apikey|token|secret|password|authorization|access[_-]?token|refresh[_-]?token|client[_-]?secret)$/i;

const BEARER_RE = /\b(bearer)\s+[A-Za-z0-9._~+/=-]+/gi;
const JSON_KEY_VALUE_RE =
  /("(?:(?:api|access|refresh|client)[_-]?(?:key|token|secret)|token|secret|password|authorization)"\s*:\s*)("(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*')/gi;
const KEY_VALUE_RE =
  /((?:api[_-]?key|apikey|access[_-]?token|refresh[_-]?token|client[_-]?secret|token|secret|password|authorization)\s*[:=]\s*)(?:"[^"]*"|'[^']*'|[^\s"',}]+)/gi;

export function collectSecrets(env: Record<string, string | undefined> = process.env): string[] {
  const secrets = new Set<string>();
  for (const [key, value] of Object.entries(env)) {
    if (typeof value !== 'string' || value.length < MIN_SECRET_LENGTH) continue;
    if (SECRET_ENV_KEY.test(key)) secrets.add(value);
  }
  return [...secrets].sort((a, b) => b.length - a.length);
}

function normalizeSecrets(secrets?: Iterable<string>): string[] {
  const out = new Set<string>();
  for (const secret of secrets ?? []) {
    // Explicit secrets are redacted regardless of length; only env auto-collection uses the
    // length threshold (a stray `TOKEN=1` must not mangle every log line).
    if (typeof secret === 'string' && secret.length > 0) out.add(secret);
  }
  return [...out].sort((a, b) => b.length - a.length);
}

/** Replaces known literals first, then bearer tokens, then JSON and plain `key=value` pairs. */
export function redactSecrets(text: string, secrets?: Iterable<string>): string {
  if (typeof text !== 'string' || !text) return text;
  let out = text;
  for (const secret of normalizeSecrets(secrets)) {
    if (out.includes(secret)) out = out.split(secret).join('[REDACTED]');
  }
  out = out.replace(BEARER_RE, 'Bearer [REDACTED]');
  out = out.replace(JSON_KEY_VALUE_RE, '$1"[REDACTED]"');
  out = out.replace(KEY_VALUE_RE, '$1[REDACTED]');
  return out;
}

/**
 * Returns a deep copy with credential-looking keys and string values redacted. Never mutates the
 * input: callers still render the original event.
 */
export function redactObject<T>(value: T, secrets?: Iterable<string>): T {
  const known = normalizeSecrets(secrets);
  const seen = new WeakSet<object>();
  const walk = (current: unknown, depth: number): unknown => {
    if (depth > 32) return current;
    if (typeof current === 'string') return redactSecrets(current, known);
    if (!current || typeof current !== 'object') return current;
    if (seen.has(current)) return '[Circular]';
    seen.add(current);
    if (Array.isArray(current)) return current.map(item => walk(item, depth + 1));
    if (current instanceof Date || current instanceof Uint8Array) return current;
    const out: Record<string, unknown> = {};
    for (const [key, child] of Object.entries(current as Record<string, unknown>)) {
      out[key] = SENSITIVE_KEY.test(key) ? '[REDACTED]' : walk(child, depth + 1);
    }
    return out;
  };
  return walk(value, 0) as T;
}

const SENSITIVE_BASENAMES = new Set([
  '.npmrc', '.netrc', '.git-credentials', '.pypirc', '.pgpass', '.htpasswd', '.dockercfg',
  'credentials', 'credentials.json', 'service-account.json',
]);
const SENSITIVE_SEGMENTS = new Set(['.ssh', '.aws', '.gnupg', 'gcloud', 'secrets', '.kube', '.docker', '.azure']);

/** True for paths whose contents must never be logged, indexed or sent to a model. */
export function isSensitivePath(filePath: string): boolean {
  const clean = filePath.replace(/\\/g, '/');
  const segments = clean.split('/').filter(Boolean).map(part => part.toLowerCase());
  if (!segments.length) return false;
  const base = segments[segments.length - 1];
  if (SENSITIVE_BASENAMES.has(base)) return true;
  if (base.startsWith('.env')) return true;
  if (/\.(?:pem|key|p12|pfx|jks|keystore|ppk|kdbx)$/i.test(base)) return true;
  if (/^id_(?:rsa|dsa|ecdsa|ed25519)(?:\.|$)/i.test(base)) return true;
  return segments.slice(0, -1).some(segment => SENSITIVE_SEGMENTS.has(segment));
}
