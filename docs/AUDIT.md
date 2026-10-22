# Audit bảo mật, lỗi và hiệu năng (dựa trên v0.2.25)

Phạm vi: toàn bộ `src/` (~11k dòng TypeScript), `bin/`, dependency và CI. Mỗi lỗi bên dưới đã
được tái hiện trước khi sửa và có test hồi quy trong `tests/audit.test.mjs` (20 test mới; toàn bộ
132 test đều pass, kể cả ở umask `027`/`007`).

## Bảo mật

| # | Mức | Vấn đề | Cách sửa |
|---|---|---|---|
| S1 | Cao | Khởi động ở `auto` (`--yes` hoặc `approvalMode: "auto"` đã lưu) rồi chuyển về `ask`/`plan` vẫn tự duyệt mọi lệnh ghi/bash, vì callback dùng chế độ chụp lúc dựng harness. | Callback chỉ đọc chế độ hiện tại. Test PTY thật: `--yes` → `/mode ask` → ghi file phải hỏi. |
| S2 | Trung bình–cao | Nhấn Shift+Tab khi đang có prompt duyệt: chế độ đi qua `auto` và trả lời `a` (always) → tool đó được duyệt vĩnh viễn trong phiên kể cả khi đã quay về `ask`. | Trả lời `y` (chỉ một lần). |
| S3 | Trung bình | `GOON_API_KEY`/`keyEnv` nằm trong môi trường của `bash`, `!cmd`, `rg` → model hoặc prompt-injection có thể `env`, `curl …$GOON_API_KEY`; output còn được lưu vào phiên. | `childEnv()` loại các biến bí mật khỏi mọi tiến trình con. |
| S4 | Trung bình | `view_image` (không cần duyệt, kể cả `--read-only`) và `@ảnh` trong prompt do model viết (subagent) đọc được ảnh ở bất kỳ đâu (`/home/…/Pictures`, `../`). Tên `*.png` là symlink tới file khác cũng được đọc. | Ảnh ngoài workspace cần duyệt; prompt của subagent chỉ đính kèm ảnh trong workspace; đích symlink phải là đường dẫn ảnh. |
| S5 | Thấp | Tool bị ẩn ở chế độ hiện tại vẫn thực thi được nếu model gọi đúng tên. | `execute()` áp cùng quy tắc với `schemas()`. |
| S6 | Thấp | API key hiện rõ khi gõ trong `goon setup`/`/setup`. | Nhập ẩn (readline và TUI), thêm `/key`. |
| S7 | Thấp | `/delete <prefix>` xoá phiên khớp đầu tiên khi prefix mơ hồ và xoá cả lock của phiên đang mở ở tiến trình khác. | Từ chối prefix mơ hồ và phiên đang bị khoá bởi PID còn sống. |
| S8 | Thấp | Base URL `http://` tới host không phải loopback gửi API key dạng rõ mà không cảnh báo. | Cảnh báo khi khởi động và trong `goon doctor`. |
| S10 | Trung bình | (Phát hiện bởi review độc lập, trong tính năng mới) Duyệt theo tiền tố `npm install` sẽ tự duyệt cả `npm install <gói khác>`; `git diff` tự duyệt cả `git diff --output=/đường/dẫn`. | Lệnh cài gói, sửa/xoá file, mạng chỉ nhớ đúng nguyên câu lệnh; tham số thêm ra ngoài workspace hoặc bật ghi/chạy chương trình phải hỏi lại. |
| S9 | Thấp–TB | `npm audit`: `@ai-sdk/provider-utils` (GHSA-866g-f22w-33x8), `ajv` ReDoS (GHSA-2g4f-4pwh-qvx6). | Nâng `ai` 6.0.312, providers, `ajv` 8.20.0, `zod` 4.6.7 → 0 lỗ hổng. |

## Lỗi chức năng

