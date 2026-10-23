import { createServer, type IncomingMessage, type Server, type ServerResponse } from 'node:http';
import { spawn, type ChildProcess } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import { networkInterfaces } from 'node:os';
import type { HarnessEvent } from './harness.js';
import type { SubagentEvent } from './subagents.js';

// ============================================================================
// Pure TypeScript ISO/IEC 18004 QR Code Encoder (Versions 1..10, Level L, Byte)
// ============================================================================

interface QrVersionSpec {
  version: number;
  dataCodewords: number;
  ecCodewordsPerBlock: number;
  blocks: Array<{ count: number; dataCount: number }>;
  alignment: number[];
}

const QR_VERSIONS_L: QrVersionSpec[] = [
  { version: 1, dataCodewords: 19, ecCodewordsPerBlock: 7, blocks: [{ count: 1, dataCount: 19 }], alignment: [] },
  { version: 2, dataCodewords: 34, ecCodewordsPerBlock: 10, blocks: [{ count: 1, dataCount: 34 }], alignment: [6, 18] },
  { version: 3, dataCodewords: 55, ecCodewordsPerBlock: 15, blocks: [{ count: 1, dataCount: 55 }], alignment: [6, 22] },
  { version: 4, dataCodewords: 80, ecCodewordsPerBlock: 20, blocks: [{ count: 1, dataCount: 80 }], alignment: [6, 26] },
  { version: 5, dataCodewords: 108, ecCodewordsPerBlock: 26, blocks: [{ count: 1, dataCount: 108 }], alignment: [6, 30] },
  { version: 6, dataCodewords: 136, ecCodewordsPerBlock: 18, blocks: [{ count: 2, dataCount: 68 }], alignment: [6, 34] },
  { version: 7, dataCodewords: 156, ecCodewordsPerBlock: 20, blocks: [{ count: 2, dataCount: 78 }], alignment: [6, 22, 38] },
  { version: 8, dataCodewords: 194, ecCodewordsPerBlock: 24, blocks: [{ count: 2, dataCount: 97 }], alignment: [6, 24, 42] },
  { version: 9, dataCodewords: 232, ecCodewordsPerBlock: 30, blocks: [{ count: 2, dataCount: 116 }], alignment: [6, 26, 46] },
  {
    version: 10,
    dataCodewords: 274,
    ecCodewordsPerBlock: 18,
    blocks: [
      { count: 2, dataCount: 68 },
      { count: 2, dataCount: 69 },
    ],
    alignment: [6, 28, 50],
  },
];

const GF_EXP = new Uint8Array(512);
const GF_LOG = new Uint8Array(256);
(() => {
  let x = 1;
  for (let i = 0; i < 255; i++) {
    GF_EXP[i] = x;
    GF_LOG[x] = i;
    x <<= 1;
    if (x & 0x100) x ^= 0x11d;
  }
  for (let i = 255; i < 512; i++) {
    GF_EXP[i] = GF_EXP[i - 255];
  }
})();

function gfMul(a: number, b: number): number {
  if (a === 0 || b === 0) return 0;
  return GF_EXP[GF_LOG[a] + GF_LOG[b]];
}

function rsGeneratorPoly(degree: number): Uint8Array {
  let poly = new Uint8Array([1]);
  for (let i = 0; i < degree; i++) {
    const next = new Uint8Array(poly.length + 1);
    const root = GF_EXP[i];
    for (let j = 0; j < poly.length; j++) {
      next[j] ^= poly[j];
      next[j + 1] ^= gfMul(poly[j], root);
    }
    poly = next;
  }
  return poly;
}

function rsRemainder(data: Uint8Array, ecCount: number): Uint8Array {
  const gen = rsGeneratorPoly(ecCount);
  const rem = new Uint8Array(ecCount);
  for (let i = 0; i < data.length; i++) {
    const factor = data[i] ^ rem[0];
    rem.copyWithin(0, 1);
    rem[ecCount - 1] = 0;
    if (factor !== 0) {
      for (let j = 0; j < ecCount; j++) {
        rem[j] ^= gfMul(gen[j + 1], factor);
      }
    }
  }
  return rem;
}

function computeFormatBits(ecLevelBits: number, mask: number): number {
  const data = ((ecLevelBits & 0x03) << 3) | (mask & 0x07);
  let rem = data << 10;
  for (let i = 14; i >= 10; i--) {
    if ((rem >>> i) & 1) {
      rem ^= 0x537 << (i - 10);
    }
  }
  return ((data << 10) | rem) ^ 0x5412;
}

function computeVersionBits(version: number): number {
  let rem = version << 12;
  for (let i = 17; i >= 12; i--) {
    if ((rem >>> i) & 1) {
      rem ^= 0x1f25 << (i - 12);
    }
  }
  return (version << 12) | rem;
}

