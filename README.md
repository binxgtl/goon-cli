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
goon resume                                       # Chọn phiên để tiếp tục (thư mục này / mọi dự án), chạy từ bất kỳ đâu
goon --resume latest                              # Tương đương goon resume
goon --resume SESSION_UUID -p 'Tiếp tục phần đang làm'
goon -p 'Đọc repo, tìm nguyên nhân lỗi và sửa; chạy kiểm tra phù hợp'
goon --effort max -p 'Phân tích sâu và tối ưu hiệu năng'
goon --read-only -p 'Đọc và giải thích kiến trúc repo'
goon --allow-net-hosts=api.github.com -p 'Gọi API đã được allowlist'
goon --yes -p 'Tự động duyệt mọi thao tác ghi file và bash'
goon --verify-cmd 'npm test' --verify-cmd 'npm run typecheck' -p 'Sửa lỗi và kiểm chứng'
goon --no-verify -p 'Chỉ sửa nhanh, không chạy kiểm chứng'
goon --max-cost-usd 0.50 --max-time-ms 600000 -p 'Tác vụ có trần chi phí và thời gian'
goon --event-log -p 'Ghi lại toàn bộ sự kiện của phiên chạy'
goon replay 1a2b3c4d                         # In lại sự kiện thô từ event log (ID hoặc tiền tố)
goon replay 1a2b3c4d --envelopes             # In cả envelope (v/seq/at/sessionId)
goon replay 1a2b3c4d --rebuild-session       # Dựng lại session từ log, ghi <ID>.replayed.json
goon --stub -p 'Kiểm thử CLI không cần mạng/API key'   # Stub provider tất định cho test
goon --json -p 'Kiểm tra dự án'
printf '%s' 'Giải thích các file chính' | goon --read-only
git diff | goon -p 'Review diff này'              # dữ liệu pipe vào stdin được đính kèm làm ngữ cảnh
goon -p 'Giải thích lỗi' < build.log               # tương tự với file chuyển hướng
goon -p 'Viết changelog' -o CHANGELOG.draft.md     # ghi câu trả lời cuối ra file (scripting/CI)
goon sessions
```

Mặc định ở chế độ `ask` (yêu cầu chấp thuận khi sửa/ghi file hoặc chạy shell).
Không có TTY thì những yêu cầu này bị từ chối trừ khi truyền `--yes` (tương đương
chế độ `auto`). `--read-only` loại bỏ các công cụ ghi/sửa/bash khỏi bộ tool.
Mã thoát: `0` thành công, `1` lỗi, `2` từ chối quyền, `130` Ctrl+C, `143` SIGTERM.

## Giao diện TUI, Phím tắt & Slash Commands

Khi chạy `goon` trong terminal, giao diện TUI toàn màn hình tối giản: không khung viền, không
nền tô màu, dùng màu mặc định của terminal (đẹp trên cả theme sáng lẫn tối).

```
› Sửa test checkout đang fail

○ Explored
  └ Read src/cart.ts
● Ran npm test -- cart (2s)
± Edited src/cart.ts (+1 -1)
◆ Tổng tiền tính bằng số thực mà không làm tròn…

