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
(`credentials.json` quyền `0600`), tự động quét danh sách model từ `/models` của endpoint
(không hardcode model) và chọn mức `effort` (nhấn `Esc` bất kỳ lúc nào để huỷ không lưu dở):

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

Goon giữ nguyên đường dẫn Base URL bạn cấu hình (chỉ tự động chuẩn hoá thêm `/v1` cho một số gateway quen thuộc như `api.yuristore.xyz` hoặc `localhost:8000` khi bỏ trống path). Nếu không đặt `baseUrl`, mặc định sẽ dùng `DEFAULT_GATEWAY_URL` (`https://api.yuristore.xyz/v1`). Endpoint phải hỗ trợ tool calling.

## Cách dùng CLI

```bash
goon                                              # Mở giao diện TUI toàn màn hình
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

Khi chạy `goon` trong terminal, giao diện TUI toàn màn hình hiển thị:
- **Header Band**: Phiên bản, model + mức `effort`, thư mục workspace, nhánh Git hiện tại và thanh đo ngữ cảnh trực tiếp (`ctx ▰▰▰▱▱▱▱▱ 34%`).
- **Hội thoại & Bảng biểu**: Render Markdown, bảng kẻ khung căn cột tự động, khối diff màu trực quan (`+dòng thêm` xanh lá, `-dòng xoá` đỏ) và trạng thái tool/subagent thời gian thực.
- **Khung nhập lệnh (Composer)**: Đổi màu viền theo chế độ (`ask` / `auto` / `plan` / `readonly`), hiển thị bộ đếm token trực tiếp ở cạnh dưới và popup gợi ý lệnh `/slash` kèm mô tả.

### Phím tắt chính

| Phím tắt | Chức năng |
|---|---|
| `Ctrl+T` | Bật/tắt **Transcript Mode** toàn màn hình — hiển thị đầy đủ cả khối suy luận (`Thinking`), cuộn xem toàn bộ lịch sử; nhấn `Ctrl+T`, `Esc` hoặc `q` để quay lại ngay |
| `Shift+Tab` | Chuyển nhanh chế độ quyền: `ask` → `auto` (auto-approve) → `plan` → `readonly` |
| `Alt+.` / `Alt+,` | Tăng / giảm mức suy luận (`effort`: `low` ↔ `medium` ↔ `high` ↔ `xhigh` ↔ `max`) |
| `Esc Esc` (nhấn đúp) | Mở bảng **Rewind / Revert** về checkpoint của các lượt trước và khôi phục file |
| `Up` / `Down` / `Tab` | Di chuyển chọn lệnh trong popup `/slash` (hoặc gọi lại lịch sử lệnh khi không mở popup) |
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
- `/view [normal|verbose]`: Đổi chế độ hiển thị suy nghĩ (`normal` ẩn khối `Thinking` cho gọn; `verbose` hiện đầy đủ toàn bộ quá trình suy luận).
- `/transcript`: Bật/tắt chế độ xem Transcript toàn màn hình (giống `Ctrl+T`).
- `/model [tên_model]` & `/models`: Đổi model hoặc quét trực tiếp danh sách model từ API.
- `/effort [low|medium|high|xhigh|max|clear]`: Chỉnh mức độ suy luận (`reasoning_effort` / thinking budget).
- `/resume [latest|số_thứ_tự|id_prefix]`: Mở menu chọn và khôi phục phiên làm việc cũ (tự động bỏ qua các phiên rỗng 0 tin nhắn).
- `/new`, `/clear`, `/fork [tên]`, `/rename <tiêu_đề>`: Tạo phiên mới, xoá màn hình, nhân bản nhánh hội thoại hoặc đặt tên phiên.
- `/rewind [N]` & `/revert [N] [both|code|messages]`: Quay lui hội thoại và/hoặc khôi phục trạng thái file về trước lượt thứ `N`.
- `/undo`: Hoàn tác các thay đổi file của lượt vừa chạy.
- `/btw <câu_hỏi>`: Hỏi nhanh câu hỏi phụ (ephemeral side question) dựa trên ngữ cảnh hiện tại mà không làm bẩn lịch sử phiên.
- `/goal [set|done|clear]`, `/memory [list|add|remove|clear]`, `/recap`: Đặt mục tiêu phiên, lưu ghi nhớ dài hạn (`memories.json`) hoặc tóm tắt tiến độ.
- `/init`, `/skills`, `/mention <path>`: Khởi tạo `AGENTS.md`, liệt kê `SKILL.md` (gọi qua `$skill-name`) hoặc đính kèm `@file`/ảnh.
- `/diff`, `/git`, `/review`: Xem `git status` + `git diff`, hoặc yêu cầu model review toàn bộ thay đổi hiện tại.
- `/compact [trọng_tâm]`: Chủ động nén ngữ cảnh hội thoại (có lưu archive đầy đủ trước khi nén).
- `/setup`, `/provider`, `/key`, `/doctor`: Cấu hình lại provider (nhấn `Esc` để huỷ), đổi API key hoặc kiểm tra kết nối/môi trường.
- `!<lệnh>`: Chạy nhanh lệnh shell trực tiếp ngay trong TUI (ví dụ `!git status`).

## Harness, Công cụ & Hoàn tác (Checkpoint / Rewind)

- **Bộ công cụ tích hợp đầy đủ**:
  - **File & Shell**: `read_file`, `list_files`, `search`, `write_file`, `edit_file`, `apply_patch`, `bash`, `view_image`.
  - **Kế hoạch & Tương tác**: `todo_read`, `todo_write`, `update_plan`, `request_user_input`, `exit_plan_mode`.
  - **Subagents (7 công cụ)**: Vòng đời tương tác (`spawn_agent`, `wait_agent`, `send_input`, `close_agent`, `list_agents`) và uỷ thác theo lô (`delegate_task`, `delegate_edit`).
  - **MCP Resources & Tools**: `list_mcp_resources`, `list_mcp_resource_templates`, `read_mcp_resource` cùng các tool động từ MCP server (`mcp__<server>__<tool>`).
- **Cơ chế Checkpoint & Rollback nhẹ, chính xác (`/rewind`, `/revert`, `/undo`)**:
  - Chỉ các file được sửa trực tiếp qua bộ công cụ chỉnh sửa (`write_file`, `edit_file`, `apply_patch` — bao gồm cả subagent worker được uỷ quyền sửa file) mới được ghi nhận bản sao lưu trước lượt chạy (trạng thái tồn tại, nội dung gốc và file mode) để rollback tức thì 100% và không tốn I/O quét toàn bộ workspace khi chạy lệnh shell (`bash`).
- **Tự động Retry API**: Khi gặp lỗi mạng hoặc lỗi upstream tạm thời (HTTP `5xx`, `429`, `408`, lỗi đóng stream/socket bất ngờ), Harness tự động thử lại tối đa **3 lần** với exponential backoff trước khi dừng.
- **Hiển thị đầy đủ Unified Diff**: `computeUnifiedDiff` hiển thị trọn vẹn số dòng thêm/xoá thực tế (`+N -M`, hỗ trợ tới 5.000 dòng mỗi hunk).
- **Bảo vệ chống lỗi file `nul` trên Windows**:
  - Tự động chuyển hướng `>nul`, `2>nul`, `>>NUL`, `>/dev/nul` trong lệnh `bash` trên Windows về `/dev/null` an toàn (`sanitizeBashCommand`).
  - `WorkspaceFiles` từ chối tạo các tên thiết bị dành riêng của Windows (`nul`, `con`, `prn`, `aux`, `com1-9`, `lpt1-9`) trên mọi nền tảng để giữ an toàn cho Git repo đa nền tảng.
  - `unlinkSafe` hỗ trợ xoá cả các file tên thiết bị bị tạo nhầm trước đó thông qua đường dẫn mở rộng `\\?\`.
- **An toàn phiên & file**:
  - Lưu phiên atomic, lock tiến trình, tự động dọn phiên rỗng (`0 messages`) khi thoát hoặc đổi phiên.
  - Ghi file theo temporary file + `chmod` + `rename`, bảo toàn nguyên vẹn quyền file (`0755`, umask `027`/`007`).

## Reasoning, Thinking Replay & Gateway Inline Think

- **Bảo toàn `reasoning_content` qua nhiều bước tool (`withReasoningReplay`)**:
  - Với các endpoint `openai-compatible` ở chế độ thinking (DeepSeek, GLM, Kimi, v.v.), `goon-cli` giữ nguyên 100% `reasoning_content` thật của model qua các bước gọi tool và resume.
  - Khi chế độ thinking đang bật (qua `--effort` hoặc lịch sử có khối reasoning) mà có bước `assistant` gọi tool nhanh không sinh thinking, `withReasoningReplay` tự động đính kèm `reasoning_content: ""` trên bản chiếu request để tránh lỗi HTTP 400 (`reasoning_content ... must be passed back`) mà không bao giờ chèn chuỗi giả vào lịch sử lưu trên đĩa.
- **Tách `<think>...</think>` inline khi stream (`AnswerProjection`)**:
  - Gateway trả `<think>...</think>` ở đầu `content` được tách ngay khi stream, kể cả khi thẻ bị cắt giữa nhiều chunk. Chỉ phần answer đi vào kết quả cuối và report của agent con.
  - Native reasoning, chữ ký (`signature`) của Anthropic và provider metadata trong transcript gốc vẫn được giữ nguyên.

```bash
goon --show-reasoning --json -p 'Kiểm tra dự án'  # xuất event reasoning riêng
goon --show-reasoning -p 'Kiểm tra dự án'         # hiển thị reasoning trên stderr/TUI
goon --inline-reasoning off -p 'Trả tài liệu XML bắt đầu bằng <think>'
```

## MCP (Model Context Protocol)

Goon dùng MCP SDK chính thức, hỗ trợ **tools và resources qua stdio và Streamable HTTP**.
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
goon mcp list           # xem cấu hình, không kết nối
goon mcp check          # kết nối và in tên tool gốc/tên namespaced
goon mcp import-codex   # nhập nhanh các MCP server từ ~/.codex/config.toml
goon                    # tự nạp các server đã bật
goon --no-mcp           # bỏ qua MCP trong lượt này
```

