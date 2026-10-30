## Thay đổi

<!-- Làm gì và vì sao. Liên kết issue nếu có (Closes #123). -->

## Kiểm tra

- [ ] `npm run check` pass (rustfmt + clippy + oxlint + cargo test + test Node + e2e)
- [ ] Có test đi kèm thay đổi hành vi (hoặc giải thích bên dưới vì sao không)
- [ ] Đã ghi vào `CHANGELOG.md` (mục `## Unreleased`) và README nếu thêm lệnh, cờ, biến môi trường hoặc event
- [ ] Không thêm dependency mới (hoặc nêu lý do bên dưới); không để API key/secret trong code, test, fixture
- [ ] Không sửa `version` (trừ khi chủ ý tăng minor/major, xem [CONTRIBUTING](https://github.com/binxgtl/goon-cli/blob/main/CONTRIBUTING.md#12-phát-hành))

> Merge vào `main` sẽ tự động phát hành phiên bản mới.

## Ghi chú cho người review

<!-- Rủi ro, lựa chọn thiết kế, phần chưa kiểm chứng được (ví dụ Windows/macOS/Termux). -->