⠋ Working  4s   esc to interrupt
──────────────────────────────────────────────────────────────
› Ask anything   / commands   @ files   $ skills   ! shell
ask   glm-5.3-flash   effort max        shop-api (main)   ctx 34%   69k tokens
```

- **Hội thoại**: Markdown, bảng căn cột, diff màu (`+` xanh, `-` đỏ), khối code có lề mờ, trạng thái tool/subagent theo thời gian thực.
- **Dòng nhập**: một đường kẻ (chuyển màu hổ phách khi goon đang chờ bạn trả lời/duyệt quyền), dòng nhập `›` và popup gợi ý `/slash` kèm mô tả.
- **Dòng trạng thái**: chế độ (`auto-approve` hiện màu hổ phách vì là chế độ rủi ro), model, effort, workspace (nhánh git), todo, % ngữ cảnh do provider báo và tổng token.

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
  `/status`, `/cost`, `/help`, `/tools`, `/diff`, `/git`, `/todo`, `/plan`, `/copy`, `!<lệnh shell>`
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
- `/resume [latest|số_thứ_tự|id_prefix]`: Mở bảng chọn phiên với 2 tab **This folder** / **All projects** (`←`/`→` hoặc `Tab` để đổi tab, `↑`/`↓` chọn, gõ để lọc theo nội dung, tên thư mục hoặc model, `Enter` để resume). Chọn phiên của dự án khác sẽ tự chuyển workspace sang thư mục đó, nên không cần nhớ phiên nằm ở thư mục nào. `goon resume` (hoặc `goon --resume ID`) cũng chạy được từ bất kỳ thư mục nào.
- `/new`, `/clear`, `/fork [tên]`, `/rename <tiêu_đề>`: Tạo phiên mới, xoá màn hình, nhân bản nhánh hội thoại hoặc đặt tên phiên.
- `/rewind [N]` & `/revert [N] [both|code|messages]`: Quay lui hội thoại và/hoặc khôi phục trạng thái file về trước lượt thứ `N`.
- `/undo`: Hoàn tác các thay đổi file của lượt vừa chạy.
- `/btw <câu_hỏi>`: Hỏi nhanh câu hỏi phụ (ephemeral side question) dựa trên ngữ cảnh hiện tại mà không làm bẩn lịch sử phiên.
- `/goal [set|done|clear]`, `/memories [list|add|rm|clear]`, `/recap`: Đặt mục tiêu phiên, lưu ghi nhớ dài hạn (`memories.json`) hoặc tóm tắt tiến độ.
- `/memory [rag index|rag stats|rag search <truy_vấn>|budget|pref set/get/rm/list]`: Memory store v0.6 — chỉ mục lexical của workspace, thống kê, tra cứu và tuỳ chọn người dùng (xem mục *Memory & lexical RAG*). `/memory <text>` vẫn là alias của `/memories add`.
- `/init`, `/skills`, `/mention <path>`: Khởi tạo `AGENTS.md`, liệt kê `SKILL.md` (gọi qua `$skill-name`) hoặc đính kèm `@file`/ảnh.
- `/pair [start|stop|status|lan]`: Mở **Cloudflared Quick Tunnel** miễn phí (`*.trycloudflare.com`), tạo mã QR ngay trên terminal + link bảo mật để quét bằng điện thoại và điều khiển phiên `goon` từ xa (chat, xem stream, đổi chế độ, duyệt/từ chối quyền tool).
- `/workflow [list|show|create|rm|run <tên> [tham_số]]`: Quản lý và thực thi các pipeline đa bước tuỳ chỉnh (`.json` hoặc `.md` trong `.goon/workflows/`) hoặc workflow tích hợp sẵn (`feature-dev`, `bug-hunt`, `tdd-cycle`, `security-audit`).
- `/network [<chủ_đề>|board|send|clear]` (hoặc `/brainstorm`, `/council`): Triệu tập **Agent Network** (nhiều vai trò `Architect`, `Researcher`, `Critic`, `Pragmatist`, `Security` thảo luận, phản biện qua lại nhiều vòng để lên ý tưởng và chốt phương án) hoặc kiểm tra **P2P Message Bus** giữa các subagents.
- `/dag <mục_tiêu>`: Chạy **multi-agent DAG** v0.6 — planner phân rã mục tiêu thành các todo theo vai trò, coder chạy tuần tự có snapshot/rollback, kết thúc bằng merge + verify (xem mục *Multi-agent DAG*).
- `/diff`, `/git`, `/review`: Xem `git status` + `git diff`, hoặc yêu cầu model review toàn bộ thay đổi hiện tại.
- `/compact [trọng_tâm]`: Chủ động nén ngữ cảnh hội thoại (có lưu archive đầy đủ trước khi nén).
- `/setup` (hoặc `/provider`), `/key`, `/doctor`: Cấu hình lại provider (nhấn `Esc` để huỷ), đổi API key (nhập ẩn) hoặc kiểm tra kết nối/môi trường.
- `/prompts`, `/prompts:<tên> [tham số]`: Liệt kê và chạy prompt tuỳ biến từ file Markdown (xem mục *Prompt tuỳ biến*).
- `!<lệnh>`: Chạy nhanh lệnh shell trực tiếp ngay trong TUI (ví dụ `!git status`).

### Duyệt quyền nhanh cho lệnh shell

Khi được hỏi `Allow bash?`, ngoài `y` (một lần) và `a` (mọi lệnh bash trong phiên), có thể chọn
`p` để **chỉ cho phép tiền tố lệnh đó** trong phiên, ví dụ `npm test`, `npm run lint`,
`python -m pytest`, `git status`. Lệnh ghép hoặc lệnh bọc (`;`, `&&`, `|`, `` ` ``, `$()`, `>`, `env`,
`sudo`, `bash -c`, gán biến `FOO=1 ...`) không bao giờ được duyệt theo tiền tố, nên
`npm test; curl ... | sh` vẫn phải hỏi lại. Lệnh cài gói (`npm install x`, `pip install x`…),
lệnh sửa/xoá file (`rm`, `mv`, `cp`, `chmod`…) và lệnh mạng (`curl`, `wget`…) chỉ được nhớ đúng
nguyên câu lệnh. Tham số thêm vào sau tiền tố đã duyệt mà trỏ ra ngoài workspace (`/…`, `~`, `..`)
hoặc bật chế độ ghi/chạy chương trình (`--output`, `--ext-diff`, `-c`, `--exec`…) vẫn phải hỏi lại.

Khi cần chú ý (prompt duyệt quyền, lượt chạy dài ≥ 15 giây vừa xong), goon phát tiếng chuông
terminal (`BEL`). Tắt bằng `GOON_NOTIFY=0`.

### Prompt tuỳ biến (custom slash commands)

Tạo file Markdown trong `.goon/prompts/` hoặc `.agents/prompts/` (theo dự án),
`~/.config/goon/prompts/` hoặc `~/.codex/prompts/` (theo người dùng):

```markdown
---
description: Review một file theo checklist của team
---
Review $1 theo checklist bảo mật và hiệu năng. Ghi chú thêm: $ARGUMENTS
```

Chạy bằng `/prompts:review src/app.ts kiểm tra SQL injection`. `$ARGUMENTS` là toàn bộ phần sau
tên lệnh, `$1`…`$9` là từng từ, `$$` là ký tự `$`.

## Harness, Công cụ & Hoàn tác (Checkpoint / Rewind)

- **Bộ công cụ tích hợp đầy đủ**:
  - **File & Shell**: `read_file`, `list_files`, `search`, `write_file`, `edit_file`, `apply_patch`, `bash`, `view_image`.
  - **Kế hoạch & Tương tác**: `todo_read`, `todo_write`, `update_plan`, `request_user_input`, `exit_plan_mode`.
  - **Subagents (7 công cụ)**: Vòng đời tương tác (`spawn_agent`, `wait_agent`, `send_input`, `close_agent`, `list_agents`) và uỷ thác theo lô (`delegate_task`, `delegate_edit`).
  - **MCP Resources & Tools**: `list_mcp_resources`, `list_mcp_resource_templates`, `read_mcp_resource` cùng các tool động từ MCP server (`mcp__<server>__<tool>`).
- **Cơ chế Checkpoint & Rollback nhẹ, chính xác (`/rewind`, `/revert`, `/undo`)**:
  - Chỉ các file được sửa trực tiếp qua bộ công cụ chỉnh sửa (`write_file`, `edit_file`, `apply_patch` — bao gồm cả subagent worker được uỷ quyền sửa file) mới được ghi nhận bản sao lưu trước lượt chạy (trạng thái tồn tại, nội dung gốc và file mode) để rollback tức thì 100% và không tốn I/O quét toàn bộ workspace khi chạy lệnh shell (`bash`).
  - File bạn đã sửa tay **sau** lượt của agent sẽ không bị ghi đè khi revert: goon giữ nguyên và báo xung đột; dùng `/revert N --force` nếu thật sự muốn ghi đè.
  - Tổng dung lượng bản sao lưu mỗi phiên giới hạn 32 MiB (các lượt cũ nhất bị lược nội dung trước và được báo rõ khi revert), để file phiên không phình to vô hạn.