Xem [docs/MCP.md](docs/MCP.md) để biết chi tiết cấu hình stdio, biến môi trường và giới hạn.

## Subagents (Tương tác nền & Uỷ thác theo lô)

- **Vòng đời tương tác kiểu Codex (`spawn_agent`, `wait_agent`, `send_input`, `close_agent`, `list_agents`)**:
  - Khởi chạy agent con (`explorer` chỉ đọc hoặc `worker` có quyền sửa file) chạy nền song song, gửi thêm chỉ dẫn bằng `send_input` và thu hoạch kết quả qua `wait_agent`.
- **Uỷ thác theo lô (`delegate_task` & `delegate_edit`)**:
  - `delegate_task`: chia tối đa 4 việc đọc/review độc lập, tối đa 2 agent chạy đồng thời.
  - `delegate_edit`: chia tối đa 4 việc sửa file, chạy tuần tự; tôn trọng chế độ duyệt quyền (`ask` hoặc `auto`/`--yes`) của phiên cha.

```bash
goon --read-only -p 'Dùng spawn_agent hoặc delegate_task để khảo sát cấu trúc repo và tìm rủi ro trong test'
goon -p 'Dùng delegate_edit để sửa lỗi trong src/files.ts và bổ sung regression test'
goon --no-subagents -p 'Đọc repo và làm trực tiếp'
```

