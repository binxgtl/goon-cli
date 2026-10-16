const STABLE = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/;

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
