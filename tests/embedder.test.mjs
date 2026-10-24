import test from 'node:test';
import assert from 'node:assert/strict';
import { rm, mkdtemp, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createEmbedder, EmbedderError } from '../dist/embedder.js';
import { HashEmbedder } from '../dist/rag.js';
import { loadConfig } from '../dist/config.js';

const KEY = 'sk-supersecret-embedder-key';

const jsonFetch = (payload, capture = []) => async (url, init) => {
  const request = { url, method: init?.method, headers: init?.headers, body: JSON.parse(init?.body ?? '{}') };
  capture.push(request);
  return {
    ok: true,
    status: 200,
    text: async () => JSON.stringify(typeof payload === 'function' ? payload(request) : payload),
  };
};

test('hash provider: default factory is byte-for-byte the HashEmbedder', async () => {
  const embedder = createEmbedder({ provider: 'hash' });
  assert.ok(embedder instanceof HashEmbedder);
  const texts = ['export function alpha() { return 1; }', 'beta gamma beta'];
  const vectors = await embedder.embed(texts);
  const reference = await new HashEmbedder().embed(texts);
  assert.equal(vectors.length, 2);
  assert.equal(vectors[0].length, 384);
  assert.deepEqual(vectors[0], reference[0]);
  assert.deepEqual(vectors[1], reference[1]);
  // Missing fields on the options object cannot accidentally switch the provider.
  assert.ok(createEmbedder({ provider: 'hash', model: 'ignored', apiKey: KEY }) instanceof HashEmbedder);
});

test('openai provider: batches requests, sends the bearer header only, maps by index', async () => {
  const capture = [];
  const embedder = createEmbedder({
    provider: 'openai',
    model: 'text-embedding-3-small',
    baseUrl: 'https://api.example.test/v1',
    apiKey: KEY,
    fetchImpl: jsonFetch(request => ({ data: request.body.input.map((_, i) => ({ index: i, embedding: [1, 0, 0] })) }), capture),
    batchSize: 2,
  });
  const vectors = await embedder.embed(['a', 'b', 'c'], undefined);
  assert.equal(vectors.length, 3);
  assert.deepEqual(vectors[2], [1, 0, 0]);
  assert.equal(capture.length, 2, 'batchSize 2 -> two HTTP requests');
  assert.equal(capture[0].url, 'https://api.example.test/v1/embeddings');
  assert.equal(capture[0].method, 'POST');
  assert.equal(capture[0].headers.authorization, `Bearer ${KEY}`, 'the key lives only in the Authorization header');
  assert.deepEqual(capture[0].body, { model: 'text-embedding-3-small', input: ['a', 'b'] });
  assert.deepEqual(capture[1].body, { model: 'text-embedding-3-small', input: ['c'] });
});

test('openai provider: wrong vector count is a hard error', async () => {
  const embedder = createEmbedder({
    provider: 'openai',
    model: 'm',
    baseUrl: 'https://api.example.test/v1',
    fetchImpl: jsonFetch({ data: [{ index: 0, embedding: [1] }] }),
  });
  await assert.rejects(embedder.embed(['a', 'b']), error => error instanceof EmbedderError && /vectors for 2 input/.test(error.message));
});

test('ollama provider: posts to /api/embed, never forwards the key, maps embeddings', async () => {
  const capture = [];
  const embedder = createEmbedder({
    provider: 'ollama',
    model: 'nomic-embed-text',
    apiKey: KEY, // must be ignored: ollama is local and keyless
    fetchImpl: jsonFetch({ embeddings: [[0, 1], [1, 1]] }, capture),
  });
  const vectors = await embedder.embed(['x', 'y']);
  assert.deepEqual(vectors, [[0, 1], [1, 1]]);
  assert.equal(capture.length, 1);
  assert.equal(capture[0].url, 'http://127.0.0.1:11434/api/embed', 'ollama defaults to the local daemon');
  assert.equal(capture[0].headers.authorization, undefined, 'no Authorization header reaches ollama');
  assert.equal(JSON.stringify(capture[0].headers).includes(KEY), false, 'the key appears nowhere in the request');
  assert.deepEqual(capture[0].body, { model: 'nomic-embed-text', input: ['x', 'y'] });
});