- **`apply_patch` đầy đủ định dạng Codex**: hỗ trợ `*** Move to:` (đổi tên/di chuyển), nhiều khối `Update File` cho cùng một file, file thêm mới luôn kết thúc bằng newline; patch chỉ được ghi khi **mọi** hunk đều khớp. File hiện có phải được đọc trước khi patch (chống revision cũ), hunk xuất hiện nhiều lần bị từ chối thay vì đoán, và cú pháp/JSON của kết quả được kiểm tra trước khi ghi.
- **Kiểm chứng sau khi sửa (verify hook, v0.4)**:
  - Sau mỗi batch có thao tác ghi/sửa file trong workspace, Harness chạy một bước kiểm chứng: kiểm tra cú pháp cục bộ (TS/JS, JSON) rồi các lệnh test/lint/typecheck được phát hiện hoặc cấu hình. Chỉ các file đã đổi trong lượt được kiểm tra.
  - Khi lệnh kiểm chứng fail, Harness chèn lỗi đã lọc secret vào lượt kế tiếp để model tự sửa (mặc định tối đa `2` vòng). Hết lượt sửa hoặc model kết thúc khi vẫn còn lỗi → **rollback byte-exact** các file của lượt (file mới bị xoá, file cũ được khôi phục) và lượt chạy kết thúc với mã thoát `1` (không phải `2`).
  - Rollback không ghi đè file bạn đã sửa tay sau agent; file xung đột được giữ lại và báo rõ, và vẫn nằm trong checkpoint để `/revert` xử lý tiếp.
  - Event mới: `verify_start` (files, commands), `verify_result` (status/stage/reason/errors/warnings/durationMs) và `verify_rollback`. Verdict và thông điệp được redact **trước** khi ghi vào session/event.
  - Lệnh kiểm chứng phải được duyệt như một tool (`verify`) trừ khi bạn đang ở `auto`/`--yes`; từ chối → `verify_result: skip`, **không** rollback. Đặt lệnh riêng bằng `--verify-cmd` (lặp lại được) hoặc `GOON_VERIFY_CMDS`; tắt hoàn toàn bằng `--no-verify`.
  - Subagent, workflow step và network agent là harness con: verify luôn tắt để tránh rollback chéo giữa các phiên.
  - Phạm vi v0.4: chỉ thay đổi qua `write_file`/`edit_file`/`apply_patch`. File do `bash` hoặc MCP sửa không được theo dõi/rollback.
