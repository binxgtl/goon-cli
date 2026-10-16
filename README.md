# goon cli

CLI coding agent độc lập, ưu tiên Termux, dùng API của bạn. Tham khảo kiến trúc
ZCode và DeepSeek Harness; **không fork base và không dùng runtime của hai dự án**.

## Cài trên Termux

Cài bản GitHub Release mới nhất:

```bash
pkg install nodejs-lts git bash ripgrep
node --version
npm install -g https://github.com/binxgtl/goon-cli/releases/latest/download/goon-cli.tgz --omit=dev
goon --version
```

Cần Node **22.16 trở lên**; Node 24 cũng được. Nếu `nodejs-lts` của môi trường
hiện tại thấp hơn, dùng gói `nodejs` đáp ứng phiên bản này. Bản npm đã chứa JS
biên dịch sẵn; không cần compiler, pnpm, proot, glibc hay addon native. Thiếu
`ripgrep` vẫn có search JavaScript dự phòng giới hạn kích thước.

Runtime dependencies được tải từ npm khi cài; gói này không kèm `node_modules`.
Hãy làm việc trong `$HOME` của Termux để có đầy đủ quyền file và executable bit.

## API ngoài, không đăng nhập tài khoản

Ví dụ dưới dùng placeholder; thay URL và model theo dịch vụ bạn đang có:

```bash
goon init \
  --provider openai-compatible \
  --base-url 'https://YOUR_PROVIDER/v1' \
  --model 'YOUR_MODEL_ID'

# Nhập key ẩn, tránh ghi key vào lịch sử lệnh bash:
read -rsp 'API key: ' GOON_API_KEY; echo
export GOON_API_KEY

cd "$HOME/your-project"
goon doctor
goon
```

Key được đọc từ biến môi trường; `init` chỉ lưu tên biến, không lưu giá trị key.
Muốn dùng biến khác, thêm `--key-env TEN_BIEN`.

| Provider | `--provider` | Base URL |
|---|---|---|
| Dịch vụ OpenAI-compatible | `openai-compatible` | API root trước `/chat/completions`, thường kết thúc `/v1` |
| OpenAI Responses-compatible | `openai-responses` | API root trước `/responses` |
| Anthropic Messages-compatible | `anthropic` | API root trước `/messages`, thường kết thúc `/v1` |

Không tự thêm `/v1` cho URL của bạn: một số gateway dùng đường dẫn riêng.
Nếu không đặt URL, provider Anthropic dùng `https://api.anthropic.com/v1`, hai
provider còn lại dùng `https://api.openai.com/v1`. Không cần acc Claude/Z.ai;
bạn cần key hợp lệ cho endpoint đã cấu hình. Endpoint phải hỗ trợ tool calling.

## Dùng

```bash
goon -p 'Đọc repo, tìm nguyên nhân lỗi và sửa; chạy kiểm tra phù hợp'
goon --read-only -p 'Đọc và giải thích kiến trúc repo'
goon --resume latest
goon --resume SESSION_UUID -p 'Tiếp tục phần đang làm'
goon --yes -p 'Sửa lỗi và chạy test'
goon --json -p 'Kiểm tra dự án'
printf '%s' 'Giải thích các file chính' | goon --read-only
goon sessions
```

Mặc định yêu cầu chấp thuận khi sửa/ghi file hoặc chạy shell. Không có TTY thì
những yêu cầu này bị từ chối; dùng `--yes` khi chủ động cho phép. `--read-only`
loại bỏ write/edit/bash khỏi bộ tool. File tools chỉ truy cập workspace đã chọn;
shell chạy với quyền tài khoản hiện tại, **không có sandbox hệ điều hành**.

Khi chạy `goon` trong terminal, TUI toàn màn hình hiện trạng thái tool/agent,
token và ô nhập. Có `/help`, `/tools`, `/status`, `/exit`; phím lên/xuống gọi lại
prompt, PgUp/PgDn cuộn, Ctrl+C huỷ lượt đang chạy, Ctrl+D thoát khi ô nhập trống.
Dùng `--no-tui` để về readline; pipe, `-p`, `--json` và `TERM=dumb` dùng đầu ra thường.
`--json` xuất NDJSON gồm session, text, tool, usage, compaction, subagent, done/error.
Mã thoát: 0 thành công, 1 lỗi, 2 từ chối quyền, 130 Ctrl+C, 143 SIGTERM.

## Harness có gì

- Vòng lặp model → tool → result → model, giữ reasoning và provider metadata
  thông qua AI SDK; xử lý stream xong mới chạy tool.