export function encodeQrMatrix(text: string): boolean[][] {
  const payload = Buffer.from(text, 'utf8');
  const spec = QR_VERSIONS_L.find(s => {
    const countBits = s.version <= 9 ? 8 : 16;
    const maxBytes = Math.floor((s.dataCodewords * 8 - 4 - countBits) / 8);
    return payload.length <= maxBytes;
  });
  if (!spec) {
    throw new Error(`QR payload too long (${payload.length} bytes; max 271 bytes).`);
  }

  const bits: number[] = [];
  const pushBits = (val: number, len: number) => {
    for (let i = len - 1; i >= 0; i--) {
      bits.push((val >>> i) & 1);
    }
  };

  // Mode indicator: Byte mode (0100)
  pushBits(0b0100, 4);
  pushBits(payload.length, spec.version <= 9 ? 8 : 16);
  for (const byte of payload) {
    pushBits(byte, 8);
  }

  const totalDataBits = spec.dataCodewords * 8;
  const terminatorLen = Math.min(4, totalDataBits - bits.length);
  pushBits(0, terminatorLen);
  while (bits.length % 8 !== 0) bits.push(0);

  const dataBytes = new Uint8Array(spec.dataCodewords);
  for (let i = 0; i < bits.length / 8; i++) {
    let b = 0;
    for (let j = 0; j < 8; j++) {
      b = (b << 1) | bits[i * 8 + j];
    }
    dataBytes[i] = b;
  }
  const padBytes = [0xec, 0x11];
  for (let i = bits.length / 8, p = 0; i < spec.dataCodewords; i++, p ^= 1) {
    dataBytes[i] = padBytes[p];
  }

  // Split into blocks & compute Reed-Solomon EC codewords
  const dataBlocks: Uint8Array[] = [];
  const ecBlocks: Uint8Array[] = [];
  let offset = 0;
  for (const grp of spec.blocks) {
    for (let i = 0; i < grp.count; i++) {
      const block = dataBytes.subarray(offset, offset + grp.dataCount);
      offset += grp.dataCount;
      dataBlocks.push(block);
      ecBlocks.push(rsRemainder(block, spec.ecCodewordsPerBlock));
    }
  }

  // Interleave data and EC codewords
  const finalCodewords: number[] = [];
  const maxDataLen = Math.max(...dataBlocks.map(b => b.length));
  for (let i = 0; i < maxDataLen; i++) {
    for (const b of dataBlocks) {
      if (i < b.length) finalCodewords.push(b[i]);
    }
  }
  for (let i = 0; i < spec.ecCodewordsPerBlock; i++) {
    for (const b of ecBlocks) {
      if (i < b.length) finalCodewords.push(b[i]);
    }
  }

  const size = 17 + spec.version * 4;
  const modules: boolean[][] = Array.from({ length: size }, () => Array.from({ length: size }, () => false));
  const isFunction: boolean[][] = Array.from({ length: size }, () => Array.from({ length: size }, () => false));

  const setFunctionModule = (r: number, c: number, dark: boolean) => {
    if (r < 0 || r >= size || c < 0 || c >= size) return;
    modules[r][c] = dark;
    isFunction[r][c] = true;
  };

  // Finder patterns + separators
  const drawFinder = (top: number, left: number) => {
    for (let dr = -1; dr <= 7; dr++) {
      for (let dc = -1; dc <= 7; dc++) {
        const r = top + dr;
        const c = left + dc;
        if (r < 0 || r >= size || c < 0 || c >= size) continue;
        const inOuter = dr >= 0 && dr <= 6 && dc >= 0 && dc <= 6;
        const onBorder = dr === 0 || dr === 6 || dc === 0 || dc === 6;
        const inInner = dr >= 2 && dr <= 4 && dc >= 2 && dc <= 4;
        setFunctionModule(r, c, inOuter && (onBorder || inInner));
      }
    }
  };
  drawFinder(0, 0);
  drawFinder(0, size - 7);
  drawFinder(size - 7, 0);

  // Timing patterns
  for (let i = 8; i < size - 8; i++) {
    setFunctionModule(6, i, i % 2 === 0);
    setFunctionModule(i, 6, i % 2 === 0);
  }

  // Alignment patterns
  const align = spec.alignment;
  for (let i = 0; i < align.length; i++) {
    for (let j = 0; j < align.length; j++) {
      if (
        (i === 0 && j === 0) ||
        (i === 0 && j === align.length - 1) ||
        (i === align.length - 1 && j === 0)
      ) {
        continue;
      }
      const cr = align[i];
      const cc = align[j];
      for (let dr = -2; dr <= 2; dr++) {
        for (let dc = -2; dc <= 2; dc++) {
          const dark = Math.max(Math.abs(dr), Math.abs(dc)) !== 1;
          setFunctionModule(cr + dr, cc + dc, dark);
        }
      }
    }
  }

  // Dark module + reserve format areas
  setFunctionModule(size - 8, 8, true);
  for (let i = 0; i <= 8; i++) {
    if (!isFunction[8][i]) setFunctionModule(8, i, false);
    if (!isFunction[i][8]) setFunctionModule(i, 8, false);
  }
  for (let i = 0; i < 8; i++) {
    if (!isFunction[8][size - 1 - i]) setFunctionModule(8, size - 1 - i, false);
    if (!isFunction[size - 1 - i][8]) setFunctionModule(size - 1 - i, 8, false);
  }

  // Version info for version >= 7
  if (spec.version >= 7) {
    const vBits = computeVersionBits(spec.version);
    for (let i = 0; i < 18; i++) {
      const bit = ((vBits >>> i) & 1) === 1;
      const a = size - 11 + (i % 3);
      const b = Math.floor(i / 3);
      setFunctionModule(b, a, bit);
      setFunctionModule(a, b, bit);
    }
  }

  // Place data bits in upward/downward 2-column zig-zag
  const allBits: number[] = [];
  for (const cw of finalCodewords) {
    for (let i = 7; i >= 0; i--) {
      allBits.push((cw >>> i) & 1);
    }
  }

  let bitIdx = 0;
  let upward = true;
  for (let right = size - 1; right >= 1; right -= 2) {
    if (right === 6) right = 5;
    for (let vert = 0; vert < size; vert++) {
      const r = upward ? size - 1 - vert : vert;
      for (let j = 0; j < 2; j++) {
        const c = right - j;
        if (!isFunction[r][c]) {
          modules[r][c] = bitIdx < allBits.length ? allBits[bitIdx++] === 1 : false;
        }
      }
    }
    upward = !upward;
  }

  // Evaluate masks 0..3 and pick lowest penalty
  const applyMaskAndFormat = (mask: number): boolean[][] => {
    const grid = modules.map(row => [...row]);
    for (let r = 0; r < size; r++) {
      for (let c = 0; c < size; c++) {
        if (isFunction[r][c]) continue;
        const invert =
          mask === 0
            ? (r + c) % 2 === 0
            : mask === 1
              ? r % 2 === 0
              : mask === 2
                ? c % 3 === 0
                : (r + c) % 3 === 0;
        if (invert) grid[r][c] = !grid[r][c];
      }
    }
    // Level L = 01
    const fBits = computeFormatBits(0b01, mask);
    const getFBit = (i: number) => ((fBits >>> i) & 1) === 1;
    for (let i = 0; i <= 5; i++) grid[8][i] = getFBit(i);
    grid[8][7] = getFBit(6);
    grid[8][8] = getFBit(7);
    grid[7][8] = getFBit(8);
    for (let i = 9; i < 15; i++) grid[14 - i][8] = getFBit(i);

    for (let i = 0; i < 8; i++) grid[8][size - 1 - i] = getFBit(i);
    for (let i = 8; i < 15; i++) grid[size - 15 + i][8] = getFBit(i);
    grid[size - 8][8] = true;
    return grid;
  };

  const scorePenalty = (grid: boolean[][]): number => {
    let score = 0;
    for (let r = 0; r < size; r++) {
      let runLen = 1;
      for (let c = 1; c < size; c++) {
        if (grid[r][c] === grid[r][c - 1]) {
          runLen++;
          if (runLen === 5) score += 3;
          else if (runLen > 5) score += 1;
        } else {
          runLen = 1;
        }
      }
    }
    for (let c = 0; c < size; c++) {
      let runLen = 1;
      for (let r = 1; r < size; r++) {
        if (grid[r][c] === grid[r - 1][c]) {
          runLen++;
          if (runLen === 5) score += 3;
          else if (runLen > 5) score += 1;
        } else {
          runLen = 1;
        }
      }
    }
    return score;
  };

  let bestGrid = applyMaskAndFormat(0);
  let bestScore = scorePenalty(bestGrid);
  for (let m = 1; m < 4; m++) {
    const candidate = applyMaskAndFormat(m);
    const sc = scorePenalty(candidate);
    if (sc < bestScore) {
      bestScore = sc;
      bestGrid = candidate;
    }
  }
  return bestGrid;
}

