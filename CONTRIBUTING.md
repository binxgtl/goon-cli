# Đóng góp cho goon-cli

Tài liệu này dành cho người muốn clone, chạy, sửa và phát hành goon-cli. Cách **dùng** CLI nằm trong
[README](README.md); hợp đồng thiết kế và các bất biến bảo mật nằm trong [docs/SPEC.md](docs/SPEC.md).

## 1. Yêu cầu

| Công cụ | Phiên bản | Ghi chú |
|---|---|---|
| Node.js | **>= 22.16.0** (`.nvmrc` ghi đúng bản tối thiểu này; CI chạy 22.16.0 và 24) | `nvm use`, `fnm use` hoặc `mise use` đều đọc `.nvmrc` |
| npm | đi kèm Node | dependency được ghim chính xác + `npm-shrinkwrap.json`, luôn cài bằng `npm ci` |
| git, bash | bất kỳ | bash cần cho tool `bash` của agent (Windows: Git Bash) |
| ripgrep | tuỳ chọn | thiếu thì CLI dùng search JavaScript dự phòng |
| python3 | chỉ POSIX | 2 test PTY dùng `tests/fixtures/*.py`; Termux: `pkg install python`; Windows tự skip |

Không cần compiler, addon native hay API key để phát triển và chạy test.

## 2. Bắt đầu nhanh

```bash
git clone https://github.com/binxgtl/goon-cli.git
cd goon-cli
nvm use                                          # hoặc: fnm use / mise use
npm ci --ignore-scripts --no-audit --no-fund     # đúng như CI
npm run check                                    # lint + build + toàn bộ test (~1 phút)
npm run dev -- --stub -p "xin chào"              # chạy CLI từ source, không cần mạng/key
```

**Điều quan trọng nhất:** `dist/` không được commit, nhưng `bin/goon.mjs` và **mọi test** đều chạy trên
`dist/`. Sửa `src/*.ts` mà không build lại thì bạn đang test mã cũ. Các script bên dưới đã tự build;
nếu gọi `node` trực tiếp, hãy bật `npm run watch` (mục 4). Lỗi `ERR_MODULE_NOT_FOUND … dist/cli.js` khi chạy
`node bin/goon.mjs` trên bản clone mới nghĩa là bạn chưa build: chạy `npm run build`.

## 3. Script npm

| Lệnh | Việc làm |
|---|---|
| `npm run check` | `lint` + `build` + test đơn vị + e2e trong một lần chạy `node --test`. `tsc` strict chính là bước typecheck. Chạy trước khi push |
| `npm test` | build rồi chạy `tests/*.test.mjs` (đúng lệnh CI) |
| `npm run test:one -- <file…>` | build rồi chạy riêng file/cờ bạn truyền vào (mục 5) |
| `npm run test:e2e` | build rồi chạy `tests/e2e/*.test.mjs` |
| `npm run build` / `typecheck` / `lint` | `tsc` / `tsc --noEmit` / `oxlint src tests bin scripts` |
| `npm run watch` | `tsc --watch`: build đầu ~10s, các lần lưu sau dưới 1s |
| `npm run dev -- <cờ goon>` | build rồi chạy `node bin/goon.mjs <cờ goon>` |
| `npm run release:pack` | thử đóng gói release cục bộ (mục 12) |

## 4. Vòng lặp phát triển nhanh

```bash
# Terminal 1: build tăng dần
npm run watch

# Terminal 2: chạy thẳng trên dist/ vừa được build
node bin/goon.mjs --stub -p "xin chào"
node --test tests/risk.test.mjs
```

## 5. Chạy test

```bash
npm test                                                        # toàn bộ test đơn vị/tích hợp
npm run test:one -- tests/risk.test.mjs                         # một file
npm run test:one -- --test-name-pattern="rm" tests/risk.test.mjs   # một vài test theo tên
npm run test:e2e                                                # e2e
GOON_UPDATE_GOLDEN=1 node --test tests/golden.test.mjs          # ghi lại tests/golden/*.jsonl, rồi xem git diff
(umask 027; node --test --test-concurrency=4 "tests/*.test.mjs")   # POSIX: CI chạy thêm umask 027 và 007
```

- Luôn đặt glob trong dấu ngoặc kép để Node tự mở rộng (cmd.exe/PowerShell không mở rộng glob).
- `--test-concurrency=4` là có chủ ý (Windows cạn bộ nhớ nếu chạy không giới hạn, xem README).
- Có thể chạy thẳng một file test bằng `node tests/risk.test.mjs`; cách này tiện để gắn debugger
  (`node --inspect-brk tests/risk.test.mjs`).