- Sáu tool: `read_file`, `list_files`, `search`, `write_file`, `edit_file`, `bash`.
- Hai tool đọc liên tiếp có thể chạy song song. Ghi file và shell là hàng rào
  tuần tự; kết quả giữ thứ tự model, kể cả khi bị huỷ.
- Lưu phiên atomic, lock tiến trình, resume theo workspace. Lượt tool dở dang
  được đánh dấu kết quả chưa rõ; không tự lặp lại side effect sau crash.
- Edit/overwrite phải đọc file trước và kiểm tra revision để tránh ghi đè thay
  đổi mới. Ghi file theo temporary file + chmod + rename, giữ đúng mode file cũ dù
  umask là `027`/`007`; file mới vẫn theo umask.
- Compact context có archive đầy đủ trước khi thay lịch sử đang hoạt động.
  Đọc `AGENTS.md` tại gốc workspace, tối đa 24.000 ký tự.
- Output tool giới hạn; shell có timeout, huỷ process group trên POSIX. Search
  JavaScript dự phòng bỏ `.git`, `node_modules`, symlink và giới hạn quét.
- Không có telemetry hoặc tự cập nhật do Goon triển khai. Request model vẫn gửi
  prompt, ngữ cảnh và kết quả tool tới API bạn chọn. Transcript lưu những dữ liệu
  đó trên máy; không coi transcript là đã được rà soát toàn bộ bí mật.

## MCP

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

Đặt token trong biến `MY_MCP_TOKEN`, rồi chạy:

```bash
goon mcp list    # xem cấu hình, không kết nối
goon mcp check   # kết nối và in tên tool gốc/tên namespaced
goon            # tự nạp các server đã bật
goon --no-mcp    # bỏ qua MCP trong lượt này
```

Mỗi tool MCP mặc định cần duyệt quyền. Chỉ đưa tên tool gốc mà bạn tin là chỉ đọc
vào `readOnlyTools` để bỏ bước duyệt và cho phép trong `--read-only`. Không tự tin
`readOnlyHint` của server. Tên tool có namespace và hash để tránh trùng.
Xem [docs/MCP.md](docs/MCP.md) cho stdio, biến môi trường, giới hạn và cấu hình mẫu.
MCP hiện chỉ tích hợp tools; chưa có resources, prompts, OAuth hay SSE cũ.

## Subagent

Hai tool mới được bật mặc định:

- `delegate_task`: chia tối đa bốn việc đọc/review, tối đa hai agent chạy đồng thời.
- `delegate_edit`: chia tối đa bốn việc sửa, chạy tuần tự; duyệt cả yêu cầu giao
  việc và từng thao tác ghi/shell của agent con. `--yes` cũng áp dụng cho agent con.

```bash
goon --read-only -p 'Dùng delegate_task để xem cấu trúc repo và tìm rủi ro trong test, rồi tổng hợp'
goon -p 'Dùng delegate_edit để sửa lỗi trong src/files.ts và bổ sung regression test'
goon --no-subagents -p 'Đọc repo và làm trực tiếp'
```

Agent con có context và transcript riêng, cùng model/workspace, không nhận toàn
bộ hội thoại cha, không có MCP và không sinh thêm agent. Công cụ giao việc phải
truyền đủ ngữ cảnh. Mỗi con có budget tối đa 30.000 token và cùng tiêu budget
lượt chạy của cha; Ctrl+C huỷ và chờ tất cả agent đang chạy dọn phiên.
Transcript nằm ở `$XDG_DATA_HOME/goon/subagents/PARENT_UUID/CHILD_UUID.json`.
`--resume` hiện dùng cho phiên chính; transcript con giữ lại để kiểm tra bằng
file hoặc mở qua `SessionStore` API. Token trong phiên chính chỉ tính request của
cha; token của con nằm trong phiên con, còn budget lượt chạy và TUI tính cả hai.

Đây là triển khai độc lập còn nhỏ, chưa tương đương toàn bộ ZCode/DSH.

## Cấu hình và dữ liệu

- Config: `$XDG_CONFIG_HOME/goon/config.json`, mặc định `~/.config/goon/config.json`.
- Sessions: `$XDG_DATA_HOME/goon/sessions/`, mặc định `~/.local/share/goon/sessions/`.
- Archives: `SESSION_UUID.compact-N.json` trong cùng thư mục session.
- Ưu tiên: cờ CLI > biến `GOON_*` > config người dùng > mặc định. Không tự đọc
  `.env` hoặc config provider của dự án.