/**
 * Renders a QR code matrix into compact Unicode half-blocks (`▀`, `▄`, `█`, ` `)
 * with a 2-module quiet zone so any phone camera can scan it in dark or light terminals.
 */
export function renderQrTerminal(url: string, indent = '  '): string {
  const matrix = encodeQrMatrix(url);
  const n = matrix.length;
  const quiet = 2;
  const total = n + quiet * 2;
  // Dark module = true, Light module = false.
  // In a dark terminal, foreground is light/white and background is dark/black.
  // To preserve standard QR polarity (dark modules inside a white quiet zone),
  // we render light modules (false) as filled blocks and dark modules (true) as empty space!
  const isLight = (r: number, c: number): boolean => {
    const mr = r - quiet;
    const mc = c - quiet;
    if (mr < 0 || mr >= n || mc < 0 || mc >= n) return true; // quiet zone is light
    return !matrix[mr][mc];
  };

  const lines: string[] = [];
  for (let r = 0; r < total; r += 2) {
    let rowStr = indent;
    for (let c = 0; c < total; c++) {
      const top = isLight(r, c);
      const bottom = r + 1 < total ? isLight(r + 1, c) : false;
      if (top && bottom) rowStr += '█';
      else if (top && !bottom) rowStr += '▀';
      else if (!top && bottom) rowStr += '▄';
      else rowStr += ' ';
    }
    lines.push(rowStr);
  }
  return lines.join('\n');
}

// ============================================================================
// Pair Device Server & Cloudflared Tunnel Manager
// ============================================================================

export interface PendingRemoteApproval {
  id: string;
  tool: string;
  preview: string;
  createdAt: string;
  resolve: (decision: 'yes' | 'no' | 'always') => void;
}

export interface PairSessionInfo {
  running: boolean;
  port: number;
  token: string;
  localUrl: string;
  lanUrl: string;
  tunnelUrl?: string;
  publicPairUrl: string;
  tunnelStatus: 'starting' | 'connected' | 'fallback_lan' | 'stopped';
  tunnelError?: string;
  connectedClients: number;
}

export interface PairLogEntry {
  id: number;
  seq: number;
  kind: 'user' | 'assistant' | 'tool' | 'system';
  text: string;
}

export interface PairCallbacks {
  getState: () => {
    model: string;
    provider: string;
    cwd: string;
    sessionId: string;
    mode: 'ask' | 'auto' | 'plan' | 'readonly';
    status: string;
    tokens: number;
    contextWindow: number;
  };
  getHistory?: () => Array<{ role: 'user' | 'assistant'; text: string }>;
  onRemotePrompt: (prompt: string) => void;
  onRemoteModeChange: (mode: 'ask' | 'auto' | 'plan' | 'readonly') => void;
  onRemoteStop: () => void;
  onNotice?: (message: string) => void;
}

function detectLanIp(): string {
  try {
    const nets = networkInterfaces();
    const candidates: Array<{ ip: string; score: number }> = [];
    for (const [name, list] of Object.entries(nets)) {
      if (!list) continue;
      const lower = name.toLowerCase();
      const isVirtual =
        lower.includes('tailscale') ||
        lower.includes('zerotier') ||
        lower.includes('wsl') ||
        lower.includes('vethernet') ||
        lower.includes('docker') ||
        lower.includes('vmware') ||
        lower.includes('virtualbox') ||
        lower.includes('loopback') ||
        lower.includes('tun') ||
        lower.includes('tap');
      const isPreferredAdapter =
        lower.includes('wi-fi') ||
        lower.includes('wifi') ||
        lower.includes('wlan') ||
        lower.includes('ethernet') ||
        lower.startsWith('en') ||
        lower.startsWith('eth');

      for (const addr of list) {
        if (addr.family !== 'IPv4' || addr.internal || addr.address.startsWith('169.254.')) continue;
        let score = 0;
        if (addr.address.startsWith('192.168.')) score += 50;
        else if (addr.address.startsWith('10.')) score += 40;
        else if (/^172\.(1[6-9]|2\d|3[0-1])\./.test(addr.address)) score += 35;
        else if (addr.address.startsWith('100.')) score -= 30; // CGNAT / Tailscale / Warp
        if (isPreferredAdapter) score += 25;
        if (isVirtual) score -= 40;
        candidates.push({ ip: addr.address, score });
      }
    }
    candidates.sort((a, b) => b.score - a.score);
    if (candidates.length > 0) return candidates[0].ip;
  } catch {
    // ignore
  }
  return '127.0.0.1';
}