- **Hai bộ test:**
  - `tests/*.test.mjs`: đơn vị + tích hợp. `tests/helpers.mjs` có `fixture` (thư mục tạm + `XDG_*` riêng),
    `provider` + `sse` (server HTTP giả kiểu OpenAI-compatible), `cli`/`stubCli` (spawn CLI thật).
  - `tests/e2e/*.test.mjs`: chạy CLI như người dùng, chỉ dùng stub provider, không mạng, chỉ assert trên
    trường ổn định (không đụng session id, timestamp, duration). Helper riêng ở `tests/e2e/helpers.mjs`.
- CI chạy e2e trên Linux; Windows/macOS hiện mới chạy bộ test đơn vị.
- Biến `GOON_TEST_BIN` cho phép chạy các test spawn-CLI của `tests/helpers.mjs` trên một `goon.mjs` khác,
  ví dụ bản đã cài từ `.tgz` (`tests/e2e` luôn dùng `bin/goon.mjs` của repo).

## 6. Chạy CLI không cần API key (stub provider)

`--stub` dùng model giả tất định (`src/stub-provider.ts`): không mạng, không đọc key. Cú pháp fixture
nằm trong README (mục "Event log, replay & stub provider") và schema ở `src/stub-fixtures.ts`.

```bash
# Câu trả lời mặc định
node bin/goon.mjs --stub -p "demo"                       # -> [stub] deterministic reply

# Kịch bản có tool call. Prompt PHẢI chứa chuỗi `match` của fixture ("read the file"),
# và thư mục làm việc phải có notes.txt; "demo" sẽ chỉ nhận về "[stub] auxiliary response".
mkdir -p /tmp/goon-demo && echo "xin chào" > /tmp/goon-demo/notes.txt
node bin/goon.mjs --stub --stub-fixture tests/fixtures/stub-tool-call.json \
  --cwd /tmp/goon-demo -p "please read the file"
node bin/goon.mjs --stub --stub-fixture tests/fixtures/stub-tool-call.json \
  --cwd /tmp/goon-demo --json -p "please read the file"  # NDJSON event, giống test e2e quan sát

# TUI toàn màn hình với stub (không có -p)
node bin/goon.mjs --stub --cwd /tmp/goon-demo
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
node bin/goon.mjs --provider openai-compatible --base-url http://127.0.0.1:11434/v1 --model <MODEL_ID> -p "..."
#   Ollama: :11434/v1    llama.cpp llama-server: :8080/v1    vLLM: :8000/v1
```

Cần server giả có kịch bản qua HTTP (kiểm tra đúng payload gửi đi)? Xem `provider()` và `sse()` trong
`tests/helpers.mjs`; hầu hết test tích hợp dùng chúng.

## 8. Debug

- `node --enable-source-maps bin/goon.mjs …` để stack trace trỏ về `src/*.ts` thay vì `dist/*.js`.
- `node --inspect-brk bin/goon.mjs --stub -p hi` rồi gắn `chrome://inspect` hoặc debugger của editor.
  `tsconfig.json` bật `sourceMap`, nên đặt breakpoint trực tiếp trong `src/*.ts` được (giữ `npm run watch`).
- `goon doctor`: cấu hình hiệu lực, đường dẫn, thư mục dữ liệu, khả năng tới endpoint. Trong TUI: `/status`, `/debug-config`.
- `--json` in event NDJSON; `--event-log` ghi `sessions/<id>.events.jsonl`, đọc lại bằng `goon replay <ID>`
  (`--envelopes`, `--rebuild-session`). Replay không cần model/key.
- `--no-tui` dùng readline thuần, dễ quan sát khi nghi lỗi nằm ở TUI.

## 9. Kiến trúc: đọc code từ đâu

Luồng chính: `bin/goon.mjs` (đường nhanh cho `--version`/`--help`) → `dist/cli.js` `main()` (cờ, lệnh con,
vòng đời) → `config.ts` `resolveRuntime` (ưu tiên: cờ > biến `GOON_*` > config > mặc định) →
`ToolRegistry` (`tools.ts`) + `Harness` (`harness.ts`, vòng lặp agent: stream model → tool call → approval →
thực thi → checkpoint ở `session.ts`). Model do `provider.ts` `createModel` tạo (`--stub` rẽ sang
`stub-provider.ts` tại đây). `tui.ts` chỉ lo hiển thị/nhập liệu; `index.ts` là API công khai của package.

