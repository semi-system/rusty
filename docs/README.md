# Rusty Mail Server - Tài liệu thiết kế và kế hoạch

Mục lục của mọi spec (thiết kế) và plan (kế hoạch) trong `docs/superpowers/`. Cập nhật lần cuối: 2026-11-13.

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
| [specs/2026-11-03-rusty-postgres-valkey-design.md](superpowers/specs/2026-11-03-rusty-postgres-valkey-design.md) | Chờ duyệt | Postgres 18 + Valkey + tenant (phần SQLite đã bỏ ngày 2026-11-13: Postgres là backend duy nhất); §3.2b PgBouncer (đã quyết định 2026-11-04); **cần sửa**: ràng buộc chéo mailbox (§3c.1) theo khóa phân mảnh mailbox |
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
| [plans/2026-11-03-postgres-backend.md](superpowers/plans/2026-11-03-postgres-backend.md) | Phần 1 (PG0-PG2) xong, GO; phần 2 (PG3-PG8) sẵn sàng thực thi | Backend Postgres 18: hạ tầng test, schema + parity, spike (GO); phần 2: Busy + chính sách đồng hồ + repo Directory, repo Mailbox, Jobs/GC, chạy toàn bộ test trên 2 backend + mutation, benchmark quy mô, hardening (role, pool, hàng đợi), PG8a: lớp PgBouncer (transaction pooling), kiểm kê trạng thái phiên, chạy bộ test qua bouncer |
| [plans/2026-11-08-usable-mailserver.md](superpowers/plans/2026-11-08-usable-mailserver.md) | Chờ duyệt | Ưu tiên mới (2026-11-08): dùng được trước, hiệu năng/hardening sau. M1 một tiến trình `rustyd`, SQLite, TLS tại dịch vụ; U1-U9 (skeleton, `rustyctl`, SMTP vào/submission, hàng đợi gửi + DKIM, IMAP, nghiệm thu, đóng gói); mục 1b: bất biến kiến trúc và bảng điểm cắm. **Tạm hoãn (backlog): PG6 sửa review, PG7, PG8, PG8a** |
| [plans/2026-11-13-status-and-next-steps.md](superpowers/plans/2026-11-13-status-and-next-steps.md) | Đang làm | **Đọc đầu tiên**: trạng thái hiện tại (đã bỏ SQLite, Postgres duy nhất), quyết định đang có hiệu lực (blob S3 qua biến môi trường, dev profile qua `RUSTY_DEV_PG_*`), việc tiếp theo, lưu ý môi trường |
| [plans/2026-11-10-handoff-remaining-work.md](superpowers/plans/2026-11-10-handoff-remaining-work.md) | Đang làm | Bàn giao cho người triển khai thứ hai (Codex): trạng thái repo, luật làm việc và các cổng CI, gói việc W1-W8 (DNS validate, web admin, backlog Postgres, CI ergonomics, U11 Postgres production, đóng gói, nghiệm thu, hai VM), việc nào bắt đầu ngay / sau merge |

## Việc cần viết tiếp (chưa có tài liệu)
- Kế hoạch backend Postgres (P-13) và thiết kế trait theo phương thức cho `Mailbox`/`Directory`.
- Kế hoạch `certd`, KMS wrapper, `store` service, `tlsd` (dự phòng), SMTP relay, IMAP, nhận/gửi thư, MTA đầy đủ.
- Sửa các spec ghi "cần sửa" ở bảng trên sau khi người dùng duyệt hướng.

## Quy trình, CI và vận hành
| Tài liệu | Nội dung |
|---|---|
| [git-workflow.md](git-workflow.md) | Quy trình git: trunk-based, nhánh `type/short-name`, Conventional Commits, PR vào `main` (squash), check bắt buộc `ci-ok`, CODEOWNERS, phát hành, hotfix, nâng pin, kiểm tra trước khi push, thiết lập GitHub một lần (import ruleset) |
| [ci.md](ci.md) | Thiết kế CI GitHub Actions: job `changes` và `ci-ok`, cache, tách job, ước lượng thời gian (nêu rõ phần đo và phần ước lượng), đối chiếu với `scripts/ci.sh` |
| [docker.md](docker.md) | Image container, workflow `docker.yml`/`release.yml`, quy tắc an toàn cho workflow |
| [buildserver.md](buildserver.md) | Máy chủ build từ xa (Ubuntu VM) |
| [local-run.md](local-run.md) | Chạy thử cục bộ |
