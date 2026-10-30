# Đóng góp cho goon-cli

Tài liệu này dành cho người muốn clone, chạy, sửa và phát hành goon-cli. Cách **dùng** CLI nằm trong
[README](README.md); hợp đồng thiết kế và các bất biến bảo mật nằm trong [docs/SPEC.md](docs/SPEC.md).

goon là một crate Rust (`src/`, binary `goon`, thư viện `goon`). Node chỉ còn dùng cho lớp vỏ npm
(`bin/goon.mjs`, `scripts/install.mjs`), các script phát hành và bộ test end-to-end.

## 1. Yêu cầu

| Công cụ | Phiên bản | Ghi chú |
|---|---|---|
| Rust | bản ghi trong `rust-toolchain.toml` | có rustup thì nó tự cài đúng bản (kèm clippy, rustfmt) ở lần `cargo` đầu tiên. Termux: `pkg install rust` (không qua rustup, dùng bản stable của Termux) |
| Node.js | **>= 22.16.0** (`.nvmrc`; CI chạy 22.16.0 và 24) | `nvm use`, `fnm use` hoặc `mise use` đều đọc `.nvmrc` |
| npm | đi kèm Node | dev dependency được ghim + `npm-shrinkwrap.json`, luôn cài bằng `npm ci` |
| C compiler | bất kỳ | `ring` (TLS) biên dịch một ít C: gcc/clang trên Linux/macOS/Termux, MSVC Build Tools trên Windows |
| git, bash | bất kỳ | bash cần cho tool `bash` của agent (Windows: Git Bash) |
| ripgrep | tuỳ chọn | thiếu thì CLI dùng search dự phòng |
| python3 | chỉ POSIX | các test PTY dùng `tests/fixtures/*.py`; Termux: `pkg install python`; Windows tự skip |

Không cần API key để phát triển và chạy test.

## 2. Bắt đầu nhanh

```bash
git clone https://github.com/binxgtl/goon-cli.git
cd goon-cli
nvm use                                          # hoặc: fnm use / mise use
npm ci --ignore-scripts --no-audit --no-fund     # đúng như CI (MCP SDK cho fixture test, oxlint)
npm run check                                    # rustfmt + clippy + oxlint + cargo test + test Node + e2e
npm run dev -- --stub -p "xin chào"              # build rồi chạy CLI từ source, không cần mạng/key
```

`cargo run -- --stub -p "xin chào"` cũng được: đó là cùng một binary.

**Binary nào đang chạy?** `bin/goon.mjs` (và mọi test Node) chọn theo thứ tự: `GOON_BINARY` nếu có,
`vendor/<target>/goon` (bản cài từ npm), rồi bản build mới nhất trong `target/release` hoặc
`target/debug` của repo. Sửa `src/` mà quên `cargo build` thì test Node đang chạy binary cũ; các
script npm bên dưới đã tự build. Muốn chắc chắn, đặt `GOON_BINARY=$PWD/target/debug/goon`.

## 3. Script npm