function renderMobileHtml(defaultToken: string): string {
  return `<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8" />
<meta name="viewport" content="width=device-width, initial-scale=1, maximum-scale=1, user-scalable=no" />
<title>goon · Paired Device</title>
<style>
  :root {
    --bg: #0f111a;
    --panel: #181a24;
    --border: #2a2e3f;
    --brand: #a78bfa;
    --text: #e2e8f0;
    --muted: #94a3b8;
    --ok: #4ade80;
    --warn: #fbbf24;
    --err: #fb7185;
    --info: #7dd3fc;
  }
  * { box-sizing: border-box; margin: 0; padding: 0; }
  body {
    background: var(--bg);
    color: var(--text);
    font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, monospace;
    display: flex;
    flex-direction: column;
    height: 100dvh;
    overflow: hidden;
  }
  header {
    background: var(--panel);
    border-bottom: 1px solid var(--border);
    padding: 10px 14px;
    display: flex;
    justify-content: space-between;
    align-items: center;
    gap: 8px;
  }
  .brand { font-weight: 700; color: var(--brand); font-size: 15px; }
  .meta { font-size: 12px; color: var(--muted); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .modes {
    display: flex;
    gap: 6px;
    padding: 8px 14px;
    background: #13151f;
    border-bottom: 1px solid var(--border);
    overflow-x: auto;
  }
  .mode-btn {
    background: var(--panel);
    color: var(--muted);
    border: 1px solid var(--border);
    border-radius: 6px;
    padding: 5px 10px;
    font-size: 12px;
    cursor: pointer;
    font-family: inherit;
  }
  .mode-btn.active {
    border-color: var(--brand);
    color: var(--brand);
    font-weight: 600;
  }
  .stop-btn {
    margin-left: auto;
    border-color: var(--err);
    color: var(--err);
  }
  #log {
    flex: 1;
    overflow-y: auto;
    padding: 12px 14px;
    display: flex;
    flex-direction: column;
    gap: 8px;
    font-size: 13px;
    line-height: 1.45;
  }
  .entry {
    padding: 8px 10px;
    border-radius: 8px;
    background: var(--panel);
    border: 1px solid var(--border);
    white-space: pre-wrap;
    word-break: break-word;
  }
  .entry.user { border-left: 3px solid var(--info); background: #1e2235; }
  .entry.assistant { border-left: 3px solid var(--brand); }
  .entry.tool { font-size: 12px; color: var(--muted); padding: 5px 10px; }
  .entry.system { font-size: 12px; color: var(--muted); border-left: 3px solid var(--border); }
  .entry.approval {
    border: 1px solid var(--warn);
    background: #262015;
  }
  .approval-actions {
    display: flex;
    gap: 8px;
    margin-top: 8px;
  }
  .btn {
    border: none;
    border-radius: 6px;
    padding: 7px 12px;
    font-size: 13px;
    font-weight: 600;
    cursor: pointer;
  }
  .btn-ok { background: var(--ok); color: #052e16; }
  .btn-info { background: var(--info); color: #082f49; }
  .btn-err { background: var(--err); color: #4c0519; }
  form {
    display: flex;
    gap: 8px;
    padding: 10px 14px;
    background: var(--panel);
    border-top: 1px solid var(--border);
  }
  input {
    flex: 1;
    background: var(--bg);
    color: var(--text);
    border: 1px solid var(--border);
    border-radius: 8px;
    padding: 10px 12px;
    font-size: 14px;
    outline: none;
  }
  input:focus { border-color: var(--brand); }
  .send-btn {
    background: var(--brand);
    color: #0f111a;
    border: none;
    border-radius: 8px;
    padding: 0 16px;
    font-weight: 700;
    cursor: pointer;
  }
</style>
</head>
<body>
  <header>
    <div>
      <div class="brand">◆ goon remote</div>
      <div class="meta" id="meta">Connecting...</div>
    </div>
    <div class="meta" id="statusBadge">●</div>
  </header>
  <div class="modes">
    <button class="mode-btn" data-mode="ask">ask</button>
    <button class="mode-btn" data-mode="auto">auto</button>
    <button class="mode-btn" data-mode="plan">plan</button>
    <button class="mode-btn" data-mode="readonly">readonly</button>
    <button class="mode-btn stop-btn" id="stopBtn">Stop</button>
  </div>
  <div id="log"></div>
  <form id="composer">
    <input id="promptInput" type="text" placeholder="Send prompt or /command..." autocomplete="off" />
    <button class="send-btn" type="submit">Send</button>
  </form>
<script>
  const params = new URLSearchParams(location.search);
  const token = params.get('token') || params.get('t') || ${JSON.stringify(defaultToken)};
  const logEl = document.getElementById('log');
  const metaEl = document.getElementById('meta');
  const statusEl = document.getElementById('statusBadge');

  function upsertEntry(entry) {
    if (!entry || !entry.id) return;
    const domId = 'entry-' + entry.id;
    let el = document.getElementById(domId);
    const isNearBottom = logEl.scrollHeight - logEl.scrollTop - logEl.clientHeight < 140;
    if (!el) {
      el = document.createElement('div');
      el.id = domId;
      el.className = 'entry ' + (entry.kind || 'system');
      el.textContent = entry.text || '';
      logEl.appendChild(el);
      logEl.scrollTop = logEl.scrollHeight;
    } else if (el.textContent !== entry.text) {
      el.textContent = entry.text || '';
      if (isNearBottom) logEl.scrollTop = logEl.scrollHeight;
    }
  }

  function updateHeader(state) {
    if (!state) return;
    metaEl.textContent = state.model + ' · ' + state.cwd.split(/[\\\\/]/).pop();
    statusEl.textContent = '● ' + state.status + ' · ' + state.mode;
    document.querySelectorAll('.mode-btn[data-mode]').forEach(btn => {
      btn.classList.toggle('active', btn.dataset.mode === state.mode);
    });
  }

  function renderApprovalBox(msg) {
    if (document.getElementById('approval-' + msg.id)) return;
    const box = document.createElement('div');
    box.className = 'entry approval';
    box.id = 'approval-' + msg.id;
    const title = document.createElement('div');
    title.textContent = '▲ Allow ' + msg.tool + '?\\n' + (msg.preview || '');
    const actions = document.createElement('div');
    actions.className = 'approval-actions';
    [['Allow', 'yes', 'btn-ok'], ['Always', 'always', 'btn-info'], ['Deny', 'no', 'btn-err']].forEach(([label, dec, cls]) => {
      const b = document.createElement('button');
      b.className = 'btn ' + cls;
      b.textContent = label;
      b.onclick = async () => {
        await apiPost('/api/approve', { id: msg.id, decision: dec });
        box.textContent = '✓ Decision sent: ' + label;
      };
      actions.appendChild(b);
    });
    box.appendChild(title);
    box.appendChild(actions);
    logEl.appendChild(box);
    logEl.scrollTop = logEl.scrollHeight;
  }

  async function apiPost(path, body) {
    const res = await fetch(path + '?t=' + encodeURIComponent(token), {
      method: 'POST',
      headers: {
        'Content-Type': 'application/json',
        'Authorization': 'Bearer ' + token,
      },
      body: JSON.stringify(body || {}),
    });
    return res.json();
  }

  async function pollState() {
    try {
      const res = await fetch('/api/state?t=' + encodeURIComponent(token), {
        headers: { 'Authorization': 'Bearer ' + token },
        cache: 'no-store',
      });
      if (!res.ok) return;
      const data = await res.json();
      if (data.state) updateHeader(data.state);
      if (Array.isArray(data.entries)) {
        data.entries.forEach(upsertEntry);
      }
      if (Array.isArray(data.pendingApprovals)) {
        data.pendingApprovals.forEach(renderApprovalBox);
      }
    } catch {}
  }

  pollState();
  setInterval(pollState, 750);

  const es = new EventSource('/api/events?t=' + encodeURIComponent(token));
  es.onmessage = (ev) => {
    try {
      const msg = JSON.parse(ev.data);
      if (msg.type === 'state') {
        updateHeader(msg.state);
      } else if (msg.type === 'entry' && msg.entry) {
        upsertEntry(msg.entry);
      } else if (msg.type === 'approval') {
        renderApprovalBox(msg);
      } else if (msg.type === 'approval_resolved') {
        const box = document.getElementById('approval-' + msg.id);
        if (box) box.textContent = '✓ Approval resolved (' + msg.decision + ')';
      }
    } catch {}
  };

  document.querySelectorAll('.mode-btn[data-mode]').forEach(btn => {
    btn.onclick = async () => {
      await apiPost('/api/mode', { mode: btn.dataset.mode });
      pollState();
    };
  });
  document.getElementById('stopBtn').onclick = async () => {
    await apiPost('/api/stop', {});
    pollState();
  };
  document.getElementById('composer').onsubmit = async (e) => {
    e.preventDefault();
    const input = document.getElementById('promptInput');
    const text = input.value.trim();
    if (!text) return;
    input.value = '';
    await apiPost('/api/prompt', { prompt: text });
    pollState();
  };
</script>
</body>
</html>`;
}