Các biến: `GOON_PROVIDER`, `GOON_BASE_URL`, `GOON_MODEL`, `GOON_KEY_ENV`,
`GOON_CONTEXT_WINDOW`, `GOON_MAX_OUTPUT_TOKENS`, `GOON_TIMEOUT_MS`,
`GOON_RUN_TOKEN_BUDGET`.

Context mặc định 128.000 token, output 8.192, thời hạn request 300.000 ms,
budget mỗi tác vụ 100.000 token. Đặt context/output theo model thực tế:

```bash
goon --context-window 64000 --max-output-tokens 4096
goon --run-token-budget 0  # tắt budget cho tác vụ dài
```

Budget kiểm tra tại ranh giới request, không phải trần chi phí thanh toán cứng.
Khi endpoint không trả usage, Goon dùng ước lượng ký tự để tránh chạy vô hạn.
Compact cũng gọi model và có tính token. Thông báo chạm output limit nghĩa là
tool của response chưa hoàn tất đó chưa được thực thi.

## Phát triển

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

Test pseudo-terminal cần Python 3 trên POSIX; bản cài Goon không cần Python.
Không cần build monorepo tham khảo. Mã nguồn chia theo trách nhiệm:

| File | Trách nhiệm |
|---|---|
| `src/harness.ts` | Agent loop, điều phối tool, usage, cancellation |
| `src/provider.ts` | Chọn adapter API |
| `src/tools.ts` | Registry, schema và approval |
| `src/files.ts`, `src/process.ts` | File và subprocess I/O |
| `src/session.ts` | Lock, checkpoint, resume và sửa lịch sử bị ngắt |
| `src/context.ts` | System prompt, AGENTS.md và compaction |
| `src/cli.ts`, `src/tui.ts` | Flags, TUI/readline, NDJSON, vòng đời terminal |
| `src/mcp.ts` | MCP client, transport, schema, lifecycle |
| `src/subagents.ts` | Context con, concurrency, budget và approval |

Public API xuất `Harness`, `ToolRegistry`, `SessionStore` và kiểu liên quan.
`ToolRegistry.register()` nhận schema Zod, cờ `readOnly` và hàm execute; trả về
hàm gỡ đăng ký. MCP nạp theo config người dùng; không tự đọc cấu hình MCP từ repo.

Xem [NOTICE.md](NOTICE.md) để biết nguồn tham khảo và commit đã đọc;
[docs/VALIDATION.md](docs/VALIDATION.md) ghi kết quả và phạm vi đã kiểm chứng.

## Repository

Source: [binxgtl/goon-cli](https://github.com/binxgtl/goon-cli).
Các bản cài và checksum nằm trong [GitHub Releases](https://github.com/binxgtl/goon-cli/releases).
Repo vẫn giữ gói bootstrap `release/goon-cli-0.1.0.tgz`; dùng Releases để lấy bản mới.

## Tự động build và release

Mỗi lần push lên `main`, workflow `Check and release` sẽ:

1. Chạy typecheck, lint và toàn bộ test trên Node 22.16 và 24, gồm umask
   mặc định, `027` và `007`.
2. Build gói npm, cài thử trong prefix riêng và kiểm tra phiên bản thực tế.
3. Tạo tag `vX.Y.Z` và GitHub Release với gói `.tgz` cùng `SHA256SUMS.txt`.

Bản đầu dùng `v0.1.0`. Sau đó mỗi commit mới trên main tự tăng patch. Có thể
đặt một phiên bản stable cao hơn trong `package.json` **và** `npm-shrinkwrap.json`
để chủ động phát hành minor/major. Bot đồng bộ và commit tăng version trước khi
tạo tag; không đổi tag cũ. Commit của bot dùng `GITHUB_TOKEN`, không lặp workflow.
Rerun tại cùng commit đã có tag sẽ dùng lại phiên bản đó để phục hồi release.
Commit bị một cập nhật mới hơn vượt qua sẽ được bỏ qua khi publish.

Pull request chỉ chạy kiểm tra. Có thể chọn Actions → Check and release → Run
workflow trên main để chạy thủ công. Không cần thêm PAT hay npm token; job dùng
quyền `contents: write` của `GITHUB_TOKEN`. Nếu sau này bật branch protection chặn
bot ghi main, cần điều chỉnh quyền/rule của chính repo.

Assets được upload vào draft trước khi công khai. Lệnh `npm run release:pack`
cho phép kiểm tra build, gói npm và cài thử ở local mà không publish. Workflow
không publish lên npm registry và không tự đổi bản Goon đã cài trên điện thoại.
Để cập nhật bản đang dùng, chạy lại lệnh `npm install -g` từ Releases ở trên.
