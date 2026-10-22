# Changelog

## 0.3.0

Giao diện tối giản, vá bảo mật, sửa lỗi và tăng hiệu năng. Chi tiết và cách tái hiện từng lỗi:
[docs/AUDIT.md](docs/AUDIT.md).

### Giao diện
- TUI tối giản: bỏ header band, khung viền và nền tô màu; một đường kẻ, dòng nhập và một dòng
  trạng thái (chế độ, model, effort … workspace (nhánh), todo, ctx %, token). Một màu nhấn duy nhất;
  màu khác chỉ xuất hiện khi mang trạng thái (diff, lỗi, duyệt quyền, auto-approve).
- Banner, `/btw`, `/recap`, resume và `goon setup` là các khối chữ ngắn, tự xuống dòng ở mọi độ rộng.

### Bảo mật
- Chuyển từ `auto`/`--yes` về `ask`/`plan` bật lại việc duyệt quyền ngay lập tức.
- Shift+Tab khi đang có prompt duyệt chỉ duyệt một lần, không còn cấp "always".
- API key của goon không còn lọt vào môi trường của `bash`, `!cmd`, `rg`, `git`.
- Đọc ảnh do model yêu cầu ngoài workspace cần duyệt; tool bị ẩn không gọi được bằng tên.
- Nhập API key ẩn; cảnh báo base URL `http://` không phải loopback; không xoá phiên mơ hồ/đang mở.
- Nâng AI SDK, `ajv`, `zod`: `npm audit` 0 lỗ hổng.

### Sửa lỗi
- `edit_file` không còn làm hỏng nội dung chứa `$&`, `$'`, `` $` ``, `$$`.
- Ảnh đính kèm không còn làm vượt context hay khiến phiên không resume được (phiên cũ tự được sửa).
- `apply_patch` hỗ trợ `Move to`, nhiều khối cho một file, newline cuối file.
- `/revert` không ghi đè file bạn sửa sau lượt của agent (`--force` để ghi đè).
- `/effort` có hiệu lực ngay; đổi effort khi đang chạy không còn giết MCP/subagent của lượt đó.
- `!cmd` và `/goal` khi đang chạy không còn làm hỏng thứ tự lịch sử (lỗi 400 từ provider).
- Lệnh gõ khi đang bận được xếp hàng thật; wizard luôn cho đổi model.

### Hiệu năng
- Render TUI tăng dần (~80 lần nhanh hơn mỗi frame), `--version`/`--help` ~5 lần nhanh hơn,
  liệt kê phiên có chỉ mục, MCP khởi động song song và được tái sử dụng, 6 tool đọc chạy song song,
  retry tôn trọng `Retry-After`.

### Tiện ích cho dev
- `git diff | goon -p "review"`, `goon -p ... < file`, `-o/--output-last-message`.
- Duyệt theo tiền tố lệnh (`p`), prompt tuỳ biến `/prompts:<tên>`, `/key`, `/provider`,
  chuông báo khi cần chú ý (`GOON_NOTIFY=0` để tắt), clipboard cho Termux/Wayland/xsel.