export class PairManager {
  private server?: Server;
  private tunnelProc?: ChildProcess;
  private heartbeatTimer?: NodeJS.Timeout;
  private port = 0;
  private token = randomBytes(6).toString('base64url');
  private tunnelUrl?: string;
  private tunnelStatus: PairSessionInfo['tunnelStatus'] = 'stopped';
  private tunnelError?: string;
  private readonly clients = new Set<ServerResponse>();
  private readonly pendingApprovals = new Map<string, PendingRemoteApproval>();
  private readonly recentEvents: string[] = [];
  private readonly entries: PairLogEntry[] = [];
  private nextEntryId = 1;
  private seqCounter = 0;
  private activeAssistantEntryId?: number;
  private seededSessionId?: string;

  constructor(private readonly callbacks: PairCallbacks) {}

  get isRunning(): boolean {
    return Boolean(this.server);
  }

  getInfo(): PairSessionInfo {
    const lanIp = detectLanIp();
    const localUrl = `http://127.0.0.1:${this.port}/?t=${this.token}`;
    const lanUrl = `http://${lanIp}:${this.port}/?t=${this.token}`;
    const publicPairUrl = this.tunnelUrl ? `${this.tunnelUrl}/?t=${this.token}` : lanUrl;
    return {
      running: Boolean(this.server),
      port: this.port,
      token: this.token,
      localUrl,
      lanUrl,
      tunnelUrl: this.tunnelUrl,
      publicPairUrl,
      tunnelStatus: this.tunnelStatus,
      tunnelError: this.tunnelError,
      connectedClients: this.clients.size,
    };
  }

  async start(options?: { port?: number; skipTunnel?: boolean; tunnelTimeoutMs?: number }): Promise<PairSessionInfo> {
    if (this.server) return this.getInfo();
    this.token = randomBytes(6).toString('base64url');
    this.tunnelUrl = undefined;
    this.tunnelError = undefined;
    this.tunnelStatus = 'starting';

    const srv = createServer((req, res) => {
      void this.handleRequest(req, res);
    });

    await new Promise<void>((resolve, reject) => {
      srv.once('error', reject);
      srv.listen(options?.port ?? 0, '0.0.0.0', () => {
        srv.removeListener('error', reject);
        resolve();
      });
    });

    const addr = srv.address();
    this.port = typeof addr === 'object' && addr ? addr.port : (options?.port ?? 0);
    this.server = srv;

    this.heartbeatTimer = setInterval(() => {
      for (const res of this.clients) {
        try {
          res.write(': ping\n\n');
        } catch {
          this.clients.delete(res);
        }
      }
    }, 15_000);
    this.heartbeatTimer.unref?.();

    if (options?.skipTunnel) {
      this.tunnelStatus = 'fallback_lan';
      return this.getInfo();
    }

    await this.launchCloudflared(options?.tunnelTimeoutMs ?? 20_000);
    return this.getInfo();
  }