| Lệnh | Việc làm |
|---|---|
| `npm run check` | `lint` + `cargo test` + toàn bộ test Node và e2e. Chạy trước khi push |
| `npm run lint` | `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `oxlint tests bin scripts` |
| `npm test` | `cargo build` rồi chạy `tests/*.test.mjs` |
| `npm run test:rust` | `cargo test --locked` |
| `npm run test:e2e` | `cargo build` rồi chạy `tests/e2e/*.test.mjs` |
| `npm run test:one -- <file…>` | `cargo build` rồi chạy riêng file/cờ bạn truyền cho `node --test` (mục 5) |
| `npm run build` / `build:release` | `cargo build` / `cargo build --release` |
| `npm run dev -- <cờ goon>` | `cargo build` rồi chạy `node bin/goon.mjs <cờ goon>` |
| `npm run release:pack` | thử đóng gói release cục bộ (mục 12) |

## 4. Vòng lặp phát triển nhanh

```bash
cargo check                                      # nhanh nhất để bắt lỗi biên dịch
cargo test --test unit risk::                    # chỉ nhóm test risk
cargo run -- --stub -p "xin chào"                # chạy thử
```

`cargo watch -x check` (cài `cargo-watch` riêng) hoặc rust-analyzer trong editor cho phản hồi ngay khi lưu.

## 5. Chạy test

Có hai tầng test:

- **Rust** (`cargo test`):
  - test đơn vị ngay trong module (`#[cfg(test)] mod tests` trong `src/**`; TUI ở `src/tui/tests.rs`);
  - `tests/unit/`: test thư viện, port từ bộ `node:test` của bản TypeScript, gom trong **một** binary
    test để link nhanh. `tests/unit/common.rs` có `fixture()` (thư mục tạm), `env()`/`fixture_env()`
    (đổi biến môi trường, có khoá toàn cục), `provider()` + `sse()` (server HTTP giả kiểu
    OpenAI-compatible), `FakeModel` (model theo kịch bản), `runtime()`/`store()`/`registry()`;
  - `tests/parity.rs`: phát lại `tests/parity/corpus.jsonl` (khoảng 10 nghìn lời gọi đã ghi từ bản
    TypeScript) và so từng kết quả; khác biệt có chủ đích (sửa lỗi cũ) liệt kê trong `DIVERGENCES`.
    Xem `tests/parity/README.md`.
- **Node** (`node --test`): chạy CLI thật như người dùng.
  - `tests/*.test.mjs`: tích hợp qua CLI. `tests/helpers.mjs` có `fixture`, `provider` + `sse`,
    `cli`/`stubCli` (spawn `bin/goon.mjs`, đổi được bằng `GOON_TEST_BIN`).
  - `tests/e2e/*.test.mjs`: chỉ dùng stub provider, không mạng, chỉ assert trên trường ổn định.

```bash
cargo test --locked                                               # toàn bộ test Rust
cargo test --test unit -- memory_integration:: harness_verify::  # vài nhóm
cargo test --lib tui::                                            # test TUI
PARITY_ONLY=classifyBashRisk cargo test --test parity            # một phần corpus parity
npm run test:one -- tests/runtime.test.mjs                        # một file Node
npm run test:one -- --test-name-pattern="resume" tests/audit.test.mjs
GOON_UPDATE_GOLDEN=1 node --test tests/golden.test.mjs            # ghi lại tests/golden/*.jsonl, rồi xem git diff
(umask 027; node --test --test-concurrency=4 "tests/*.test.mjs")  # POSIX: CI chạy thêm umask 027 và 007
```

- Test Rust trong `tests/unit/` cần `node` và `node_modules` (fixture MCP dùng MCP SDK): chạy `npm ci` trước.
- Luôn đặt glob trong dấu ngoặc kép để Node tự mở rộng (cmd.exe/PowerShell không mở rộng glob).
- `--test-concurrency=4` là có chủ ý (Windows cạn bộ nhớ nếu chạy không giới hạn, xem README).
- Test đổi biến môi trường phải đi qua `env()` trong `common.rs`; nó khoá toàn cục và khôi phục khi drop.

## 6. Chạy CLI không cần API key (stub provider)

`--stub` dùng model giả tất định (`src/providers/stub.rs`): không mạng, không đọc key. Cú pháp
fixture nằm trong README (mục "Event log, replay & stub provider"); khoá lạ trong fixture bị từ chối.

```bash
# Câu trả lời mặc định
cargo run -- --stub -p "demo"                            # -> [stub] deterministic reply

# Kịch bản có tool call. Prompt PHẢI chứa chuỗi `match` của fixture ("read the file"),
# và thư mục làm việc phải có notes.txt; "demo" sẽ chỉ nhận về "[stub] auxiliary response".
mkdir -p /tmp/goon-demo && echo "xin chào" > /tmp/goon-demo/notes.txt
cargo run -- --stub --stub-fixture tests/fixtures/stub-tool-call.json \
  --cwd /tmp/goon-demo -p "please read the file"
cargo run -- --stub --stub-fixture tests/fixtures/stub-tool-call.json \
  --cwd /tmp/goon-demo --json -p "please read the file"  # NDJSON event, giống test e2e quan sát

# TUI toàn màn hình với stub (không có -p)
cargo run -- --stub --cwd /tmp/goon-demo
```

Cách stub chọn turn: `match` là chuỗi con của tin nhắn **user** cuối cùng (kết quả tool đi qua message
role `tool` nên không khớp được); turn chưa dùng đầu tiên khớp sẽ được chọn; nếu chỉ còn đúng một turn thì
nó được dùng dù không khớp; còn lại rơi vào `auxiliary`; hết kịch bản trả `[stub] script exhausted`.
Ví dụ fixture sẵn có: `tests/fixtures/*.json` (text, tool call, usage, error).

**Cô lập dữ liệu khi chạy tay.** Mặc định CLI ghi session vào `~/.local/share/goon` và đọc config/key
trong `~/.config/goon`. Để không đụng bản goon thật của bạn:

```bash
export XDG_CONFIG_HOME="$(mktemp -d)" XDG_DATA_HOME="$(mktemp -d)"
```

## 7. Chạy với model cục bộ hoặc thật

Mọi endpoint OpenAI-compatible có tool calling đều dùng được. CLI luôn đòi một key không rỗng; server cục bộ
thường bỏ qua nó, nên đặt giá trị bất kỳ. `http://` tới loopback không bị cảnh báo.

```bash
export GOON_API_KEY=dummy
cargo run -- --provider openai-compatible --base-url http://127.0.0.1:11434/v1 --model <MODEL_ID> -p "..."
#   Ollama: :11434/v1    llama.cpp llama-server: :8080/v1    vLLM: :8000/v1
```

Cần server giả có kịch bản qua HTTP (kiểm tra đúng payload gửi đi)? Xem `provider()` và `sse()` trong
`tests/unit/common.rs` (Rust) hoặc `tests/helpers.mjs` (Node).

## 8. Debug

- `RUST_BACKTRACE=1 cargo run -- …` khi gặp panic.
- Debugger: build debug (`cargo build`) rồi `rust-gdb`/`rust-lldb target/debug/goon -- --stub -p hi`,
  hoặc cấu hình CodeLLDB trong VS Code trỏ tới `target/debug/goon`.
- `goon doctor`: cấu hình hiệu lực, đường dẫn, thư mục dữ liệu, khả năng tới endpoint. Trong TUI: `/status`, `/debug-config`.
- `--json` in event NDJSON; `--event-log` ghi `sessions/<id>.events.jsonl`, đọc lại bằng `goon replay <ID>`
  (`--envelopes`, `--rebuild-session`). Replay không cần model/key.
- `--no-tui` dùng chế độ dòng lệnh thuần, dễ quan sát khi nghi lỗi nằm ở TUI.

## 9. Kiến trúc: đọc code từ đâu

Luồng chính: `src/main.rs` → `src/cli/` (cờ, lệnh con, vòng đời, lệnh slash) → `config.rs`
`resolve_runtime` (ưu tiên: cờ > biến `GOON_*` > config > mặc định) → `ToolRegistry` (`tools.rs`) +
`Harness` (`harness.rs`, vòng lặp agent: stream model → tool call → approval → thực thi → checkpoint ở
`session.rs`). Model do `providers/mod.rs` tạo (`--stub` rẽ sang `providers/stub.rs` tại đây). `tui/`
chỉ lo hiển thị/nhập liệu.

- **Bảng "file → trách nhiệm"** đầy đủ nằm ở cuối mục "Phát triển & Kiểm thử" trong [README](README.md).
- Mỗi module phức tạp mở đầu bằng comment nêu bất biến của nó (xem đầu `coord.rs`, `context_budget.rs`, `memory.rs`).
- `jsutil.rs` tái tạo đúng hành vi JavaScript ở những chỗ người dùng nhìn thấy hoặc được ghi xuống đĩa
  (định dạng số, `JSON.stringify`, `toFixed`, thông báo lỗi `JSON.parse` của V8). Đừng thay bằng
  định dạng mặc định của Rust: file config/session/event log phải giữ nguyên byte như bản TypeScript.
- Tài liệu trong `docs/`: `SPEC.md` (hợp đồng thiết kế), `MCP.md` (cấu hình MCP), `AUDIT.md` (audit bảo mật
  kèm test hồi quy). `PLAN-yolo-port.md`, `IMPLEMENTATION-REPORT.md`, `VALIDATION.md`, `REFERENCE_REVIEW.md`
  là báo cáo/kế hoạch **tại thời điểm viết** (thời bản TypeScript); tên file và số liệu trong đó có thể đã cũ.

## 10. Quy ước code

- Rust 2021, `cargo fmt` theo `rustfmt.toml` (dòng tối đa 200 cột), clippy không cảnh báo
  (`-D warnings`). Regex dùng nhiều lần thì dùng macro `re!` (biên dịch một lần mỗi chỗ gọi).
- JavaScript (lớp vỏ npm, script, test Node): ESM, thụt lề 2 dấu cách, lint bằng `oxlint`.
- LF và newline cuối file (`.editorconfig`). Mỗi thay đổi hành vi cần test đi kèm.
- Không thêm dependency mới nếu không thật sự cần, nhất là dependency kéo C/C++ hoặc thư viện hệ thống:
  binary phải build được cho Android, musl, macOS và Windows. Dependency Node chỉ còn là dev dependency;
  đổi bản bằng `npm install <gói>@<x.y.z> --save-dev --save-exact` để `package.json` và
  `npm-shrinkwrap.json` cùng cập nhật.
- Chạy được trên Termux, Linux, macOS và Windows: không giả định `/tmp`, dấu `/` hay mode bit POSIX trong test
  (tạo thư mục tạm qua `fixture()`, và `#[cfg(unix)]`/`skip` rõ ràng cho phần chỉ có ở POSIX).
- Giữ các bất biến bảo mật trong `docs/SPEC.md` và `docs/PLAN-yolo-port.md` §0: key không vào môi trường tiến
  trình con, mọi thứ ghi ra log/session/memory đều qua `redact.rs`, event và config chỉ được thêm
  trường tuỳ chọn (config cũ vẫn phải parse được nhờ giá trị mặc định).

## 11. Commit, pull request, CHANGELOG

- Nhánh từ `main`; PR nhắm vào `main`. PR chạy job `check` và build đủ 8 target (không phát hành).
- **Merge vào `main` là phát hành** (mục 12), nên chỉ merge khi CI xanh.
- Commit: tiền tố kiểu Conventional Commits, tiếng Anh, thể mệnh lệnh: `feat:`, `fix:`, `fix(security):`,
  `test:`, `docs:`, `chore:`, `ci:`. `[skip ci]` bỏ qua cả CI lẫn phát hành; chỉ dùng có chủ ý (bot dùng nó cho
  commit release).
- Đừng sửa `version` bằng tay trừ khi muốn tăng minor/major (mục 12).
- Ghi thay đổi người dùng thấy được vào `CHANGELOG.md`, mục `## Unreleased` (pipeline không tự sửa file này).
- Cập nhật README khi thêm lệnh, cờ, biến môi trường hoặc event mới.
- Template PR nhắc đúng các bước trên.

## 12. Phát hành

Không có bước phát hành thủ công. Mỗi lần push lên `main` (kể cả merge PR), workflow `Check and release`
(`.github/workflows/release.yml`) chạy:

1. **plan**: chọn phiên bản (`scripts/release-version.mjs`). Nếu `version` trong `package.json` cao hơn mọi
   tag `v*` thì dùng nguyên; ngược lại tăng patch của tag cao nhất. Lần chạy lại sau khi bot đã đẩy commit
   release sẽ tái dùng đúng số đó.
2. **check**: rustfmt, clippy, `cargo test` và test Node trên Ubuntu (Node 22.16 và 24), Windows, macOS.
3. **build**: 8 target, binary được đóng dấu phiên bản qua biến `GOON_VERSION`: Android `aarch64`/`armv7`/`x86_64`
   (Android NDK, API 24), Linux musl `x86_64`/`aarch64`, macOS `x86_64`/`aarch64`, Windows `x86_64` (CRT
   tĩnh). Target chạy được trên runner thì được chạy thử `--version`. Mỗi binary được nén thành
   `goon-<target>.gz`.
4. **release** (chỉ trên `binxgtl/goon-cli`, không chạy cho PR): `scripts/third-party-notices.mjs` ghi
   `THIRD-PARTY-NOTICES.txt` (license của mọi crate được link vào binary, lấy từ `cargo metadata`), rồi
   `scripts/publish-release.mjs` đồng bộ
   phiên bản vào `package.json`, `npm-shrinkwrap.json`, `Cargo.toml` và `Cargo.lock`, ghi
   `vendor/manifest.json` (SHA-256 của từng binary), `npm pack`, cài thử `.tgz` vào prefix tạm để lớp vỏ tự
   tải + kiểm checksum + chạy `--version`/`--help`, rồi đẩy commit `chore: release vX.Y.Z [skip ci]` + tag và
   tạo GitHub Release với `goon-cli-X.Y.Z.tgz`, bản alias `goon-cli.tgz` (địa chỉ cài trong README), các
   `goon-<target>.gz`, `THIRD-PARTY-NOTICES.txt` và `SHA256SUMS.txt`.

Việc thủ công duy nhất là **tăng minor/major**: trong một PR, sửa `version` ở cả `package.json`,
`npm-shrinkwrap.json` (`npm version minor --no-git-tag-version` lo hai file này), `Cargo.toml` và mục
`goon-cli` trong `Cargo.lock` (chạy `cargo check` sau khi sửa `Cargo.toml`). Bốn nơi phải khớp; test
`tests/release.test.mjs` kiểm tra điều đó. Lần push kế tiếp lên `main` sẽ phát hành đúng số đó.

Thử đóng gói trên máy (không đẩy gì lên GitHub): đặt các `goon-<target>.gz` vào `release-bin/` (đủ 8 target),
chạy `node scripts/third-party-notices.mjs` rồi `npm run release:pack`; kết quả nằm trong `release-out/` (đã ignore). Đừng chạy
`scripts/publish-release.mjs` cục bộ: nó tạo tag, push và phát hành thật.

## 13. Ghi chú cho Termux

- Dùng: `pkg install nodejs-lts git bash ripgrep` rồi cài từ README; binary `aarch64-linux-android` được tải sẵn.
- Phát triển trên máy: thêm `pkg install rust python`; làm việc trong `$HOME` (không dùng `/sdcard`) để có
  đủ quyền file và executable bit. Build release lần đầu mất vài phút trên điện thoại.
- `bash scripts/smoke-termux.sh --build`: `cargo build --release` rồi chạy các test stub-provider, golden và e2e
  trên thiết bị. Không có `--build` thì dùng binary sẵn có (`GOON_BINARY` hoặc `target/`). Script không thuộc CI.
- Trên máy yếu, ưu tiên `cargo test --test unit <nhóm>::` và `npm run test:one -- <file>` thay vì cả bộ.
