# goon cli

CLI coding agent độc lập, ưu tiên Termux và Windows/Linux/macOS, dùng API của bạn.
Tham khảo kiến trúc OpenAI Codex, ZCode và DeepSeek Harness; **không fork base và không dùng runtime của các dự án đó**.

## Cài đặt (Termux / Linux / macOS / Windows)

Cài bản GitHub Release mới nhất:

```bash
# Trên Termux (cài thêm công cụ hệ thống nếu chưa có):
pkg install nodejs-lts git bash ripgrep

node --version
npm install -g https://github.com/binxgtl/goon-cli/releases/latest/download/goon-cli.tgz --omit=dev
goon --version
```

Cần Node **22.16 trở lên**; Node 24 cũng được. Bản npm đã chứa JS biên dịch sẵn;
không cần compiler, pnpm, proot, glibc hay addon native. Thiếu `ripgrep` vẫn có
search JavaScript dự phòng giới hạn kích thước.

Runtime dependencies được tải từ npm khi cài; gói này không kèm `node_modules`.
Trên Termux, hãy làm việc trong `$HOME` để có đầy đủ quyền file và executable bit.

## Thiết lập nhanh (`goon setup` hoặc `goon init`)

Chạy wizard tương tác để chọn provider, nhập Base URL, lưu API key an toàn
(`keys.json` quyền `0600`), tự động quét danh sách model từ `/models` của endpoint
(không hardcode model) và chọn mức `effort`:

```bash
goon setup
# hoặc trong TUI gõ:
/setup
```

Hoặc cấu hình bằng lệnh `goon init` truyền thống:

```bash
goon init \
  --provider openai-compatible \
  --base-url 'https://YOUR_PROVIDER/v1' \
  --model 'YOUR_MODEL_ID' \
  --effort xhigh

# Nhập key ẩn qua biến môi trường (hoặc lưu qua goon setup / /key):
read -rsp 'API key: ' GOON_API_KEY; echo
export GOON_API_KEY

cd "$HOME/your-project"
goon doctor
goon
```

| Provider | `--provider` | Base URL |
|---|---|---|
| Dịch vụ OpenAI-compatible | `openai-compatible` | API root trước `/chat/completions`, thường kết thúc `/v1` |
| OpenAI Responses-compatible | `openai-responses` | API root trước `/responses` |
| Anthropic Messages-compatible | `anthropic` | API root trước `/messages`, thường kết thúc `/v1` |

Không tự thêm `/v1` cho URL của bạn: một số gateway dùng đường dẫn riêng.
Nếu không đặt URL, provider Anthropic dùng `https://api.anthropic.com/v1`, hai
provider còn lại dùng `https://api.openai.com/v1`. Endpoint phải hỗ trợ tool calling.

## Cách dùng CLI

```bash
goon                                              # Mở Codex-style TUI toàn màn hình
goon resume                                       # Tiếp tục phiên gần nhất có tin nhắn trong workspace
goon --resume latest                              # Tương đương goon resume
goon --resume SESSION_UUID -p 'Tiếp tục phần đang làm'
goon -p 'Đọc repo, tìm nguyên nhân lỗi và sửa; chạy kiểm tra phù hợp'
goon --effort max -p 'Phân tích sâu và tối ưu hiệu năng'
goon --read-only -p 'Đọc và giải thích kiến trúc repo'
goon --yes -p 'Tự động duyệt mọi thao tác ghi file và bash'
goon --json -p 'Kiểm tra dự án'
printf '%s' 'Giải thích các file chính' | goon --read-only
goon sessions
```

Mặc định ở chế độ `ask` (yêu cầu chấp thuận khi sửa/ghi file hoặc chạy shell).
Không có TTY thì những yêu cầu này bị từ chối trừ khi truyền `--yes` (tương đương
chế độ `auto`). `--read-only` loại bỏ các công cụ ghi/sửa/bash khỏi bộ tool.
Mã thoát: `0` thành công, `1` lỗi, `2` từ chối quyền, `130` Ctrl+C, `143` SIGTERM.

## Giao diện TUI, Phím tắt & Slash Commands