  private async waitForTunnelDns(tunnelUrl: string, maxWaitMs: number): Promise<void> {
    let hostname = '';
    try {
      hostname = new URL(tunnelUrl).hostname;
    } catch {
      return;
    }
    const deadline = Date.now() + maxWaitMs;
    while (Date.now() < deadline) {
      try {
        const dohRes = await fetch(
          `https://cloudflare-dns.com/dns-query?name=${encodeURIComponent(hostname)}&type=A`,
          {
            headers: { accept: 'application/dns-json' },
            signal: AbortSignal.timeout(2000),
          },
        );
        if (dohRes.ok) {
          const json = (await dohRes.json()) as { Status?: number; Answer?: unknown[] };
          if (json.Status === 0 && Array.isArray(json.Answer) && json.Answer.length > 0) {
            await new Promise(r => setTimeout(r, 600));
            return;
          }
        }
      } catch {
        // ignore and retry
      }
      await new Promise(r => setTimeout(r, 600));
    }
  }

  private async launchCloudflared(timeoutMs: number): Promise<void> {
    const targetLocal = `http://127.0.0.1:${this.port}`;
    return new Promise<void>(resolve => {
      let settled = false;
      let matchedUrl: string | undefined;
      let registered = false;
      let finalizing = false;

      const finish = () => {
        if (!settled) {
          settled = true;
          clearTimeout(timer);
          resolve();
        }
      };

      const finalizeTunnel = async (url: string) => {
        if (finalizing || settled) return;
        finalizing = true;
        const remaining = Math.max(2000, timeoutMs - 4000);
        await this.waitForTunnelDns(url, Math.min(8000, remaining));
        this.tunnelUrl = url;
        this.tunnelStatus = 'connected';
        this.callbacks.onNotice?.(`Paired tunnel ready: ${this.tunnelUrl}/?t=${this.token}`);
        finish();
      };

      const timer = setTimeout(() => {
        if (matchedUrl && !this.tunnelUrl) {
          this.tunnelUrl = matchedUrl;
          this.tunnelStatus = 'connected';
          this.callbacks.onNotice?.(`Paired tunnel ready: ${this.tunnelUrl}/?t=${this.token}`);
        } else if (!this.tunnelUrl) {
          this.tunnelStatus = 'fallback_lan';
          this.tunnelError ??= 'Timed out waiting for trycloudflare.com URL; using local/LAN URL.';
        }
        finish();
      }, timeoutMs);

      try {
        const proc = spawn('cloudflared', ['tunnel', '--no-autoupdate', '--url', targetLocal], {
          stdio: ['ignore', 'pipe', 'pipe'],
          windowsHide: true,
        });
        this.tunnelProc = proc;

        const onChunk = (chunk: Buffer) => {
          const text = chunk.toString('utf8');
          const match = /https:\/\/[a-z0-9-]+\.trycloudflare\.com/i.exec(text);
          if (match && !matchedUrl) {
            matchedUrl = match[0];
          }
          if (text.includes('Registered tunnel connection')) {
            registered = true;
          }
          if (matchedUrl && registered && !finalizing) {
            void finalizeTunnel(matchedUrl);
          }
        };

        proc.stdout?.on('data', onChunk);
        proc.stderr?.on('data', onChunk);
        proc.on('error', err => {
          this.tunnelStatus = 'fallback_lan';
          this.tunnelError = `cloudflared not found or failed (${err.message}). Using LAN URL.`;
          finish();
        });
        proc.on('exit', code => {
          if (!this.tunnelUrl) {
            this.tunnelStatus = 'fallback_lan';
            this.tunnelError ??= `cloudflared exited with code ${code ?? 'unknown'}.`;
            finish();
          }
        });
      } catch (err) {
        this.tunnelStatus = 'fallback_lan';
        this.tunnelError = err instanceof Error ? err.message : String(err);
        finish();
      }
    });
  }

  async stop(): Promise<void> {
    if (this.heartbeatTimer) {
      clearInterval(this.heartbeatTimer);
      this.heartbeatTimer = undefined;
    }
    for (const pending of this.pendingApprovals.values()) {
      pending.resolve('no');
    }
    this.pendingApprovals.clear();
    for (const client of this.clients) {
      try {
        client.end();
      } catch {
        // ignore
      }
    }
    this.clients.clear();
    if (this.tunnelProc) {
      try {
        this.tunnelProc.kill();
      } catch {
        // ignore
      }
      this.tunnelProc = undefined;
    }
    if (this.server) {
      const srv = this.server;
      this.server = undefined;
      await new Promise<void>(resolve => srv.close(() => resolve()));
    }
    this.tunnelStatus = 'stopped';
    this.tunnelUrl = undefined;
  }

  private ensureHistorySeeded(): void {
    const curSessionId = this.callbacks.getState().sessionId;
    if (this.seededSessionId !== curSessionId) {
      this.seededSessionId = curSessionId;
      this.entries.length = 0;
      this.activeAssistantEntryId = undefined;
      if (this.callbacks.getHistory) {
        for (const item of this.callbacks.getHistory()) {
          const clean = item.text.trim();
          if (!clean) continue;
          this.appendEntry(item.role, item.role === 'user' ? `› ${clean}` : clean, false);
        }
      }
    }
  }

