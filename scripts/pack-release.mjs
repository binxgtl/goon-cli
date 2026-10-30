// Packs a release: the npm wrapper package (bin/, scripts/, vendor/manifest.json) plus one
// gzip-compressed native binary per target. The wrapper downloads the binary for its platform
// on install and verifies it against the SHA-256 recorded in vendor/manifest.json.
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { readFile, writeFile, copyFile, mkdir, mkdtemp, rm, readdir } from 'node:fs/promises';
import { join, resolve, dirname } from 'node:path';
import { tmpdir } from 'node:os';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { createHash } from 'node:crypto';
import { gunzipSync } from 'node:zlib';
import { parseVersion } from './release-version.mjs';
import { TARGETS, assetName } from './install.mjs';

const exec = promisify(execFile);
export async function command(program, args, cwd, env) {
  const { stdout } = await exec(program, args, { cwd, maxBuffer: 16 * 1024 * 1024, env: env ? { ...process.env, ...env } : process.env, shell: process.platform === 'win32' && program === 'npm' });
  return stdout.trim();
}

const sha256 = bytes => createHash('sha256').update(bytes).digest('hex');

/** Reads `goon-<triple>.gz` for every release target from `binaries` and records its digest. */
export async function binaryManifest(binaries, version, repository) {
  const present = new Set(await readdir(binaries).catch(() => []));
  const missing = TARGETS.filter(t => !present.has(assetName(t)));
  if (missing.length) throw new Error(`Missing release binaries in ${binaries}: ${missing.map(assetName).join(', ')}`);
  const assets = {};
  for (const triple of TARGETS) {
    const file = assetName(triple);
    const packed = await readFile(join(binaries, file));
    const bytes = gunzipSync(packed);
    if (bytes.length < 1024) throw new Error(`${file} is too small to be a goon binary.`);
    assets[triple] = { file, sha256: sha256(bytes), size: bytes.length, gzSha256: sha256(packed) };
  }
  return { version, repository, assets };
}

export async function packRelease(root = resolve(dirname(fileURLToPath(import.meta.url)), '..'), options = {}) {
  const { version } = JSON.parse(await readFile(join(root, 'package.json'), 'utf8'));
  parseVersion(version);
  const binaries = resolve(root, options.binaries ?? process.env.GOON_RELEASE_BINARIES ?? 'release-bin');
  const repository = options.repository ?? process.env.GITHUB_REPOSITORY ?? 'binxgtl/goon-cli';
  const manifest = await binaryManifest(binaries, version, repository);
  await mkdir(join(root, 'vendor'), { recursive: true });
  await writeFile(join(root, 'vendor', 'manifest.json'), `${JSON.stringify(manifest, null, 2)}\n`);

  const output = join(root, 'release-out');
  await rm(output, { recursive: true, force: true });
  await mkdir(output, { recursive: true });
  const packed = JSON.parse(await command('npm', ['pack', '--ignore-scripts', '--json', '--pack-destination', output], root));
  const filename = `goon-cli-${version}.tgz`;
  if (packed.length !== 1 || packed[0].filename !== filename) throw new Error('Unexpected npm pack output.');
  const packedFiles = packed[0].files.map(f => f.path);
  if (!packedFiles.includes('vendor/manifest.json')) throw new Error('The npm package must contain vendor/manifest.json.');
  if (packedFiles.some(p => /^vendor\/.+\/goon(\.exe)?$/.test(p))) throw new Error('Native binaries must not be packed into the npm package.');
  const archive = join(output, filename);

  // Smoke test: install the archive as users do and let the wrapper fetch, verify and run the
  // native binary for this machine from the freshly built assets.
  const prefix = await mkdtemp(join(tmpdir(), 'goon-release-'));
  try {
    await command('npm', ['install', '--prefix', prefix, archive, '--omit=dev', '--no-audit', '--no-fund'], root, { GOON_DOWNLOAD_BASE: binaries });
    const bin = join(prefix, 'node_modules', 'goon-cli', 'bin', 'goon.mjs');
    const env = { GOON_DOWNLOAD_BASE: binaries, GOON_BINARY: '' };
    const reported = await command(process.execPath, [bin, '--version'], root, env);
    if (reported !== `goon cli ${version}`) throw new Error(`Installed archive reports the wrong version: ${reported}`);
    await command(process.execPath, [bin, '--help'], root, env);
  } finally { await rm(prefix, { recursive: true, force: true }); }

  await copyFile(archive, join(output, 'goon-cli.tgz'));
  const files = [archive, join(output, 'goon-cli.tgz')];
  const sums = [`${sha256(await readFile(archive))}  ${filename}`, `${sha256(await readFile(archive))}  goon-cli.tgz`];
  for (const triple of TARGETS) {
    const file = assetName(triple);
    await copyFile(join(binaries, file), join(output, file));
    files.push(join(output, file));
    sums.push(`${manifest.assets[triple].gzSha256}  ${file}`);
  }
  const checksums = join(output, 'SHA256SUMS.txt');
  await writeFile(checksums, `${sums.join('\n')}\n`);
  files.push(checksums);
  return { version, files, output, manifest };
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  console.log(JSON.stringify(await packRelease()));
}
