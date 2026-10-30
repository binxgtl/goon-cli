// Release version selection, shared by the CI plan job (which stamps the binaries) and the
// publish step (which tags and uploads them), so both always agree on the version.
import { readFile } from 'node:fs/promises';
import { join } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const STABLE = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/;
/** Files a release commit may touch; nothing else is allowed in the version-only child commit. */
export const VERSION_FILES = ['package.json', 'npm-shrinkwrap.json', 'Cargo.toml', 'Cargo.lock'];

export function parseVersion(value) {
  if (!STABLE.test(value)) throw new Error(`Expected a stable x.y.z version: ${value}`);
  const parts = value.split('.').map(Number);
  if (parts.some(n => !Number.isSafeInteger(n))) throw new Error('Version component exceeds safe integer range.');
  return parts;
}
export function compareVersions(a, b) {
  const left = parseVersion(a), right = parseVersion(b);
  for (let i = 0; i < 3; i++) if (left[i] !== right[i]) return left[i] < right[i] ? -1 : 1;
  return 0;
}
export function selectVersion(current, tags, head) {
  parseVersion(current);
  const stable = tags.filter(t => t.name.startsWith('v') && STABLE.test(t.name.slice(1)));
  const highest = stable.map(t => t.name.slice(1)).sort(compareVersions).at(-1);
  const exact = stable.find(t => t.name === `v${current}`);
  if (exact?.sha === head && (!highest || compareVersions(current, highest) >= 0)) return current;
  if (!highest || compareVersions(current, highest) > 0) return current;
  const next = parseVersion(highest); next[2]++;
  return parseVersion(next.join('.')).join('.');
}

export function setManifestVersion(manifest, lock, version) {
  parseVersion(version);
  if (manifest.version !== lock.version || manifest.version !== lock.packages?.['']?.version) {
    throw new Error('package.json and npm-shrinkwrap.json versions disagree.');
  }
  return {
    manifest: { ...manifest, version },
    lock: { ...lock, version, packages: { ...lock.packages, '': { ...lock.packages[''], version } } },
  };
}

const CARGO_PACKAGE_VERSION = /^(\[package\][^[]*?^version\s*=\s*")([^"]*)(")/m;
const LOCK_PACKAGE_VERSION = name => new RegExp(`(\\[\\[package\\]\\]\\r?\\nname = "${name.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}"\\r?\\nversion = ")([^"]*)(")`);

/** The [package] version of Cargo.toml and the matching Cargo.lock entry. */
export function cargoVersions(cargoToml, cargoLock, name = 'goon-cli') {
  const manifest = CARGO_PACKAGE_VERSION.exec(cargoToml)?.[2];
  const lock = LOCK_PACKAGE_VERSION(name).exec(cargoLock)?.[2];
  if (!manifest || !lock) throw new Error('Cannot find the package version in Cargo.toml or Cargo.lock.');
  return { manifest, lock };
}

/** Rewrites only the version strings, keeping every other byte of both files. */
export function setCargoVersion(cargoToml, cargoLock, expected, version, name = 'goon-cli') {
  parseVersion(version);
  const current = cargoVersions(cargoToml, cargoLock, name);
  if (current.manifest !== expected || current.lock !== expected) throw new Error('Cargo.toml, Cargo.lock and package.json versions disagree.');
  return {
    cargoToml: cargoToml.replace(CARGO_PACKAGE_VERSION, `$1${version}$3`),
    cargoLock: cargoLock.replace(LOCK_PACKAGE_VERSION(name), `$1${version}$3`),
  };
}

/**
 * Decides which commit gets released and with which version. A rerun may start at the source
 * commit while main already holds this workflow's version-only child commit: that child is
 * released again (same version). Any unrelated newer work on main wins and the run is skipped.
 */
export async function planRelease(root, git) {
  await git('fetch', 'origin', 'main', '--tags');
  let head = await git('rev-parse', 'HEAD');
  const remote = await git('rev-parse', 'origin/main');
  let checkout;
  if (remote !== head) {
    const parent = await git('rev-parse', `${remote}^`);
    const subject = await git('log', '-1', '--format=%s', remote);
    const files = (await git('diff', '--name-only', head, remote)).split('\n').filter(Boolean);
    const releaseChild = parent === head && /^chore: release v\d+\.\d+\.\d+ \[skip ci\]$/.test(subject) &&
      files.length > 0 && files.every(p => VERSION_FILES.includes(p));
    if (!releaseChild) return { skip: true, reason: 'A newer main commit superseded this run.' };
    checkout = remote;
    head = remote;
  }
  const tags = [];
  for (const name of (await git('tag', '--list', 'v*')).split('\n').filter(Boolean)) tags.push({ name, sha: await git('rev-parse', `${name}^{commit}`) });
  // In the release-child case the manifest on disk is still the source's; read the child's.
  const manifest = JSON.parse(checkout ? await git('show', `${checkout}:package.json`) : await readFile(join(root, 'package.json'), 'utf8'));
  return { skip: false, head, remote, checkout, tags, current: manifest.version, version: selectVersion(manifest.version, tags, head) };
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href && process.argv[2] === 'plan') {
  // Not pack-release.mjs's `command`: that module imports this one, and awaiting it from here
  // deadlocks the module graph (Node exits 13 with an unsettled top-level await).
  const { execFile } = await import('node:child_process');
  const { promisify } = await import('node:util');
  const root = fileURLToPath(new URL('..', import.meta.url));
  const git = async (...args) => (await promisify(execFile)('git', args, { cwd: root, maxBuffer: 16 * 1024 * 1024 })).stdout.trim();
  const plan = await planRelease(root, git);
  console.log(JSON.stringify({ skip: plan.skip, version: plan.version ?? '', reason: plan.reason ?? '' }));
}