  private appendEntry(kind: PairLogEntry['kind'], text: string, broadcastNow = true): PairLogEntry {
    if (kind !== 'assistant') {
      this.activeAssistantEntryId = undefined;
    }
    const entry: PairLogEntry = {
      id: this.nextEntryId++,
      seq: ++this.seqCounter,
      kind,
      text,
    };
    this.entries.push(entry);
    if (this.entries.length > 150) this.entries.shift();
    if (broadcastNow) {
      this.broadcast({ type: 'entry', entry }, false);
    }
    return entry;
  }

  broadcast(payload: Record<string, unknown>, replayable = true): void {
    const data = `data: ${JSON.stringify(payload)}\n\n`;
    if (replayable) {
      this.recentEvents.push(data);
      if (this.recentEvents.length > 60) this.recentEvents.shift();
    }
    for (const res of this.clients) {
      try {
        res.write(data);
      } catch {
        this.clients.delete(res);
      }
    }
  }

  notifyUserPrompt(prompt: string): void {
    this.ensureHistorySeeded();
    const formatted = `› ${prompt}`;
    const last = this.entries[this.entries.length - 1];
    if (!last || last.kind !== 'user' || last.text !== formatted) {
      this.appendEntry('user', formatted);
    }
    this.activeAssistantEntryId = undefined;
    this.syncState();
  }

  notifySystem(text: string): void {
    const clean = text.trim();
    if (!clean) return;
    if (
      clean.includes('Pair Device via Cloudflared Tunnel') ||
      clean.includes('╭─── goon') ||
      clean.includes('╭─── Resumed Session') ||
      clean.startsWith('↻ Resumed session') ||
      clean.startsWith('Starting Cloudflared tunnel') ||
      clean.startsWith('[answer]\n') ||
      clean.startsWith('[reasoning]\n') ||
      clean.startsWith('> ')
    ) {
      return;
    }
    this.ensureHistorySeeded();
    this.activeAssistantEntryId = undefined;
    this.appendEntry('system', clean);
  }

  notifyEvent(event: HarnessEvent | SubagentEvent): void {
    this.ensureHistorySeeded();
    if (event.type === 'text') {
      let entry = this.activeAssistantEntryId
        ? this.entries.find(e => e.id === this.activeAssistantEntryId)
        : undefined;
      if (!entry) {
        entry = this.appendEntry('assistant', event.text);
        this.activeAssistantEntryId = entry.id;
      } else {
        entry.text += event.text;
        entry.seq = ++this.seqCounter;
        this.broadcast({ type: 'entry', entry }, false);
      }
    } else if (event.type === 'tool' && event.state === 'start') {
      this.activeAssistantEntryId = undefined;
      this.appendEntry('tool', `● [${event.name}] ${event.meta?.summary || event.state}`);
    } else if (event.type === 'subagent' && event.state !== 'progress') {
      this.activeAssistantEntryId = undefined;
      this.appendEntry('tool', `● [subagent:${event.id.slice(0, 8)}] ${event.state}`);
    } else if (event.type === 'usage' || event.type === 'stopped' || event.type === 'plan_exit') {
      if (event.type === 'stopped') {
        this.activeAssistantEntryId = undefined;
      }
      this.syncState();
    }
  }

  syncState(): void {
    if (!this.server) return;
    this.broadcast({ type: 'state', state: this.callbacks.getState() }, false);
  }

  /**
   * Registers a pending tool approval so the paired mobile/web UI can approve or deny it.
   * Returns a cleanup function and a promise that resolves if the remote user responds first.
   */
  requestRemoteApproval(
    tool: string,
    preview: string,
    signal: AbortSignal,
  ): { id: string; promise: Promise<'yes' | 'no' | 'always'>; cancel: (resolvedBy?: string) => void } {
    const id = randomBytes(6).toString('hex');
    let cleanup = () => {};
    const promise = new Promise<'yes' | 'no' | 'always'>((resolve, reject) => {
      const entry: PendingRemoteApproval = {
        id,
        tool,
        preview: preview.slice(0, 2000),
        createdAt: new Date().toISOString(),
        resolve: decision => {
          this.pendingApprovals.delete(id);
          signal.removeEventListener('abort', onAbort);
          this.broadcast({ type: 'approval_resolved', id, decision }, false);
          resolve(decision);
        },
      };
      const onAbort = () => {
        this.pendingApprovals.delete(id);
        this.broadcast({ type: 'approval_resolved', id, decision: 'cancelled' }, false);
        reject(signal.reason ?? new Error('Cancelled'));
      };
      cleanup = (resolvedBy = 'terminal') => {
        if (this.pendingApprovals.delete(id)) {
          signal.removeEventListener('abort', onAbort);
          this.broadcast({ type: 'approval_resolved', id, decision: resolvedBy }, false);
        }
      };
      this.pendingApprovals.set(id, entry);
      signal.addEventListener('abort', onAbort, { once: true });
      this.broadcast({ type: 'approval', id, tool, preview: entry.preview }, false);
    });
    return { id, promise, cancel: cleanup };
  }

  formatBanner(): string {
    const info = this.getInfo();
    const qrTarget = info.tunnelUrl ?? info.publicPairUrl;
    const qr = renderQrTerminal(qrTarget, '  ');
    const lines: string[] = [
      `╭─── Pair Device via Cloudflared Tunnel ──────────────────────────╮`,
      `  Status      : ${info.tunnelStatus === 'connected' ? 'Cloudflared Tunnel Active (Free)' : 'Local / LAN Fallback'}`,
      ...(info.tunnelUrl ? [`  Tunnel URL  : ${info.publicPairUrl}`] : []),
      `  LAN URL     : ${info.lanUrl}`,
      `  Local URL   : ${info.localUrl}`,
      ...(info.tunnelError ? [`  Note        : ${info.tunnelError}`] : []),
      `  Clients     : ${info.connectedClients} connected`,
      ``,
      `  Scan QR code with your phone camera to control this session:`,
      qr,
      `╰─────────────────────────────────────────────────────────────────╯`,
    ];
    return lines.join('\n');
  }

