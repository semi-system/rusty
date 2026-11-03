# Rusty - Tài liệu thiết kế và kế hoạch

Mục lục của mọi spec (thiết kế) và plan (kế hoạch) trong `docs/superpowers/`. Cập nhật lần cuối: 2026-11-03.

**Quy ước trạng thái**
- **Đã triển khai**: có mã và test trong repo.
- **Đang làm**: đang được thực thi.
- **Chờ duyệt**: đã viết, quyết định chính đã được người dùng chọn nhưng spec chưa được duyệt chính thức; có thể còn mục phải sửa (ghi ở cột Ghi chú).
- **Thay thế một phần / Đã thay thế**: không còn là nguồn sự thật cho phần được nêu.

**Quy trình cho mỗi dự án con:** brainstorm -> spec -> duyệt -> kế hoạch -> thực thi (agent triển khai + reviewer độc lập) -> review cuối. Sổ ghi quyết định và phát hiện của từng đợt nằm ở `.superpowers/sdd/<tên-kế-hoạch>/progress.md` (không đưa vào git).

## Lộ trình
| Tài liệu | Nội dung |
|---|---|
| [plans/2026-11-03-roadmap-zimbra-parity.md](superpowers/plans/2026-11-03-roadmap-zimbra-parity.md) | Lộ trình tổng tới mức tương đương Zimbra (mức C, trừ ActiveSync), các cột mốc M1-M5, quyết định đã chốt (mục 0c, 0d), chính sách thư viện và kiểm tra giấy phép |

## Spec (thiết kế)
| Spec | Trạng thái | Ghi chú |
|---|---|---|
| [specs/2026-11-02-rusty-foundation-design.md](superpowers/specs/2026-11-02-rusty-foundation-design.md) | Đã triển khai | Dự án nền tảng (thư viện core/store/mime/directory); §8c là các điều chỉnh sau review cuối |
| [specs/2026-11-03-rusty-scale-targets-design.md](superpowers/specs/2026-11-03-rusty-scale-targets-design.md) | Quyết định đã chốt | Mục tiêu 100.000 hộp thư, 1.000.000 thư/ngày; nhân bản Postgres bất đồng bộ; T3 chỉ là mở rộng; khóa phân mảnh theo mailbox |
| [specs/2026-11-03-rusty-postgres-valkey-design.md](superpowers/specs/2026-11-03-rusty-postgres-valkey-design.md) | Chờ duyệt | Postgres 18 + Valkey + SQLite system DB + tenant; **cần sửa**: ràng buộc chéo mailbox (§3c.1) theo khóa phân mảnh mailbox |
| [specs/2026-11-03-rusty-process-model-design.md](superpowers/specs/2026-11-03-rusty-process-model-design.md) | Chờ duyệt | `rustyd`, `rustyctl`, dịch vụ `store` tách riêng; §0 điều chỉnh cho container |
| [specs/2026-11-03-rusty-container-deployment-design.md](superpowers/specs/2026-11-03-rusty-container-deployment-design.md) | Thay thế một phần | Triển khai container; **§3b (Traefik) bị thay bởi spec biên HAProxy**, các phần khác còn hiệu lực |
| [specs/2026-11-03-rusty-edge-tls-layer-design.md](superpowers/specs/2026-11-03-rusty-edge-tls-layer-design.md) | Quyết định đã chốt | Biên = HAProxy cách A trước; `tlsd` (cách B) là dự phòng; §0 là quyết định hiện hành |
| [specs/2026-11-03-rusty-distributed-model-design.md](superpowers/specs/2026-11-03-rusty-distributed-model-design.md) | Chờ duyệt | Mô hình phân tán; **cần sửa §9** (khóa phân mảnh là mailbox, không phải tenant) và §7 (RPO > 0) |
| [specs/2026-11-03-rusty-acl-sharing-design.md](superpowers/specs/2026-11-03-rusty-acl-sharing-design.md) | Chờ duyệt | ACL/chia sẻ folder (RFC 4314); **cần sửa §4** (khóa ngoại chéo mảnh) |
| [specs/2026-11-03-rusty-tenant-tls-design.md](superpowers/specs/2026-11-03-rusty-tenant-tls-design.md) | Chờ duyệt | TLS theo domain, `certd` bằng Go, dashboard hạn chứng chỉ (§6c), KMS (§6d) |
| [specs/2026-11-03-rusty-kms-design.md](superpowers/specs/2026-11-03-rusty-kms-design.md) | Quyết định đã chốt | OpenBao sau wrapper Rust; một số điểm OpenBao cần kiểm chứng (xóa khóa, xuất khóa, `mlock`) |

## Plan (kế hoạch thực thi)
| Plan | Trạng thái | Ghi chú |
|---|---|---|
| [plans/2026-11-02-rusty-foundation.md](superpowers/plans/2026-11-02-rusty-foundation.md) | Đã triển khai | Task 1-9 xong và qua review |
| [plans/2026-11-03-distributed-prep.md](superpowers/plans/2026-11-03-distributed-prep.md) | Đang làm | P0-P4 xong; P6 đang sửa sau review; P5 đang chạy |
| [plans/2026-11-03-edge-haproxy.md](superpowers/plans/2026-11-03-edge-haproxy.md) | Sẵn sàng thực thi | Biên HAProxy cách A (H1-H3), có báo cáo go/no-go |
| [plans/2026-11-03-edge-skeleton.md](superpowers/plans/2026-11-03-edge-skeleton.md) | Đã thay thế | Kế hoạch Traefik cũ; **không thực thi** |

## Việc cần viết tiếp (chưa có tài liệu)
- Kế hoạch backend Postgres (P-13) và thiết kế trait theo phương thức cho `Mailbox`/`Directory`.
- Kế hoạch `certd`, KMS wrapper, `store` service, `tlsd` (dự phòng), SMTP relay, IMAP, nhận/gửi thư, MTA đầy đủ.
- Sửa các spec ghi "cần sửa" ở bảng trên sau khi người dùng duyệt hướng.