- **Bảng "file → trách nhiệm"** đầy đủ nằm ở cuối mục "Phát triển & Kiểm thử" trong [README](README.md).
- Các file lớn nhất là `cli.ts` (~3,8k dòng) và `tui.ts` (~3k dòng); phần còn lại là module nhỏ, nhiều module
  thuần (không I/O) có test riêng, ví dụ `risk.ts`, `policy.ts`, `cost.ts`, `context-budget.ts`.
- Mỗi module phức tạp mở đầu bằng comment nêu bất biến của nó (xem đầu `coord.ts`, `context-budget.ts`, `memory.ts`).
- Tài liệu trong `docs/`: `SPEC.md` (hợp đồng thiết kế), `MCP.md` (cấu hình MCP), `AUDIT.md` (audit bảo mật
  kèm test hồi quy). `PLAN-yolo-port.md`, `IMPLEMENTATION-REPORT.md`, `VALIDATION.md`, `REFERENCE_REVIEW.md`
  là báo cáo/kế hoạch **tại thời điểm viết**, số liệu trong đó có thể đã cũ.

## 10. Quy ước code

- TypeScript strict, ESM (`NodeNext`): import tương đối phải kèm đuôi `.js` (`from './files.js'`).
  Thụt lề 2 dấu cách, LF, newline cuối file (`.editorconfig`). Lint bằng `oxlint` với cấu hình mặc định.
- Test là `.mjs` thuần chạy trên `dist/` bằng `node:test`; mỗi thay đổi hành vi cần test đi kèm.
- Không thêm dependency native hay dependency runtime mới nếu không thật sự cần. Dependency được ghim chính
  xác; đổi bản bằng `npm install <gói>@<x.y.z> --save-exact` để `package.json` và `npm-shrinkwrap.json` cùng cập nhật.
- Chạy được trên Termux, Linux, macOS và Windows: không giả định `/tmp`, dấu `/` hay mode bit POSIX trong test
  (tạo thư mục tạm qua `fixture()`/`e2eFixture()`, và `skip` rõ ràng cho phần chỉ có ở POSIX).
- Giữ các bất biến bảo mật trong `docs/SPEC.md` và `docs/PLAN-yolo-port.md` §0: key không vào môi trường tiến
  trình con, mọi thứ ghi ra log/session/memory đều qua `redact.ts`, `HarnessEvent` và config chỉ được thêm
  trường tuỳ chọn (config cũ vẫn phải parse được nhờ giá trị mặc định).

## 11. Commit, pull request, CHANGELOG

- Nhánh từ `main`; PR nhắm vào `main`. PR chạy job `check` (không phát hành).
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
(`.github/workflows/release.yml`) chạy matrix Ubuntu/Windows/macOS × Node 22.16/24, rồi job `release` (chỉ trên
`binxgtl/goon-cli`, không chạy cho PR) gọi `scripts/publish-release.mjs`:

1. Chọn phiên bản (`scripts/release-version.mjs`): nếu `version` trong `package.json` cao hơn mọi tag `v*`
   thì dùng nguyên; ngược lại tăng patch của tag cao nhất. Nếu số đổi, bot đẩy commit
   `chore: release vX.Y.Z [skip ci]` (chỉ sửa `package.json` + `npm-shrinkwrap.json`) và tag `vX.Y.Z`.
2. Đóng gói (`scripts/pack-release.mjs`): build, `npm pack`, cài thử `.tgz` vào prefix tạm với `--omit=dev`
   và chạy `--version`/`--help`.
3. Tạo GitHub Release với `goon-cli-X.Y.Z.tgz`, bản alias `goon-cli.tgz` (là địa chỉ cài trong README) và `SHA256SUMS.txt`.

Việc thủ công duy nhất là **tăng minor/major**: chạy `npm version minor --no-git-tag-version` (cập nhật cả
`package.json` lẫn `npm-shrinkwrap.json`, hai nơi phải khớp) trong một PR. Lần push kế tiếp lên `main` sẽ phát
hành đúng số đó.

Thử đóng gói trên máy (không đẩy gì lên GitHub): `npm run release:pack`; kết quả nằm trong `release-out/`
(đã ignore). Đừng chạy `scripts/publish-release.mjs` cục bộ: nó tạo tag, push và phát hành thật.

## 13. Ghi chú cho Termux

- Cài: `pkg install nodejs-lts git bash ripgrep python`; làm việc trong `$HOME` (không dùng `/sdcard`) để có
  đủ quyền file và executable bit.
- Chạy nhanh các phần đặc thù Termux: `bash scripts/smoke-termux.sh` (hoặc `--no-install`): npm ci + build + test
  eventlog, stub-provider, golden. Script không thuộc CI.
- Trên máy yếu, ưu tiên `npm run test:one -- <file>` thay vì toàn bộ `npm test`; test PTY cần `python3`.