| # | Vấn đề | Cách sửa |
|---|---|---|
| B1 | `edit_file` dùng `String.replace` nên `$&`, `$'`, `` $` ``, `$$` trong nội dung mới bị diễn giải: `PRICE="$$5"` thành `PRICE="$5"`, `$&` chèn lại chuỗi cũ. | Thay thế literal (`replaceFirstLiteral`), cả trong preview diff. |
| B2 | Ảnh lưu dạng `Buffer`/`Uint8Array` → JSON thành mảng số: ảnh 200 KB ≈ 200k "token" ước lượng (ảnh > ~40 KB đã vượt context 128k → lỗi "exceeds the context budget"), file phiên phình to, và phiên có ảnh **không resume được** (bị ẩn khỏi danh sách). | Lưu base64, ước lượng cố định 1.600 token/ảnh, bỏ ảnh khỏi dữ liệu tóm tắt; phiên cũ được tự sửa khi nạp. |
| B3 | `/effort <mức>` chỉ lưu config, harness đang chạy vẫn dùng effort cũ. | Áp dụng và dựng lại harness. |
| B4 | Alt+. / Alt+, khi agent đang chạy dựng lại harness giữa lượt → đóng MCP server và huỷ subagent mà lượt đó đang dùng. | Hoãn rebuild tới khi lượt kết thúc. |
| B5 | `!cmd`, `/goal …` khi đang chạy chèn tin nhắn user giữa `tool_call` và `tool_result` → provider trả 400 ở bước sau. | Hàng đợi ngữ cảnh, chèn ở điểm an toàn (sau kết quả tool, trước request kế tiếp). |
| B6 | Wizard `setup` bỏ qua bước chọn model nếu đã có model (kể cả khi đổi provider). | Luôn hỏi; Enter giữ model cũ; cảnh báo nếu model không có trong `/models`. |
| B7 | `apply_patch` bỏ qua `*** Move to:`, khối `Update` thứ hai của cùng file ghi đè khối đầu, file thêm mới thiếu newline cuối. | Kế hoạch ảo theo từng file, hỗ trợ move, kiểm tra toàn bộ trước khi ghi. |
| B8 | `/revert` ghi đè cả những sửa đổi bạn làm sau lượt của agent (trường `afterHash` có lưu nhưng không dùng). | Phát hiện xung đột, giữ file và báo; `--force` để ghi đè. |
| B9 | Lệnh slash gõ khi đang bận được báo "Queued" nhưng thực ra bị bỏ. | Đưa vào hàng đợi thật. |
| B10 | Cắt transcript TUI ở 120k ký tự có thể rơi giữa khối code → phần còn lại hiển thị như code. | Cắt theo ranh giới block. |
| B11 | `/btw`, `/recap` gửi effort cấu hình (vd. `max`) thay vì `low` vì fetch wrapper ghi đè. | Header override theo từng request, bị gỡ trước khi gửi đi. |
| B12 | `rg` trả mã 2 (một file không đọc được) làm mất toàn bộ kết quả tìm kiếm. | Giữ kết quả, kèm ghi chú. |
| B13 | `AGENTS.md` là thư mục hoặc không đọc được → mọi lượt chạy đều lỗi. | Bỏ qua lỗi đọc. |
| B14 | Ctrl+C khi `!lệnh-dài` đang chạy (lúc rảnh) thoát luôn goon. | Ctrl+C huỷ lệnh đó. |
| B15 | README nhắc `/key`, `/provider`, `snapshotWorkspace` không tồn tại; nhiều nhánh lệnh trùng lặp không bao giờ chạy tới trong `cli.ts`. | Hiện thực `/key`, `/provider`; xoá code chết; sửa tài liệu. |

## Hiệu năng (đo trên cùng máy, trước → sau)

| Hạng mục | Trước | Sau |
|---|---|---|
| Render TUI mỗi frame (transcript 50k ký tự, spinner/stream) | ~11.8 ms | ~0.15 ms (render tăng dần, kết quả giống hệt render đầy đủ — có property test) |
| `goon --version` / `--help` | ~150 ms | ~30 ms |
| Liệt kê 80 phiên (~50 MB) | ~700 ms | ~165 ms lần đầu, ~2–3 ms khi có chỉ mục |
| Khởi động 4 MCP server (mỗi cái 700 ms) | ~3.5 s | ~1.2 s; đổi effort/model không còn khởi động lại server |
| Tool chỉ-đọc song song | 2 | 6 |

Ngoài ra: không còn parse/serialize lại toàn bộ body request mỗi bước (fast path khi SDK đã đặt
đúng effort), ước lượng token cache theo từng message, `AGENTS.md`/memories đọc một lần mỗi lượt,
file phiên ghi dạng compact và các lần lưu chồng nhau được gộp, bản sao lưu `/undo` giới hạn 32 MiB.