  private checkAuth(reqUrl: URL, req: IncomingMessage): boolean {
    const qToken = reqUrl.searchParams.get('token') ?? reqUrl.searchParams.get('t');
    if (qToken === this.token) return true;
    const auth = req.headers.authorization;
    if (auth === `Bearer ${this.token}`) return true;
    return false;
  }

  private async readJsonBody(req: IncomingMessage): Promise<Record<string, unknown>> {
    const chunks: Buffer[] = [];
    let total = 0;
    for await (const chunk of req) {
      const buf = Buffer.from(chunk);
      total += buf.length;
      if (total > 256 * 1024) throw new Error('Payload too large');
      chunks.push(buf);
    }
    const raw = Buffer.concat(chunks).toString('utf8').trim();
    return raw ? (JSON.parse(raw) as Record<string, unknown>) : {};
  }

  private async handleRequest(req: IncomingMessage, res: ServerResponse): Promise<void> {
    try {
      const reqUrl = new URL(req.url ?? '/', `http://${req.headers.host ?? '127.0.0.1'}`);

      if (req.method === 'GET' && reqUrl.pathname === '/favicon.ico') {
        res.writeHead(204);
        res.end();
        return;
      }

      if (req.method === 'GET' && reqUrl.pathname === '/') {
        res.writeHead(200, {
          'Content-Type': 'text/html; charset=utf-8',
          'Cache-Control': 'no-store, no-cache, must-revalidate',
        });
        res.end(renderMobileHtml(this.token));
        return;
      }

      if (!this.checkAuth(reqUrl, req)) {
        res.writeHead(401, { 'Content-Type': 'application/json; charset=utf-8' });
        res.end(JSON.stringify({ error: 'Unauthorized: invalid or missing pairing token.' }));
        return;
      }

      if (req.method === 'GET' && reqUrl.pathname === '/api/state') {
        this.ensureHistorySeeded();
        res.writeHead(200, {
          'Content-Type': 'application/json; charset=utf-8',
          'Cache-Control': 'no-store',
        });
        res.end(
          JSON.stringify({
            state: this.callbacks.getState(),
            entries: this.entries,
            pendingApprovals: [...this.pendingApprovals.values()].map(p => ({
              id: p.id,
              tool: p.tool,
              preview: p.preview,
              createdAt: p.createdAt,
            })),
          }),
        );
        return;
      }

      if (req.method === 'GET' && reqUrl.pathname === '/api/events') {
        this.ensureHistorySeeded();
        res.writeHead(200, {
          'Content-Type': 'text/event-stream; charset=utf-8',
          'Cache-Control': 'no-cache, no-store, no-transform',
          Connection: 'keep-alive',
          'X-Accel-Buffering': 'no',
        });
        res.flushHeaders();
        this.clients.add(res);
        // Send 2KB comment padding first so Cloudflare / mobile proxies flush the SSE stream immediately
        res.write(`: ${' '.repeat(2048)}\n\n`);
        res.write(`data: ${JSON.stringify({ type: 'state', state: this.callbacks.getState() })}\n\n`);
        for (const entry of this.entries) {
          res.write(`data: ${JSON.stringify({ type: 'entry', entry })}\n\n`);
        }
        for (const pending of this.pendingApprovals.values()) {
          res.write(
            `data: ${JSON.stringify({ type: 'approval', id: pending.id, tool: pending.tool, preview: pending.preview })}\n\n`,
          );
        }
        req.on('close', () => {
          this.clients.delete(res);
        });
        return;
      }

      if (req.method === 'POST' && reqUrl.pathname === '/api/prompt') {
        const body = await this.readJsonBody(req);
        const prompt = typeof body.prompt === 'string' ? body.prompt.trim() : '';
        if (!prompt) {
          res.writeHead(400, { 'Content-Type': 'application/json' });
          res.end(JSON.stringify({ error: 'Prompt is required' }));
          return;
        }
        this.notifyUserPrompt(prompt);
        this.callbacks.onRemotePrompt(prompt);
        res.writeHead(200, { 'Content-Type': 'application/json' });
        res.end(JSON.stringify({ ok: true }));
        return;
      }

      if (req.method === 'POST' && reqUrl.pathname === '/api/mode') {
        const body = await this.readJsonBody(req);
        const m = body.mode;
        if (m === 'ask' || m === 'auto' || m === 'plan' || m === 'readonly') {
          this.callbacks.onRemoteModeChange(m);
          this.syncState();
          res.writeHead(200, { 'Content-Type': 'application/json' });
          res.end(JSON.stringify({ ok: true, mode: m }));
          return;
        }
        res.writeHead(400, { 'Content-Type': 'application/json' });
        res.end(JSON.stringify({ error: 'Invalid mode' }));
        return;
      }

      if (req.method === 'POST' && reqUrl.pathname === '/api/stop') {
        this.callbacks.onRemoteStop();
        this.syncState();
        res.writeHead(200, { 'Content-Type': 'application/json' });
        res.end(JSON.stringify({ ok: true }));
        return;
      }

      if (req.method === 'POST' && reqUrl.pathname === '/api/approve') {
        const body = await this.readJsonBody(req);
        const id = typeof body.id === 'string' ? body.id : '';
        const decision = body.decision;
        const pending = this.pendingApprovals.get(id);
        if (!pending || (decision !== 'yes' && decision !== 'no' && decision !== 'always')) {
          res.writeHead(404, { 'Content-Type': 'application/json' });
          res.end(JSON.stringify({ error: 'Approval request not found or already resolved' }));
          return;
        }
        pending.resolve(decision);
        res.writeHead(200, { 'Content-Type': 'application/json' });
        res.end(JSON.stringify({ ok: true, decision }));
        return;
      }

      res.writeHead(404, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify({ error: 'Not found' }));
    } catch (err) {
      res.writeHead(500, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify({ error: err instanceof Error ? err.message : String(err) }));
    }
  }
}
