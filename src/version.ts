import { readFileSync } from 'node:fs';

// The installed manifest is the version owner; release bumps must also update
// CLI help, doctor and --version without editing source constants.
export const VERSION: string = JSON.parse(readFileSync(new URL('../package.json', import.meta.url), 'utf8')).version;