- **Tự động Retry API**: Khi gặp lỗi mạng hoặc lỗi upstream tạm thời (HTTP `5xx`, `429`, `408`, lỗi đóng stream/socket bất ngờ), Harness tự động thử lại tối đa **3 lần** với exponential backoff + jitter, tôn trọng header `Retry-After` (tối đa 20 giây) trước khi dừng.
- **Đọc song song**: tối đa 6 lời gọi tool chỉ-đọc liên tiếp (`read_file`, `search`, `list_files`, MCP read-only…) chạy đồng thời; thao tác ghi và shell vẫn tuần tự theo đúng thứ tự model yêu cầu.
- **Hiển thị đầy đủ Unified Diff**: `computeUnifiedDiff` hiển thị trọn vẹn số dòng thêm/xoá thực tế (`+N -M`, hỗ trợ tới 5.000 dòng mỗi hunk).
- **Bảo vệ chống lỗi file `nul` trên Windows**:
  - Tự động chuyển hướng `>nul`, `2>nul`, `>>NUL`, `>/dev/nul` trong lệnh `bash` trên Windows về `/dev/null` an toàn (`sanitizeBashCommand`).
  - `WorkspaceFiles` từ chối tạo các tên thiết bị dành riêng của Windows (`nul`, `con`, `prn`, `aux`, `com1-9`, `lpt1-9`) trên mọi nền tảng để giữ an toàn cho Git repo đa nền tảng.
  - `unlinkSafe` hỗ trợ xoá cả các file tên thiết bị bị tạo nhầm trước đó thông qua đường dẫn mở rộng `\\?\`.
- **An toàn phiên & file**:
  - Lưu phiên atomic, lock tiến trình, tự động dọn phiên rỗng (`0 messages`) khi thoát hoặc đổi phiên.
  - Ghi file theo temporary file + `chmod` + `rename`, bảo toàn nguyên vẹn quyền file (`0755`, umask `027`/`007`).

## Bảo mật

- Approval chỉ dựa trên chế độ **hiện tại**: chuyển từ `auto`/`--yes` về `ask`/`plan` có hiệu lực ngay cho lời gọi tool kế tiếp.
- API key của goon (`GOON_API_KEY` và biến trong `keyEnv`) bị loại khỏi môi trường của mọi tiến trình con (`bash`, `!cmd`, `rg`, `git`), nên model hoặc prompt-injection không thể `env`/`curl` để lấy key. Nếu dự án của bạn cần chính biến đó khi chạy test, hãy dùng một biến khác cho goon (`--key-env`).
- Tool đọc ảnh do model gọi (`view_image`, `@ảnh` trong prompt của subagent) chỉ đọc trong workspace; ảnh ngoài workspace cần bạn duyệt. Ảnh bạn tự kéo/thả hoặc dán vẫn đọc được ở bất kỳ đâu.
- Tool bị ẩn ở chế độ hiện tại (ví dụ `view_image`, subagent trong `--read-only`) không thể bị gọi "chui" bằng tên.
- API key luôn nhập ẩn (`goon setup`, `/setup`, `/key`); `goon doctor` cảnh báo `credentials.json` có quyền quá rộng và base URL `http://` không phải loopback (key bị gửi dạng rõ).
- Lưu ý: approval và network policy **không phải sandbox của hệ điều hành**; lệnh shell đã duyệt chạy với quyền của user hiện tại.
- **Network default-deny (v0.5)**: mọi lệnh `bash`/workflow shell được phân loại là network (`curl`, `wget`, `ssh`, `git push/pull/fetch/clone`, `npm install`, …) bị **từ chối trước bước approval** khi chưa cấp quyền. Bật toàn bộ bằng `--allow-net` (hoặc `GOON_ALLOW_NET=1`, `"allowNet": true`); chỉ mở một số host bằng `--allow-net-hosts=api.example.com` (lặp lại hoặc phân tách bằng dấu phẩy) hoặc `"allowNetHosts": [...]`. Host so khớp **chính xác** (không wildcard/hậu tố); lệnh network không xác định được host (ví dụ `git push origin`, `npm install`) bị từ chối theo kiểu fail-closed. Policy không hạ mức rủi ro: host được allowlist vẫn cần approval như thường.
- **MCP annotations là dữ liệu không tin cậy**: chúng chỉ có thể *nâng* rủi ro (`destructiveHint`/`openWorldHint` → high; `openWorldHint` đánh dấu network). `trust: true` chỉ tự động duyệt tool rủi ro medium/low, **không** biến tool thành read-only và không hạ risk; chỉ `readOnlyTools` (do bạn đặt) mới đưa tool về low và mới expose tool trong `--read-only`.
- Lưu ý: trong chế độ `auto`/`--yes`, các lệnh package-manager "an toàn" như `npm test`, `npm run build`, `npm run lint` được xếp **low** và chạy không hỏi. **Quyết định rủi ro đã chấp nhận (known issue 7)**: `npm test` thực thi cả lifecycle script (`pretest`/`posttest`) mà model có thể sửa trong phiên (`edit_file` là medium, cũng auto-approve), nhưng auto mode vốn đã tự động chạy `node <script>.js` (medium) nên mức rủi ro dư là tương đương và được chấp nhận rõ ràng. Điều **không bao giờ** được thừa hưởng trust đó: script tên lạ (`npm run pretest`, `npm run deploy` → medium), lệnh đổi hướng bằng flag (`npm --prefix <dir> test`, `yarn --cwd <dir> test` → medium, không bao giờ low), thực thi tuỳ ý (`npm exec`, `npx`, `pnpx` → high), và mọi chuỗi nối thêm segment network/phá hủy (`&& curl …`, redirect → **high**, không bao giờ tự chạy và không được nhớ vào allowlist phiên). Có test red-team khóa toàn bộ các biến thể này (`tests/risk.test.mjs`).

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
      "readOnlyTools": [],
      "net": false,
      "allowNet": false
    }
  }
}
```

- `net: true` đánh dấu **toàn bộ tool của server** là network; chúng bị execution policy chặn trước approval. `allowNet: true` (hoặc `--allow-net` toàn cục) cho phép server này dùng network dù policy mặc định deny.
- `annotations` do server khai báo (`destructiveHint`, `openWorldHint`, `readOnlyHint`) chỉ được phép nâng rủi ro; xem mục Bảo mật.

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

## Multi-agent DAG (v0.6)

- **`/dag <mục_tiêu>`** (hoặc `goon dag <mục_tiêu>`): phân rã mục tiêu thành một DAG todo theo vai trò
  (`planner`/`coder`/`reviewer`/`tester`/`researcher`) rồi thực thi với các subagent:
  - Planner chạy read-only và trả về JSON plan; plan bị kiểm tra cấu trúc (id duy nhất, `dependsOn`
    tồn tại, không tự phụ thuộc, không có chu trình, role hợp lệ) — plan lỗi phát sự kiện
    `dag_plan_invalid` và **không spawn** gì cả.
  - Todo chỉ được chạy khi **tất cả** dependency `done`; dependency `failed`/`blocked` làm todo
    phụ thuộc `blocked` (lan xuống toàn bộ chuỗi). Todo ghi file (`coder`) chạy **tuần tự**;
    role chỉ đọc chạy song song (giới hạn `multiAgentMaxConcurrency`, mặc định 2).
  - **Snapshot theo todo**: trước mỗi todo ghi file, workspace được snapshot; review bị từ chối
    hoặc test thất bại → rollback rồi rework (tối đa `multiAgentReworkCap`, mặc định 3 lần, vượt cap
    → todo `failed`). Plan fail/huỷ/verify không đạt → rollback toàn bộ theo thứ tự ngược và không
    bao giờ đè lên thay đổi của người dùng (file conflict được giữ nguyên, todo chỉ `failed`).
  - **Merge chỉ tổng hợp, không ghi đè**: hai todo `done` cùng sửa một file với diff khác rỗng →
    `MergeConflict`; combined diff rỗng và không có artifact → `verified: false` ("no changes").
  - **Cổng verify cuối**: chạy `runVerification` trên workspace với lệnh trong khối `verify`;
    lệnh luôn đi qua callback duyệt quyền (auto-approve ở chế độ `--yes`). Verify không đạt →
    rollback toàn bộ + `ErrNotVerified`.
- **Toolset theo vai trò được pin ở registry con**: `planner`/`reviewer`/`researcher` chỉ đọc;
  `coder` có thêm `write_file`/`edit_file`/`apply_patch`/`bash`; `tester` có `bash` (vẫn phải duyệt
  quyền như mọi nơi — approval không phải sandbox). MCP **không** được expose cho DAG roles trừ khi
  liệt kê trong `multiAgentMcpTools` (`mcp__server__tool` hoặc wildcard `mcp__*`); tool bị ẩn thì
  model gọi trực tiếp cũng bị `execute()` từ chối. `delegate_task`/`delegate_edit` pin đúng toolset
  của registry cha — input `allowedTools` từ model bị bỏ qua (không tự nâng quyền).
- **Ngân sách dùng chung**: mọi spawn (và cả planner) dùng chung một `RunBudget`/`CostLedger` với
  phiên cha; `multiAgentMode: "off"` trong config tắt hoàn toàn `/dag`.
- Sự kiện mới (additive): `dag_plan_invalid`, `dag_plan_ready`, `dag_todo_update`, `dag_merge`.

## Workflow động, scope loop & multi-candidate (v0.7)

- **Classifier workflow thuần TS (không gọi model)**: `classifyWorkflowComplexity(prompt, context)`
  trong `src/workflow.ts` chấm điểm tín hiệu — số động từ mệnh lệnh (kể cả tiếng Việt: thêm/sửa/cập
  nhật/kiểm tra/…), chuỗi nối tuần tự ("then", "sau đó", "after that", "tiếp theo"), số path được
  nhắc tới và từ khoá "plan/workflow/multi-step/pipeline". **Bảo thủ theo mặc định**: thiếu tín hiệu
  rõ → `single`, nên workflow cũ tuyến tính chạy y nguyên.
- **Bước `dag` trong workflow**: file workflow (`.goon/workflows/*.md|*.json`) nhận step type mới
  `dag` (markdown: `## Step: Decompose [dag]`). Bước này chỉ thực sự chạy multi-agent DAG
  (hook `runDag` — CLI nối vào cùng lõi `/dag`) khi **cả ba** điều kiện đạt: classifier trả `multi`,
  `multiAgentMode !== "off"`, và hook `runDag` khả dụng. Ngược lại bước chạy như một prompt tuyến
  tính. Mỗi lần phân loại phát sự kiện additive `workflow_classified { mode, reason }`.
- **Scope loop** (`runScopeLoop` trong `src/coord.ts`): vòng lặp sửa-lỗi dẫn đường bởi verify —
  attempt → verify → fail thì **mở rộng** ngữ cảnh (output verify đã redact + file liên quan + lệnh)
  cho vòng sau, pass thì **thu hẹp**. Tối đa `min(verify.maxRepairRounds + 1, 4)` vòng; dừng khi
  pass, không còn thay đổi, chạm trần token/cost/time của `RunBudget` dùng chung (kiểm tra trước
  mỗi attempt), hoặc hết vòng. Verify luôn qua `runVerification` với callback `approve` nhận đúng
  lệnh hiển thị — có lệnh mà thiếu callback → `VerificationApprovalError` (fail-closed); plan chỉ
  AST vẫn chạy không cần duyệt. Terminal fail → rollback toàn bộ snapshot theo §5.1 (không đè
  thay đổi của người dùng). Sự kiện additive: `scope_round { round, action: expand|contract|stop }`.
- **Multi-candidate reflection** (`runCandidates`, `reflect`, `rerank`, `costDegrade` trong
  `src/coord.ts`): mỗi candidate chạy cô lập trên snapshot riêng (`beginTurn`/`takeTurnBackups`),
  verify xong — fail thì rollback ngay để candidate sau khởi từ cùng trạng thái gốc, candidate
  đầu tiên không fail được giữ lại (winner) và snapshot của nó được merge vào cửa sổ turn để
  checkpoint vẫn revert được. **Cap 2 candidate** (`MAX_CANDIDATES`); khi còn dưới 20% ngân sách
  cost/time → còn **1 candidate** kèm yêu cầu hạ effort/model (hook nhận `degraded`) và phát sự
  kiện `cost_degrade { reason, candidates }`. `rerank` xếp thứ tự deterministic: verify pass >
  ít warning hơn > cost thấp hơn > thứ tự sinh.
- **Embedder seam (P24)**: config `embedderProvider` (`hash`|`openai`|`ollama`, mặc định `hash`
  — embedder offline deterministic không đổi), `embedderModel`, `embedderBaseUrl`; env
  `GOON_EMBEDDER_PROVIDER`, `GOON_EMBEDDER_MODEL`, `GOON_EMBEDDER_BASE_URL`. Provider `openai`
  (endpoint `/embeddings` OpenAI-compatible, mặc định dùng `baseUrl` của gateway) gửi API key
  **chỉ** trong header `Authorization` — key không bao giờ vào log, message lỗi (body được scrub),
  env con hay request tới `ollama` (local, không key). Lỗi embedder degrade về `memory_error`,
  không bao giờ làm hỏng run.

## Event log, replay & stub provider (v0.5)

- **Bật ghi log**: `--event-log` (hoặc `GOON_EVENT_LOG=1`, hoặc `"eventLog": true` trong
  `config.json`). Log là JSONL append-only tại `sessions/<id>.events.jsonl`, mỗi dòng là một
  envelope `{v, seq, at, sessionId, type, event}`. Ghi xong từng dòng được `fsync`; file có mode
  `0600` trên POSIX (Windows không có mode bit nên kế thừa ACL của thư mục).
- **Khôi phục tuần tự**: số `seq` được lưu kèm trong `<log>.seq` và tự dựng lại bằng cách quét
  tail (64 KiB, nhân đôi tới 1 MiB; dòng lớn hơn đọc toàn file). Dòng cuối bị ghi dở được bỏ qua
  (`truncated`), không làm hỏng replay. Thiếu/lệch sidecar sẽ tự sửa từ nội dung log.
- **Redaction**: text/reasoning stream được giữ lại một tail để secret bị chia giữa hai chunk vẫn
  bị thay bằng `[REDACTED]`; mọi string field được redact trước khi ghi, khóa nhạy cảm
  (`apiKey`, `token`, `authorization`, …) bị thay toàn bộ.
- **`goon replay <ID|tiền tố|đường dẫn log>`** in lại từng event thô (mặc định và `--json` giống
  nhau); `--envelopes` in cả envelope; cảnh báo ra stderr nếu `seq` bị nhảy nhưng không dừng.
  Replay không cần model/API key.
- **`goon replay ... --rebuild-session`** dựng `<id>.replayed.json` (không ghi đè session gốc) từ
  event `session` đầu, các `user`, `done`/`error` cuối. Log bị cắt trước `done` vẫn dựng được
  phần user (partial). Chạy lại cho cùng nội dung.
- **Xoá phiên** (`/delete` hoặc `deleteSession`) xoá luôn `.events.jsonl`, `.seq` và file tạm.
- **Stub provider**: `--stub` dùng `LanguageModelV3` tất định, không mạng và không đọc API key;
  `--stub-fixture FILE` nạp kịch bản JSON (tối đa 1 MiB, validate bằng zod). Mỗi turn chọn theo
  `match`/`fixture.input`, fallback `auxiliary`, hết kịch bản trả `[stub] script exhausted`.
  Fixture hỗ trợ `reasoning`, `text[]`, `toolCalls[]`, `usage` và `error` (ném `APICallError`).
  Đây là công cụ test/CI; không dùng cho phiên thật.

```json
{
  "v": 1,
  "model": "stub-model",
  "input": "hello",
  "turns": [
    { "match": "hello", "text": ["Xin chào từ stub."],
      "usage": { "inputTokens": 10, "outputTokens": 5, "totalTokens": 15 } }
  ],
  "auxiliary": { "text": ["[stub] phụ trợ"] }
}
```

## Memory & lexical RAG (v0.6, opt-in)

- **Tắt theo mặc định**: `memoryEnabled`/`ragEnabled` mặc định `false` (bật bằng config hoặc
  `GOON_MEMORY_ENABLED=1`, `GOON_RAG_ENABLED=1`). Các lệnh `/memory` tự mở memory store
  on-demand nên dùng được ngay cả khi chưa bật. Khi `memoryEnabled=1`, harness mở memory store
  cho cả phiên và bơm retrieval vào prompt dưới dạng khối untrusted (xem mục *Context budget &
  memory wiring*); `ragEnabled` chỉ quyết định việc tự index repo, không ảnh hưởng lệnh `/memory`.
- **6 loại memory**: `working` (RAM mỗi lượt chạy), `conversation` (view trực tiếp các message
  của session, không retrieval lại), `exec` (tối đa 50 bài học ngắn gọn theo dự án, không chèn
  raw output), `repo` (cache tóm tắt cây thư mục), `knowledge` (chỉ mục lexical của workspace +
  insight), `preference` (key/value theo người dùng trong `preferences.json`, **tách khỏi**
  `memories.json`). Mọi dữ liệu ghi đĩa đều redact secret trước (`redactSecrets`: literal đã
  biết, bearer, `key=value`/JSON, và các env var trông như credential
  `*_API_KEY|*_TOKEN|*_SECRET|*_PASSWORD` được gom tự động qua `collectSecrets()` — cùng hợp
  đồng với event log), ghi tuần tự
  qua một promise chain (single-writer) + atomic write, nên gọi `handleEvent`/`persist` đồng thời
  không bao giờ ghi hỏng file.
- **RAG lexical**: đây là lexical (hashed term-frequency) dùng FNV-1a, 384 chiều, chạy offline và
  tất định — **không phải semantic search**. Chunk theo hàm/class (TS/JS/Go/Python, nhận diện
  bằng regex, fallback cửa sổ 40 dòng overlap 8; soft 1500 / hard 4000 ký tự). Index trong RAM
  có LRU eviction theo `ragCapacity` (mặc định 5000 chunk), trả top-k `ragTopK` (mặc định 10)
  với ngưỡng `ragThreshold` (mặc định 0). Snapshot `rag.json` lưu theo dự án tại
  `<data>/memory/<projectId>/` với `projectId` = 16 hex đầu của sha256(cwd); khi load lại vector
  được tính lại từ chunk (không lưu vector trên đĩa).
- **An toàn khi index**: bỏ qua symlink và mọi file có real path ra ngoài workspace, file lớn hơn
  `ragIndexMaxFileBytes` (mặc định 1 MiB) hoặc quá `ragIndexMaxFiles` (mặc định 2000 file), file
  binary, thư mục mặc định (`.git`, `node_modules`, `dist`, `build`, `vendor`, `coverage`, `venv`,
  `__pycache__`, `target`, …), đường dẫn nhạy cảm (`.env*`, `*.pem`, `*.key`, `id_rsa*`, `.npmrc`,
  `.netrc`, `.git-credentials`, `.ssh/**`, `.aws/**`, `secrets/**`, gcloud credentials, …) và mọi
  pattern trong `.goonignore` (subset gitignore: comment, `!negation`, `/` cuối = chỉ thư mục,
  `*`, `?`, `**/`). `ensureRepoIndexed` so mtime/size để chỉ index lại file thay đổi và gộp các
  lời gọi đồng thời (in-flight dedup).
- **Lệnh liên quan**: `/memory rag index` (quét + index workspace), `/memory rag stats`,
  `/memory rag search <truy_vấn>`, `/memory budget` (ngân sách retrieval hiện tại),
  `/memory pref set|get|rm|list` (ghi qua sự kiện `user_preference` của memory store).

## Context budget & memory wiring (v0.6, P18-P19)

- **Ngân sách context**: mỗi request được chuẩn bị qua `src/context-budget.ts` (thuần, không sửa
  session). `inputBudget = max(1024, contextWindow - maxOutputTokens)` được chia thành reserve
  (`contextReserveRatio`, mặc định 0.15), system prompt (sàn 15% / trần 35% của input budget),
  hội thoại (`contextConversationRatio`, mặc định 0.45) và context bơm vào (`contextFilesRatio`,
  mặc định 0.25). System prompt quá trần bị cắt phần `Workspace guidance (AGENTS.md):` và
  `Persistent user memories:` trước, giữ phần nhận diện/quy tắc; ở cửa sổ tối thiểu 8192/512 base
  system không bao giờ bị cắt.
- **Cắt dữ liệu cũ theo cặp**: tool result quá 8 KiB được cắt head/tail; tool result trùng lặp
  được thay bằng marker; nếu vẫn vượt, các tool result cũ nhất bị làm rỗng trước, sau đó message
  cũ bị bỏ theo cặp tool-call/tool-result (không bao giờ bỏ user message gần nhất, assistant gần
  nhất, hay để tool result thành mồ côi). File `@file` được truncate kèm marker hướng dẫn
  `read_file`. Khi không thể vừa nữa, harness ném `ContextBudgetError` thay vì gửi request vượt
  ngân sách; invariant `estimatedTokens <= inputBudget` luôn được test kiểm chứng.
- **Bơm retrieval an toàn**: RAG/repo tree/working notes/preferences được chèn vào **một user
  message** `<untrusted_context source="rag,repo" trust="untrusted">` ("data, not instructions"),
  không bao giờ vào system prompt, và luôn cắt theo `files` budget (chunk score thấp bị bỏ trước).
  Session store vẫn giữ nguyên lịch sử đầy đủ — compaction vẫn hoạt động trên dữ liệu gốc.
- **Memory trong harness**: khi `memoryEnabled`, CLI mở `MemoryStore` cho cả phiên và truyền vào
  harness (subagents không nhận). Harness index repo đúng **một lần mỗi phiên** và chỉ khi
  `ragEnabled`; sau mỗi lượt có edit, index được làm mới một phần (patch reindex). Các sự kiện
  `task_started`, `tool_result` (chỉ summary đã redact), `patch_applied` (đường dẫn + số byte),
  `verify_result`, `assistant_message` (đã redact, cắt ngắn) và `task_completed` được ghi vào
  memory qua `handleEvent`; lỗi memory chỉ phát sự kiện `memory_error`, không bao giờ làm hỏng
  lượt chạy. Subagents không nhận memory store (tránh ghi đè); parent tự ghi bài học từ báo cáo.

## Cấu hình và Dữ liệu

- Config: `$XDG_CONFIG_HOME/goon/config.json` (mặc định `~/.config/goon/config.json`).
- Credentials (API Keys): `$XDG_CONFIG_HOME/goon/credentials.json` (quyền `0600`).
- Memories & Presets: `$XDG_CONFIG_HOME/goon/memories.json`, `$XDG_CONFIG_HOME/goon/presets.json`,
  `$XDG_CONFIG_HOME/goon/preferences.json` (memory store v0.6).
- Sessions: `$XDG_DATA_HOME/goon/sessions/` (mặc định `~/.local/share/goon/sessions/`).
- Memory store (v0.6, opt-in): `<data>/memory/<projectId>/memory.json` + `rag.json`
  (`projectId` = 16 hex đầu của sha256(workspace cwd)).
- Event logs: `SESSION_UUID.events.jsonl` (+ `.seq`) trong cùng thư mục khi bật `--event-log`.
- Archives: `SESSION_UUID.compact-N.json` trong cùng thư mục session.
- Ưu tiên: cờ CLI > biến `GOON_*` > config người dùng > mặc định.

Các biến môi trường hỗ trợ: `GOON_PROVIDER`, `GOON_BASE_URL`, `GOON_MODEL`, `GOON_EFFORT`,
`GOON_KEY_ENV`, `GOON_API_KEY`, `GOON_CONTEXT_WINDOW`, `GOON_MAX_OUTPUT_TOKENS`,
`GOON_TIMEOUT_MS`, `GOON_RUN_TOKEN_BUDGET`, `GOON_MAX_COST_USD`, `GOON_MAX_TIME_MS`, `GOON_PRICING_JSON`, `GOON_INLINE_REASONING`, `GOON_NOTIFY` (`0` để tắt chuông).
Execution policy: `GOON_ALLOW_NET` (`1` để mở toàn bộ network), `GOON_ALLOW_NET_HOSTS` (danh sách host chính xác, phân tách bằng dấu phẩy).
Event log / stub: `GOON_EVENT_LOG` (`1` để bật), `GOON_STUB` (`1` để dùng stub provider), `GOON_STUB_FIXTURE` (đường dẫn JSON fixture).
Memory/RAG (v0.6): `GOON_MEMORY_ENABLED`, `GOON_RAG_ENABLED` (`1` để bật), `GOON_RAG_TOP_K`, `GOON_RAG_CAPACITY` (số nguyên).
Multi-agent DAG (v0.6): config `multiAgentMode` (`off`|`manual`|`auto`, mặc định `manual`; `off` tắt `/dag`),
`multiAgentReworkCap` (mặc định 3), `multiAgentMaxConcurrency` (mặc định 2), `multiAgentMcpTools`
(danh sách tool MCP cho DAG roles, ví dụ `["mcp__fs__read"]` hoặc `"mcp__*"`).
Embedder (v0.7): `GOON_EMBEDDER_PROVIDER` (`hash` mặc định | `openai` | `ollama`),
`GOON_EMBEDDER_MODEL`, `GOON_EMBEDDER_BASE_URL` — provider `openai` cần model (và URL nếu khác
gateway chính); key chỉ dùng cho header `Authorization`, không log và không gửi tới `ollama`.
Verify: `GOON_VERIFY_ENABLED`, `GOON_VERIFY_CMDS` (mảng JSON hoặc danh sách phân tách bằng dấu phẩy),
`GOON_VERIFY_TIMEOUT_MS`, `GOON_VERIFY_MAX_REPAIR_ROUNDS`, `GOON_VERIFY_REQUIRE_TESTS`,
`GOON_VERIFY_REQUIRE_LINT`, `GOON_VERIFY_REQUIRE_TYPECHECK`, `GOON_VERIFY_ALLOW_WARNINGS`.

Khối `verify` trong `config.json` (mặc định bật, tự phát hiện lệnh):

```json
{
  "verify": {
    "enabled": true,
    "commands": ["npm test", "npm run typecheck"],
    "timeoutMs": 120000,
    "maxRepairRounds": 2,
    "policy": { "requireTests": false, "requireLint": false, "requireTypecheck": false, "allowWarnings": true }
  }
}
```

- Prompt tuỳ biến: `.goon/prompts/*.md`, `.agents/prompts/*.md`, `$XDG_CONFIG_HOME/goon/prompts/*.md`, `~/.codex/prompts/*.md`.
- Chỉ mục phiên: `sessions/.index.json` (cache tóm tắt theo kích thước + mtime, tự dựng lại khi hỏng hoặc bị xoá).

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
(umask 027; node --test --test-concurrency=4 "tests/*.test.mjs")   # POSIX only
(umask 007; node --test --test-concurrency=4 "tests/*.test.mjs")   # POSIX only
npm run build
npm install -g . --omit=dev
```

CI chạy matrix 3 hệ điều hành (Ubuntu, Windows, macOS) × Node 22.16/24. Trên Windows,
các test chỉ có ý nghĩa POSIX được skip: mode bit/umask (`tests/files.test.mjs`) và shim
`gh` trong `tests/publish.test.mjs`; PTY test trong `tests/tui.test.mjs` cũng skip. Dấu
ngoặc kép quanh glob để Node tự mở rộng trên cmd.exe/PowerShell.

`--test-concurrency=4` giới hạn số file test chạy song song: mỗi file là một process Node
riêng, và trên Windows chạy không giới hạn (7-8 process cùng lúc) có thể cạn commit charge
của hệ thống — process test chết với `FATAL ERROR: Committing semi space failed`, kéo theo
các test còn lại của file đó không chạy. Giới hạn 4 giữ nguyên độ phủ, chỉ chậm hơn không
đáng kể.

Smoke test Termux (chạy thủ công trên thiết bị, không thuộc CI):

```bash
bash scripts/smoke-termux.sh              # npm ci + build + eventlog/stub-provider/golden
bash scripts/smoke-termux.sh --no-install # dùng lại node_modules hiện có
```

`.gitattributes` chuẩn hoá LF cho source và đánh dấu binary; script `.bat`/`.cmd`/`.ps1`
giữ CRLF.

| File | Trách nhiệm |
|---|---|
| `src/harness.ts` | Agent loop, retry API, điều phối tool, checkpoint, `/btw`, usage, cancellation |
| `src/provider.ts` | Chọn adapter API, quét `/models`, truyền `reasoningEffort` / thinking budget |
| `src/setup.ts` | Setup wizard tương tác (`Esc` để huỷ), quét `/models` động, lưu `credentials.json` |
| `src/tools.ts`, `src/approval.ts` | Registry, schema, alias chuẩn hoá, `sanitizeBashCommand`, approval và duyệt theo tiền tố lệnh |
| `src/prompts.ts`, `src/help.ts` | Prompt tuỳ biến `/prompts:<tên>`; nội dung `--help` (nạp nhanh, không kéo SDK) |
| `src/files.ts`, `src/process.ts` | File I/O, unified diff, patch, backup cho /undo, bảo vệ `nul`, subprocess và lọc biến môi trường bí mật |
| `src/session.ts` | Lock, checkpoint, rewind, resume và sửa lịch sử bị ngắt |
| `src/context.ts` | System prompt, `AGENTS.md`, `SKILL.md`, `memories.json`, `presets.json` và compaction |
| `src/cli.ts`, `src/tui.ts` | Flags, giao diện TUI, Transcript Mode (`Ctrl+T`), popup `/slash` |
| `src/mcp.ts` | MCP client, stdio/HTTP transport, resources, `import-codex`, lifecycle |
| `src/subagents.ts` | `spawn_agent`/`wait_agent` & `delegate_task`/`delegate_edit`, concurrency, budget |
| `src/network.ts` | `AgentNetwork` (Brainstorm / Council Room đa vòng) & `AgentNetworkBus` (P2P `peer_send`, `peer_broadcast`, `peer_read`, `peer_list`) |
| `src/workflow.ts` | Custom Workflow engine (`.json` & `.md`), biến nội suy (`$input`, `$1`, `$prev`), bước `prompt`/`shell`/`subagent`/`network`/`approval` |
| `src/pair.ts` | Ghép đôi thiết bị qua Cloudflared Tunnel miễn phí, bộ tạo mã QR ISO/IEC 18004 thuần TypeScript và giao diện Mobile Web/SSE |
| `src/reasoning.ts` | `withReasoningReplay` (`reasoning_content`), `AnswerProjection`, giữ transcript gốc |
| `src/risk.ts`, `src/cost.ts` | Phân loại rủi ro shell/MCP (low→critical), ledger chi phí, trần token/cost/time mỗi lượt |
| `src/policy.ts` | Execution policy (v0.5): network default-deny/allowlist, host extraction (peel `sudo`/`env`/`time`), giới hạn output/timeout dùng chung |
| `src/verify.ts`, `src/syntax.ts` | Verify hook sau edit (syntax/lint/test/typecheck), redaction, rollback byte-exact |
| `src/eventlog.ts`, `src/redact.ts` | Event log JSONL append-only (envelope/seq sidecar/torn tail), replay, redaction dùng chung |
| `src/stub-provider.ts`, `src/stub-fixtures.ts` | Stub `LanguageModelV3` tất định, fixture zod (tối đa 1 MiB), không mạng/không API key |

## Repository & Tự động Release

- Source: [binxgtl/goon-cli](https://github.com/binxgtl/goon-cli)
- Bản cài và checksum: [GitHub Releases](https://github.com/binxgtl/goon-cli/releases)

Mỗi lần push lên `main` (trừ khi commit có `[skip ci]`), workflow `Check and release`
sẽ chạy toàn bộ typecheck, lint, test (Node 22.16 & Node 24 + restrictive umasks),
tự động tăng phiên bản patch, đóng gói `.tgz` kèm `SHA256SUMS.txt` và phát hành
GitHub Release mới.
