// Resolves (and if needed downloads) the native goon binary for this platform.
// Used by bin/goon.mjs at run time and as the npm postinstall step. Downloads are pinned to the
// package version and verified against vendor/manifest.json (written by the release workflow).
import { createHash } from 'node:crypto';
import { chmodSync, existsSync, mkdirSync, readFileSync, renameSync, statSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { gunzipSync } from 'node:zlib';

const TABLE = {
  'android-arm64': 'aarch64-linux-android',
  'android-arm': 'armv7-linux-androideabi',
  'android-x64': 'x86_64-linux-android',
  'linux-x64': 'x86_64-unknown-linux-musl',
  'linux-arm64': 'aarch64-unknown-linux-musl',
  'darwin-x64': 'x86_64-apple-darwin',
  'darwin-arm64': 'aarch64-apple-darwin',
  'win32-x64': 'x86_64-pc-windows-msvc',
  // Windows on ARM runs the x64 build under emulation.
  'win32-arm64': 'x86_64-pc-windows-msvc',
};

/** Every target a release ships a binary for. */
export const TARGETS = [...new Set(Object.values(TABLE))];

/** Rust target triple of the binary that runs on this Node platform/arch. */
export function targetTriple(platform = process.platform, arch = process.arch) {
  return TABLE[`${platform}-${arch}`];
}

/** Release asset name of a target's gzip-compressed binary. */
export const assetName = triple => `goon-${triple}.gz`;

const exe = platform => (platform === 'win32' ? 'goon.exe' : 'goon');

function newest(paths) {
  let best;
  for (const p of paths) {
    try {
      const mtime = statSync(p).mtimeMs;
      if (!best || mtime > best.mtime) best = { path: p, mtime };
    } catch { /* missing */ }
  }
  return best?.path;
}

function readManifest(root) {
  try {
    return JSON.parse(readFileSync(join(root, 'vendor', 'manifest.json'), 'utf8'));
  } catch {
    return undefined;
  }
}

async function download(root, triple, destination) {
  const manifest = readManifest(root);
  const asset = manifest?.assets?.[triple];
  if (!manifest || !asset) return false;
  const repository = manifest.repository ?? 'binxgtl/goon-cli';
  // GOON_DOWNLOAD_BASE points at a mirror (URL) or a local directory holding the release assets.
  const base = process.env.GOON_DOWNLOAD_BASE || `https://github.com/${repository}/releases/download/v${manifest.version}`;
  let packed;
  if (/^https?:\/\//i.test(base)) {
    const url = `${base.replace(/\/+$/, '')}/${asset.file}`;
    const response = await fetch(url, { redirect: 'follow', signal: AbortSignal.timeout(120_000) });
    if (!response.ok) throw new Error(`downloading ${url} failed: HTTP ${response.status}`);
    packed = Buffer.from(await response.arrayBuffer());
  } else {
    packed = readFileSync(join(base, asset.file));
  }
  const bytes = asset.file.endsWith('.gz') ? gunzipSync(packed) : packed;
  const digest = createHash('sha256').update(bytes).digest('hex');
  if (digest !== asset.sha256) throw new Error(`checksum mismatch for ${asset.file} (expected ${asset.sha256}, got ${digest})`);
  mkdirSync(dirname(destination), { recursive: true });
  const temp = `${destination}.${process.pid}.tmp`;
  writeFileSync(temp, bytes, { mode: 0o755 });
  if (process.platform !== 'win32') chmodSync(temp, 0o755);
  renameSync(temp, destination);
  return true;
}

export async function ensureBinary(root = join(dirname(fileURLToPath(import.meta.url)), '..')) {
  const override = process.env.GOON_BINARY;
  if (override) {
    if (!existsSync(override)) throw new Error(`GOON_BINARY points to a missing file: ${override}`);
    return override;
  }
  const triple = targetTriple();
  const name = exe(process.platform);
  if (triple) {
    const vendored = join(root, 'vendor', triple, name);
    if (existsSync(vendored)) return vendored;
  }
  // Source checkout: a local cargo build.
  const local = newest([join(root, 'target', 'release', name), join(root, 'target', 'debug', name)]);
  if (local) return local;
  if (!triple) throw new Error(`no prebuilt goon binary for ${process.platform}/${process.arch}; build from source with "cargo build --release".`);
  const destination = join(root, 'vendor', triple, name);
  if (await download(root, triple, destination)) return destination;
  throw new Error(`the goon binary for ${triple} is missing; reinstall goon or run "cargo build --release" in ${root}.`);
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const root = join(dirname(fileURLToPath(import.meta.url)), '..');
  try {
    const binary = await ensureBinary(root);
    if (process.argv.includes('--print')) console.log(binary);
  } catch (error) {
    // Never fail npm install: bin/goon.mjs retries the download on first run. A source checkout
    // has no release manifest; it runs the local cargo build instead.
    if (existsSync(join(root, 'Cargo.toml'))) console.warn('goon: no binary yet; run "cargo build" (see CONTRIBUTING.md).');
    else console.warn(`goon: ${error instanceof Error ? error.message : String(error)}`);
  }
}