test('errors never leak the api key (HTTP failure, network failure, non-JSON body)', async () => {
  const failing = async () => ({ ok: false, status: 401, text: async () => `{"error":"bad key ${KEY}"}` });
  const openai = createEmbedder({ provider: 'openai', model: 'm', baseUrl: 'https://api.example.test/v1', apiKey: KEY, fetchImpl: failing });
  await assert.rejects(openai.embed(['a']), error => {
    assert.equal(error instanceof EmbedderError, true);
    assert.equal(error.message.includes(KEY), false, `leaked in: ${error.message}`);
    assert.match(error.message, /\[REDACTED\]/);
    assert.match(error.message, /status 401/);
    return true;
  });
  const boom = async () => { throw new Error(`connect ECONNREFUSED with ${KEY}`); };
  const networked = createEmbedder({ provider: 'openai', model: 'm', baseUrl: 'https://api.example.test/v1', apiKey: KEY, fetchImpl: boom });
  await assert.rejects(networked.embed(['a']), error => {
    assert.equal(error.message.includes(KEY), false, `leaked in: ${error.message}`);
    return true;
  });
  const garbage = async () => ({ ok: true, status: 200, text: async () => `<html>${KEY}</html>` });
  const nonJson = createEmbedder({ provider: 'openai', model: 'm', baseUrl: 'https://api.example.test/v1', apiKey: KEY, fetchImpl: garbage });
  await assert.rejects(nonJson.embed(['a']), error => {
    assert.equal(error.message.includes(KEY), false, `leaked in: ${error.message}`);
    return true;
  });
});

test('factory fails closed on missing model or baseUrl without touching credentials', async () => {
  assert.throws(
    () => createEmbedder({ provider: 'openai', baseUrl: 'https://api.example.test/v1' }),
    error => error instanceof EmbedderError && /requires embedderModel/.test(error.message),
  );
  assert.throws(
    () => createEmbedder({ provider: 'openai', model: 'm' }),
    error => error instanceof EmbedderError && /requires embedderBaseUrl/.test(error.message),
  );
  assert.equal(createEmbedder({ provider: 'ollama', model: 'm' }).constructor.name, 'OllamaEmbedder', 'ollama needs no baseUrl (default local daemon)');
});

test('config: embedderProvider defaults to hash and old configs still parse', async t => {
  const dir = await mkdtemp(join(tmpdir(), 'goon-embedder-cfg-'));
  t.after(() => rm(dir, { recursive: true, force: true, maxRetries: 5, retryDelay: 50 }));
  await writeFile(join(dir, 'config.json'), JSON.stringify({}));
  const previousXdg = process.env.XDG_CONFIG_HOME;
  const previousEnv = { provider: process.env.GOON_EMBEDDER_PROVIDER, model: process.env.GOON_EMBEDDER_MODEL };
  process.env.XDG_CONFIG_HOME = dir;
  delete process.env.GOON_EMBEDDER_PROVIDER;
  delete process.env.GOON_EMBEDDER_MODEL;
  try {
    const defaults = await loadConfig();
    assert.equal(defaults.embedderProvider, 'hash');
    assert.equal(defaults.embedderModel, undefined);
    process.env.GOON_EMBEDDER_PROVIDER = 'ollama';
    process.env.GOON_EMBEDDER_MODEL = 'nomic-embed-text';
    const fromEnv = await loadConfig();
    assert.equal(fromEnv.embedderProvider, 'ollama');
    assert.equal(fromEnv.embedderModel, 'nomic-embed-text');
    assert.equal(fromEnv.memoryEnabled, false, 'unrelated defaults untouched');
  } finally {
    if (previousXdg === undefined) delete process.env.XDG_CONFIG_HOME;
    else process.env.XDG_CONFIG_HOME = previousXdg;
    if (previousEnv.provider === undefined) delete process.env.GOON_EMBEDDER_PROVIDER;
    else process.env.GOON_EMBEDDER_PROVIDER = previousEnv.provider;
    if (previousEnv.model === undefined) delete process.env.GOON_EMBEDDER_MODEL;
    else process.env.GOON_EMBEDDER_MODEL = previousEnv.model;
  }
});