## Cấu hình và Dữ liệu

- Config: `$XDG_CONFIG_HOME/goon/config.json` (mặc định `~/.config/goon/config.json`).
- Credentials (API Keys): `$XDG_CONFIG_HOME/goon/credentials.json` (quyền `0600`).
- Memories & Presets: `$XDG_CONFIG_HOME/goon/memories.json`, `$XDG_CONFIG_HOME/goon/presets.json`.
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
| `src/harness.ts` | Agent loop, retry API, điều phối tool, checkpoint, `/btw`, usage, cancellation |
| `src/provider.ts` | Chọn adapter API, quét `/models`, truyền `reasoningEffort` / thinking budget |
| `src/setup.ts` | Setup wizard tương tác (`Esc` để huỷ), quét `/models` động, lưu `credentials.json` |
| `src/tools.ts` | Registry, schema, alias chuẩn hoá, `sanitizeBashCommand` và approval |
| `src/files.ts`, `src/process.ts` | File I/O, unified diff, patch, `snapshotWorkspace`, bảo vệ `nul` và subprocess |
| `src/session.ts` | Lock, checkpoint, rewind, resume và sửa lịch sử bị ngắt |
| `src/context.ts` | System prompt, `AGENTS.md`, `SKILL.md`, `memories.json`, `presets.json` và compaction |
| `src/cli.ts`, `src/tui.ts` | Flags, giao diện TUI, Transcript Mode (`Ctrl+T`), popup `/slash` |
| `src/mcp.ts` | MCP client, stdio/HTTP transport, resources, `import-codex`, lifecycle |
| `src/subagents.ts` | `spawn_agent`/`wait_agent` & `delegate_task`/`delegate_edit`, concurrency, budget |
| `src/reasoning.ts` | `withReasoningReplay` (`reasoning_content`), `AnswerProjection`, giữ transcript gốc |

## Repository & Tự động Release

- Source: [binxgtl/goon-cli](https://github.com/binxgtl/goon-cli)
- Bản cài và checksum: [GitHub Releases](https://github.com/binxgtl/goon-cli/releases)

Mỗi lần push lên `main` (trừ khi commit có `[skip ci]`), workflow `Check and release`
sẽ chạy toàn bộ typecheck, lint, test (Node 22.16 & Node 24 + restrictive umasks),
tự động tăng phiên bản patch, đóng gói `.tgz` kèm `SHA256SUMS.txt` và phát hành
GitHub Release mới.