Khi chạy `goon` trong terminal, TUI toàn màn hình kiểu Codex hiển thị banner căn
chuẩn Unicode (hỗ trợ cả đường dẫn tiếng Việt NFC/NFD), lịch sử hội thoại, diff
màu trực quan (`+dòng thêm` xanh lá, `-dòng xoá` đỏ), trạng thái tool/agent và
khung nhập lệnh bo góc.

### Phím tắt chính

| Phím tắt | Chức năng |
|---|---|
| `Ctrl+T` | Bật/tắt **Transcript Mode** toàn màn hình (`T R A N S C R I P T`) — hiển thị đầy đủ cả khối `• Thinking`, cuộn xem toàn bộ lịch sử; nhấn `Ctrl+T`, `Esc` hoặc `q` để quay lại ngay |
| `Shift+Tab` | Chuyển nhanh chế độ quyền: `ask` → `auto` (auto-approve) → `plan` → `readonly` |
| `Alt+.` / `Alt+,` | Tăng / giảm mức suy luận (`effort`: `low` ↔ `medium` ↔ `high` ↔ `xhigh` ↔ `max`) |
| `Esc Esc` (nhấn đúp) | Mở bảng **Rewind / Revert** về checkpoint của các lượt trước và khôi phục cả file trên đĩa |
| `Tab` | Tự động hoàn thành (autocomplete) lệnh `/slash` |
| `Up` / `Down` | Cuộn lịch sử khi đang đọc hoặc gọi lại lệnh cũ trong ô nhập |
| `PgUp` / `PgDn` / Lăn chuột | Cuộn khung hội thoại hoặc khung Transcript |
| `Ctrl+C` | Huỷ lượt model/tool đang chạy |
| `Ctrl+D` | Thoát khi ô nhập trống |

### Thực thi lệnh ngay khi Agent đang chạy (`Running`)

Khi model đang suy nghĩ hoặc chạy tool:
- Gõ các lệnh điều khiển như `/auto`, `/yolo`, `/view`, `/mode`, `/effort`, `/model`,
  `/status`, `/help`, `/tools`, `/diff`, `/git`, `/todo`, `/plan`, `/copy`, `!<lệnh shell>`
  sẽ **chạy và áp dụng ngay lập tức** mà không cần chờ xong lượt.
- Gõ tin nhắn prompt bình thường khi máy đang bận sẽ được đưa vào hàng đợi (`queuedPrompts`)
  để tự động chạy ngay khi lượt hiện tại kết thúc.

### Danh sách Slash Commands tiêu biểu

- `/auto` (hoặc `/yolo [on|off]`): Bật/tắt chế độ **Auto-Approve** (tự động chấp thuận mọi thao tác sửa file, bash và subagent).
- `/mode [ask|auto|plan|readonly]`: Xem hoặc đổi chế độ quyền thực thi.
- `/view [normal|verbose]`: Đổi chế độ hiển thị suy nghĩ (`normal` ẩn khối `• Thinking` cho gọn; `verbose` hiện đầy đủ toàn bộ quá trình suy luận).
- `/transcript`: Bật/tắt chế độ xem Transcript toàn màn hình (giống `Ctrl+T`).
- `/model [tên_model]` & `/models`: Đổi model hoặc quét trực tiếp danh sách model từ API.
- `/effort [low|medium|high|xhigh|max|clear]`: Chỉnh mức độ suy luận (`reasoning_effort` / thinking budget).
- `/resume [latest|số_thứ_tự|id_prefix]`: Mở menu chọn và khôi phục phiên làm việc cũ (tự động bỏ qua các phiên rỗng 0 tin nhắn).
- `/new`, `/clear`, `/fork [tên]`, `/rename <tiêu_đề>`: Tạo phiên mới, xoá màn hình, nhân bản nhánh hội thoại hoặc đặt tên phiên.
- `/rewind [N]` & `/revert [N]`: Quay lui hội thoại và khôi phục trạng thái file về trước lượt thứ `N`.
- `/undo`: Hoàn tác các thay đổi file của lượt vừa chạy.
- `/diff`, `/git`, `/review`: Xem `git status` + `git diff`, hoặc yêu cầu model review toàn bộ thay đổi hiện tại.
- `/compact`: Chủ động nén ngữ cảnh hội thoại (có lưu archive đầy đủ trước khi nén).
- `/setup`, `/provider`, `/key`, `/doctor`: Cấu hình lại provider, đổi API key hoặc kiểm tra kết nối/môi trường.
- `!<lệnh>`: Chạy nhanh lệnh shell trực tiếp ngay trong TUI (ví dụ `!git status`).

## Harness & Công cụ tích hợp

- **8 công cụ cốt lõi**: `read_file`, `list_files`, `search`, `write_file`, `edit_file`, `apply_patch`, `todo_write`, `bash` cùng 2 công cụ subagent (`delegate_task`, `delegate_edit`).
- **Tự động Retry API**: Khi gặp lỗi mạng hoặc lỗi upstream tạm thời (HTTP `5xx`, `429`, `408`, lỗi đóng stream/socket bất ngờ), Harness tự động thử lại tối đa **3 lần** với exponential backoff trước khi dừng.
- **Hiển thị đầy đủ Unified Diff**: `computeUnifiedDiff` hiển thị trọn vẹn số dòng thêm/xoá thực tế (`+N -M`, hỗ trợ tới 5.000 dòng mỗi hunk thay vì cắt ở 60 dòng).
- **Bảo vệ chống lỗi file `nul` trên Windows**:
  - Tự động chuyển hướng `>nul`, `2>nul`, `>>NUL`, `>/dev/nul` trong lệnh `bash` trên Windows về `/dev/null` an toàn (`sanitizeBashCommand`).
  - `WorkspaceFiles` từ chối tạo các tên thiết bị dành riêng của Windows (`nul`, `con`, `prn`, `aux`, `com1-9`, `lpt1-9`) trên mọi nền tảng để giữ an toàn cho Git repo đa nền tảng.
  - `unlinkSafe` hỗ trợ xoá cả các file tên thiết bị bị tạo nhầm trước đó thông qua đường dẫn mở rộng `\\?\`.
- **An toàn phiên & file**:
  - Lưu phiên atomic, lock tiến trình, tự động dọn phiên rỗng (`0 messages`) khi thoát hoặc đổi phiên.
  - Ghi file theo temporary file + `chmod` + `rename`, bảo toàn nguyên vẹn quyền file (`0755`, umask `027`/`007`).

## Reasoning và Gateway Inline Think

Gateway trả `<think>...</think>` ở đầu content được tách ngay khi stream, kể cả khi
thẻ bị cắt giữa nhiều chunk. Chỉ phần answer đi vào kết quả cuối và report của
agent con. Native reasoning, signature và provider metadata trong transcript gốc
vẫn giữ nguyên để tool roundtrip/resume.

```bash
goon --show-reasoning --json -p 'Kiểm tra dự án'  # xuất event reasoning riêng
goon --show-reasoning -p 'Kiểm tra dự án'         # hiển thị reasoning trên stderr/TUI
goon --inline-reasoning off -p 'Trả tài liệu XML bắt đầu bằng <think>'
```

## MCP (Model Context Protocol)

Goon dùng MCP SDK chính thức, hỗ trợ **tools qua stdio và Streamable HTTP**.
Tạo `~/.config/goon/mcp.json` (hoặc `$XDG_CONFIG_HOME/goon/mcp.json`):

```json
{
  "servers": {
    "remote": {
      "transport": "http",
      "url": "https://YOUR_MCP_HOST/mcp",
      "bearerEnv": "MY_MCP_TOKEN",
      "readOnlyTools": []
    }
  }
}
```

```bash
goon mcp list    # xem cấu hình, không kết nối
goon mcp check   # kết nối và in tên tool gốc/tên namespaced
goon             # tự nạp các server đã bật
goon --no-mcp    # bỏ qua MCP trong lượt này
```

Xem [docs/MCP.md](docs/MCP.md) để biết chi tiết cấu hình stdio, biến môi trường và giới hạn.

## Subagent (`delegate_task` & `delegate_edit`)

- `delegate_task`: chia tối đa bốn việc đọc/review, tối đa hai agent chạy đồng thời.
- `delegate_edit`: chia tối đa bốn việc sửa, chạy tuần tự; tôn trọng chế độ duyệt quyền (`ask` hoặc `auto`/`--yes`) của phiên cha.

```bash
goon --read-only -p 'Dùng delegate_task để xem cấu trúc repo và tìm rủi ro trong test, rồi tổng hợp'
goon -p 'Dùng delegate_edit để sửa lỗi trong src/files.ts và bổ sung regression test'
goon --no-subagents -p 'Đọc repo và làm trực tiếp'
```

## Cấu hình và Dữ liệu

- Config: `$XDG_CONFIG_HOME/goon/config.json` (mặc định `~/.config/goon/config.json`).
- Keys: `$XDG_CONFIG_HOME/goon/keys.json` (quyền `0600`).
- Sessions: `$XDG_DATA_HOME/goon/sessions/` (mặc định `~/.local/share/goon/sessions/`).
- Archives: `SESSION_UUID.compact-N.json` trong cùng thư mục session.
- Ưu tiên: cờ CLI > biến `GOON_*` > config người dùng > mặc định.

Các biến môi trường hỗ trợ: `GOON_PROVIDER`, `GOON_BASE_URL`, `GOON_MODEL`, `GOON_EFFORT`,
`GOON_KEY_ENV`, `GOON_API_KEY`, `GOON_CONTEXT_WINDOW`, `GOON_MAX_OUTPUT_TOKENS`,
`GOON_TIMEOUT_MS`, `GOON_RUN_TOKEN_BUDGET`, `GOON_INLINE_REASONING`.

Context mặc định `128.000` token, output `16.384` token, timeout mỗi request `300.000` ms,
và **không giới hạn token mỗi lượt chạy theo mặc định** (`runTokenBudget = 0`) để
tránh ngắt giữa chừng các tác vụ dài. Nếu muốn đặt trần token cho một lượt chạy:

```bash
goon --run-token-budget 200000 -p 'Tác vụ có giới hạn ngân sách token'
```

## Phát triển & Kiểm thử

```bash
npm ci
npm run typecheck
npm run lint
npm test
(umask 027; node --test tests/*.test.mjs)
(umask 007; node --test tests/*.test.mjs)
npm run build
npm install -g . --omit=dev
```

| File | Trách nhiệm |
|---|---|
| `src/harness.ts` | Agent loop, retry API, điều phối tool, usage, cancellation |
| `src/provider.ts` | Chọn adapter API, truyền `reasoningEffort` / thinking budget |
| `src/setup.ts` | Setup wizard tương tác, quét `/models` động, quản lý `keys.json` |
| `src/tools.ts` | Registry, schema, alias chuẩn hoá, `sanitizeBashCommand` và approval |
| `src/files.ts`, `src/process.ts` | File I/O, unified diff, patch, bảo vệ `nul` và subprocess |
| `src/session.ts` | Lock, checkpoint, rewind, resume và sửa lịch sử bị ngắt |
| `src/context.ts` | System prompt, `AGENTS.md` và auto-compaction |
| `src/cli.ts`, `src/tui.ts` | Flags, Codex-style TUI, Transcript Mode (`Ctrl+T`), slash commands |
| `src/mcp.ts` | MCP client, transport, schema, lifecycle |
| `src/subagents.ts` | Context con, concurrency, budget và approval |
| `src/reasoning.ts` | Projection answer/reasoning, giữ transcript gốc |

## Repository & Tự động Release

- Source: [binxgtl/goon-cli](https://github.com/binxgtl/goon-cli)
- Bản cài và checksum: [GitHub Releases](https://github.com/binxgtl/goon-cli/releases)

Mỗi lần push lên `main` (trừ khi commit có `[skip ci]`), workflow `Check and release`
sẽ chạy toàn bộ typecheck, lint, test (Node 22.16 & Node 24 + restrictive umasks),
tự động tăng phiên bản patch, đóng gói `.tgz` kèm `SHA256SUMS.txt` và phát hành
GitHub Release mới.
