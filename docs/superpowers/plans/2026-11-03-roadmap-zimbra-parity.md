# Rusty - Lộ trình đạt chức năng tương đương Zimbra

Ngày: 2026-11-03. Trạng thái: bản nháp để người dùng duyệt thứ tự và phạm vi. Đây là **lộ trình tổng**, không phải kế hoạch triển khai chi tiết: mỗi dự án con (P-xx) khi tới lượt phải đi qua vòng riêng **brainstorm -> spec -> duyệt -> kế hoạch -> thực thi -> review**, như dự án nền tảng.

Nguyên tắc xuyên suốt (đã chốt trước đó): Rust mặc định (Go chỉ cho `certd` và công cụ vận hành được duyệt), bảo mật là ưu tiên số một, Postgres 18 + Valkey + SQLite cho cài đặt hệ thống, domain là tenant, mỗi dịch vụ một container, `store` là cổng dữ liệu duy nhất, thiết kế mới dùng giao thức chuẩn (không SOAP/LDAP/MariaDB của Zimbra).

Không đưa ước lượng lịch cụ thể: chưa có dữ liệu năng suất thực tế của dự án này. Kích cỡ dưới đây là **tương đối** (S < M < L < XL; dự án nền tảng vừa xong tính là L).

## 0. Hiện trạng
| | Trạng thái |
|---|---|
| Nền tảng (core, store SQLite, mime, directory, GC, CI, fuzz, musl) | Xong, ~306 test |
| Chuẩn bị phân tán: P0, P1, P2 xong; P3 đang sửa sau review; P4, P6, P5 chờ | Đang làm |
| Spec đã viết, chờ duyệt: mô hình tiến trình, Postgres+Valkey+tenant, container+Traefik, phân tán, ACL, TLS theo domain | Có |
| Kế hoạch đã viết: chuẩn bị phân tán, khung cổng vào (E1-E3) | Có |
| **Dịch vụ chạy được** | **Chưa có** |

## 0b. Mục tiêu quy mô (cập nhật 2026-11-03)
Tải thực tế hiện tại nhỏ (~700-800 thư đi và đến mỗi ngày cho cả hệ thống), nhưng người dùng yêu cầu **thiết kế cho tải siêu lớn**. Spec `2026-11-03-rusty-scale-targets-design.md` định nghĩa các bậc T1/T2/T3, phát hiện rằng **khóa phân mảnh phải là mailbox chứ không phải tenant**, và đặt các cổng benchmark G1-G4. Hệ quả cho lộ trình:
- **Postgres 18 + Valkey (P-13) là đích sản xuất** (không phải tùy chọn); SQLite chỉ cho dev/test/cài đặt nhỏ và system DB.
- **Khôi phục** P-43 (chuyển mailbox, phân tầng lưu trữ, S3), P-44 (nhiều node, HA), P-45 (Kubernetes) và P6 (lô, tối ưu truy vấn) làm dự án chính.
- **Thêm P-13b: cụm điều khiển và phân mảnh mailbox** (thư mục tenant ở cụm điều khiển, dữ liệu thư phân mảnh theo mailbox, định tuyến, ràng buộc chéo mảnh). Phụ thuộc P-13; cần sửa spec phân tán §9, Postgres-Valkey §3c và ACL §4 trước.
- **Thêm P-13c: thang benchmark G1-G4** và bộ sinh tải tái lập; G1 chạy ngay trước khi viết backend Postgres.
- Đánh giá lại hàng đợi/sự kiện (message queue) ở cổng G3; chưa cần ở T1-T2.
- Triển khai nhỏ vẫn phải gọn (scale-targets §6): mọi thành phần phân tán tắt mặc định, cùng một mã.

## 0c. QUYẾT ĐỊNH ĐÃ CHỐT (người dùng, 2026-11-03)
| # | Quyết định | Hệ quả |
|---|---|---|
| 1 | **Mức C** (tương đương B + tính năng bản thương mại: ActiveSync, sao lưu/khôi phục tích hợp, HSM...) | Đưa P-50 (ActiveSync) vào phạm vi, kèm điều kiện: kiểm tra điều khoản/giấy phép của giao thức trước khi triển khai. Zimlets (P-51), chat (P-52), eDiscovery/DLP (P-53) **chưa được chọn**, vẫn cần quyết định riêng |
| 2 | **Dùng thư viện có sẵn trước, viết lại sau MVP** | Chính sách thư viện ở mục 0d; giữ ranh giới (trait) để thay thế được |
| 3 | **rspamd + ClamAV** trong container cô lập | P-12 dùng hai thành phần ngoài; sandbox chặt, không cấp mạng ra ngoài trừ cập nhật chữ ký qua cổng riêng |
| 4 | **100.000 hộp thư, 1.000.000 thư/ngày** | Mục tiêu thiết kế nới từ T3 xuống T2; phân mảnh hoãn (scale-targets §1b, §7b) |
| 5 | **Nhân bản Postgres bất đồng bộ** | RPO > 0; hệ quả và giảm thiểu ở scale-targets §1c |
| 6 | **Backend Postgres trước** | P-13 đứng trước IMAP trên đường găng; M1 chạy trên Postgres |

### Trả lời bổ sung của người dùng (2026-11-03, lượt hai)
| Mục | Quyết định | Hệ quả |
|---|---|---|
| Zimbra hiện có để chuyển | Có nhưng **không gấp** | P-41 giữ nhưng không nằm trên đường găng; cần phiên bản và số liệu Zimbra khi tới lượt |
| Tải | 700-800 thư/ngày là **toàn hệ thống** | Đã tính trong scale-targets |
| IP gửi thư | **IP riêng**; cần thêm chức năng **nhập cấu hình SMTP relay** | Thêm vào P-06/P-10: smarthost cấu hình được (host, cổng, TLS, xác thực) theo hệ thống và theo tenant; thông tin xác thực relay là bí mật, lưu mã hóa qua KMS, không bao giờ trả lại qua API; kiểm tra kết nối khi lưu; định tuyến theo domain gửi/địa chỉ; dự phòng khi relay hỏng; hạn mức và nhật ký |
| Ngôn ngữ giao diện | **Chỉ tiếng Anh** | Web client và admin console chỉ tiếng Anh (vẫn xử lý Unicode đúng trong dữ liệu) |
| ActiveSync | **Không cần: dùng giao thức tiêu chuẩn là đủ** | **P-50 ra khỏi phạm vi.** Mức C nay = B + sao lưu/khôi phục tích hợp + HSM/phân tầng lưu trữ + các tính năng vận hành khác, không có ActiveSync. Hết điều kiện kiểm tra pháp lý ActiveSync. Di động dùng IMAP, CalDAV, CardDAV, JMAP |
| Zimlets | Chưa cần | P-51 ngoài phạm vi hiện tại |
| LDAP/AD ngoài | Chưa cần | Chỉ SSO qua OIDC/SAML ở P-28 nếu cần |
| Chat, eDiscovery/DLP | Để sau | P-52, P-53 hoãn |
| Chia sẻ giữa tenant, danh sách chứa tài khoản domain khác | **Không cho** | Khóa ngoại tổng hợp "cùng tenant" giữ làm quy tắc; không cần thiết kế quan hệ tin cậy giữa tenant |
| Cổng STARTTLS/SSL | **Mở tại cổng vào (edge), không mở trực tiếp ở dịch vụ** | Ghi đè mặc định "đóng 587/143/110" của tôi: nay mở ở `edge`; xem container spec §3b.2 |
| CA production | **Let's Encrypt** | Cấu hình sẵn ở `certd` (staging cho dev/test) |
| Xác minh domain bằng DNS TXT | Đồng ý | Điều kiện để domain hoạt động |
| Tên máy mỗi tenant | Giữ trần mặc định 20 | "Tên máy" là tên DNS máy khách dùng để kết nối (xem tenant-tls) |
| Chứng chỉ sắp hết hạn | **Hiển thị trên dashboard của admin tenant và super admin; nút bật/tắt failover ACME** | Thêm vào P-17 và tenant-tls §6/§7 |
| KMS | **Dựng sẵn như một microservice** | Dự án mới P-60 (KMS); cần quyết định "tự viết hay triển khai OpenBao" (đã hỏi) |
| Runtime container | **Docker** | Khuyến nghị Docker rootless; ghi nhận khác biệt khi test |
| Liên hệ báo lỗ hổng | mornngstr1@gmail.com | Đã đưa vào SECURITY.md |
| Cách review | Đồng ý đề xuất | Review mạnh cho bảo mật và đồng thời; nhẹ hơn cho phần còn lại |
| Môi trường test | **Chạy ngay trên máy này** | Benchmark G1-G2 chạy cục bộ (máy 12 nhân, ~16 GB RAM, ext4); G3-G4 chưa có môi trường nhiều máy nên vẫn hoãn |

Hệ quả thứ tự: P-01 (hoàn tất chuẩn bị) -> benchmark G1 (SQLite làm đường cơ sở) -> **P-13 (Postgres + Valkey)** song song với P-02 (`store`) -> P-04/P-05/P-06 (M1). P-13b (phân mảnh) hoãn; P-43/P-44/P-45 hạ xuống sau M3, trừ HA đơn giản (replica bất đồng bộ + WAL) vào P-40.

## 0d. Chính sách thư viện: dùng có sẵn trước, viết lại sau MVP
Nguyên tắc: bọc mọi thư viện giao thức sau trait/ranh giới của Rusty để thay được; chỉ nhận thư viện có giấy phép cho phép (allowlist `deny.toml`: MIT, Apache-2.0, BSD, ISC, MPL-2.0...), bảo trì tốt, và phải qua `cargo-deny`/`cargo-audit`/fuzz của ta; mỗi thư viện phải được ghi lại phiên bản, giấy phép, người bảo trì và kế hoạch thay thế.

Kết quả kiểm tra giấy phép ngày 2026-11-03 (lấy từ API công khai của crates.io qua công cụ tra cứu, **cần kiểm lại bằng `cargo-deny` khi thực sự đưa vào**):
| Nhu cầu | Ứng viên | Giấy phép | Ghi chú |
|---|---|---|---|
| IMAP (mã hóa/giải mã giao thức) | `imap-codec` (duesee) 1.0.0 ổn định, 2.0.0-alpha mới nhất; `imap-next` 0.3.4 | MIT OR Apache-2.0 | Chỉ là codec/sans-I/O; **logic máy chủ IMAP vẫn tự viết**; bản 2.x còn alpha: cân nhắc ghim 1.0.0 |
| SMTP (phân tích giao thức) | `smtp-proto` 0.2.5 | Apache-2.0 OR MIT | Parser; máy chủ SMTP vẫn tự viết |
| SMTP gửi đi + DKIM ký | `mail-send` 0.7.0 | Apache-2.0 OR MIT | |
| DKIM/SPF/DMARC/ARC | `mail-auth` 0.15.1 | Apache-2.0 OR MIT | Dùng ở P-11 |
| MIME | `mail-parser` (đã dùng 0.9.4) | (đã cho phép) | |
| iCalendar/vCard/JSCalendar/JSContact | `calcard` 0.3.14 | Apache-2.0 OR MIT | Ít người dùng hơn (≈83 nghìn lượt tải): đánh giá kỹ trước P-30/P-31 |
| JMAP phía máy khách | `jmap-client` 0.4.3 | Apache-2.0 OR MIT | Chỉ để test/công cụ; **không có thư viện JMAP phía máy chủ được xác nhận giấy phép cho phép**: JMAP server tự viết trên các kiểu trên |
| **Sieve** | `sieve-rs` 0.7.3 | **AGPL-3.0-only** | **KHÔNG dùng được**: AGPL buộc toàn bộ sản phẩm kết hợp phải công bố mã nguồn theo AGPL và bị `deny.toml` chặn. Phiên bản 1.0.x đã bị yanked. Phương án: tự viết bộ thông dịch Sieve nhỏ (P-23), hoặc hoãn Sieve sau MVP. Nên coi **mọi crate máy chủ của Stalwart ngoài các thư viện giao thức/phân tích ở trên là AGPL cho tới khi kiểm tra từng cái** (tôi chưa kiểm) |
| Chống thư rác | rspamd | Apache-2.0 (trí nhớ, chưa kiểm) | Chạy container riêng |
| Chống virus | ClamAV | GPL-2.0 (trí nhớ, chưa kiểm) | Chạy container riêng, giao tiếp qua socket nên không liên kết mã |
| Tìm kiếm | `tantivy` | MIT (trí nhớ, chưa kiểm) | Đánh giá ở P-20 |

Việc phải làm đầu mỗi dự án dùng thư viện: một **bản đánh giá ngắn** (giấy phép, bảo trì, số lượng phụ thuộc kéo theo, lịch sử lỗ hổng, kết quả chạy fuzz của ta trên điểm vào của thư viện, và ranh giới thay thế).

## 1. Phạm vi "tương đương Zimbra"
Chưa có định nghĩa chốt, và nó quyết định khối lượng. Ghi nhận 3 mức để người dùng chọn:
- **Mức A - Lõi mail (Zimbra FOSS tối thiểu dùng được):** nhận/gửi thư, IMAP/POP3, web mail, thư mục, tìm kiếm, bộ lọc, chống thư rác/virus, multi-tenant, quản trị.
- **Mức B - Groupware (FOSS đầy đủ):** A + lịch, danh bạ, tác vụ, Briefcase, chia sẻ, GAL, CalDAV/CardDAV, tài nguyên.
- **Mức C - Mở rộng giống bản thương mại:** B + đồng bộ di động (ActiveSync), sao lưu/khôi phục tích hợp, HSM, và các tính năng khác thường chỉ có ở bản trả phí. Tôi **nhớ** là các mục này thuộc bản Network Edition chứ không phải FOSS, nhưng chưa kiểm lại với tài liệu hiện hành; cần xác minh trước khi coi là chuẩn tham chiếu.

Lộ trình dưới đây nhắm **mức C** (quyết định 2026-11-03, mục 0c), với A (M3) và B (M4) là các cột mốc trung gian; phần mức C (ActiveSync, sao lưu tích hợp, HSM) nằm ở giai đoạn 5-6.

## 2. Các dự án con
Ký hiệu: **Spec** = đã có spec (chờ duyệt) hay chưa; **Phụ thuộc** = phải xong trước.

### Giai đoạn 1 - Dịch vụ đầu tiên chạy được ("First mail")
| ID | Dự án | Nội dung | Phụ thuộc | Cỡ | Spec |
|---|---|---|---|---|---|
| P-01 | Hoàn tất chuẩn bị phân tán | P3, P4 (capability), P6 (truy vấn/lô), P5 (tài liệu) | nền tảng | M | Plan có |
| P-02 | Dịch vụ `store` + giao thức nội bộ | API theo vai trò, capability, truyền luồng, phân trang, UDS + mTLS, `rusty-storeapi`, `RemoteMailbox` | P-01 | L | Có (process-model §7b) |
| P-03 | Khung container + cổng vào | Compose, Traefik, checker cứng hóa, backend giả | P-02 (tùy) | M | Plan có (E1-E3) |
| P-04 | IMAP server | IMAP4rev2, TLS, SASL, IDLE, UIDPLUS, CONDSTORE/QRESYNC, NAMESPACE, ACL ánh xạ sau | P-02 | XL | **Chưa** |
| P-05 | Nhận thư (LMTP + SMTP vào cơ bản) | nhận từ MTA ngoài, giao qua `deliver`, kiểm tra người nhận, giới hạn | P-02 | L | **Chưa** |
| P-06 | Gửi thư cơ bản (submission) | SMTP submission, xác thực, hàng đợi đơn giản, relay ra ngoài | P-02, P-05 | L | **Chưa** |
| **M1** | **Cột mốc: dùng Thunderbird đọc/gửi thư thật** | Compose chạy được, một domain thử nghiệm | P-01..P-06 | | |

Tiêu chí M1: tạo domain+tài khoản bằng `rustyctl`, gửi thư từ Internet (hoặc máy thử) vào, đọc/xóa/di chuyển bằng Thunderbird qua IMAPS, gửi thư ra qua submission; mọi test ma trận cô lập chéo mailbox xanh; không sự cố dữ liệu khi kill dịch vụ giữa chừng.

### Giai đoạn 2 - MTA đầy đủ và vận hành production tối thiểu
| ID | Dự án | Nội dung | Phụ thuộc | Cỡ | Spec |
|---|---|---|---|---|---|
| P-10 | MTA hoàn chỉnh | Hàng đợi bền (lease), thử lại/backoff, bounce/DSN, giới hạn tốc độ, SMTP TLS cơ hội, chọn đường gửi, chống lạm dụng, điều tiết theo tenant | P-06 | XL | **Chưa** |
| P-11 | Xác thực thư | DKIM ký/kiểm, SPF, DMARC, ARC (tùy), MTA-STS/DANE (sau) | P-10 | L | **Chưa** |
| P-12 | Chống thư rác và virus | Giao diện bộ lọc (milter-like hoặc nội bộ), tích hợp rspamd/ClamAV (container riêng, sandbox), chấm điểm, quarantine, học | P-10 | L | **Chưa** |
| P-13 | Backend Postgres 18 + Valkey | Trait theo phương thức, migration Postgres, trigger plpgsql, bộ test tuân thủ dùng chung, cache cứng hóa | P-02 | XL | Có (Postgres-Valkey) |
| P-14 | Mô hình tenant thực thi | `domain_id` khắp nơi, quota, vai trò `domain-admin`, xác minh quyền sở hữu domain, test cô lập chéo tenant | P-13 | L | Có (§3c) |
| P-15 | `certd` (Go) + TLS theo domain | ACME, chứng chỉ tùy chỉnh, cấu hình động cho Traefik, CI Go | P-03, P-14 | L | Có (tenant-tls) |
| P-16 | `rustyd`/`rustyctl` đầy đủ + admin API | `status/start/stop/logs/check/config/backup/db/tls`, kiểm toán, `admin-api` cho domain-admin | P-02, P-14 | L | Có (process-model) |
| P-17 | Admin console v1 | Svelte 5; domain, tài khoản, alias, danh sách, quota, TLS, hàng đợi thư, job | P-16 | L | **Chưa** |
| **M2** | **Cột mốc: chạy production nhỏ, nhiều tenant** | MTA đầy đủ, TLS tự động, quản trị bằng console | P-10..P-17 | | |

Tiêu chí M2: nhiều domain độc lập, DKIM/SPF/DMARC đạt kiểm tra ngoài (ví dụ gửi tới hộp thư kiểm thử của nhà cung cấp lớn), thư rác/virus bị chặn trong test, quota thực thi, TLS tự cấp và gia hạn trong Pebble, sao lưu thủ công khôi phục được, benchmark đạt SLO tạm do người dùng đặt.

### Giai đoạn 3 - Trải nghiệm người dùng mức A
| ID | Dự án | Nội dung | Phụ thuộc | Cỡ | Spec |
|---|---|---|---|---|---|
| P-20 | Tìm kiếm toàn văn | tantivy theo mailbox, đánh chỉ mục nền, lọc theo ACL, dựng lại từ DB+blob, tìm kiếm đã lưu | P-02, P-13 | XL | **Chưa** |
| P-21 | JMAP | Core, Mail, Submission, push (EventSource/WebSocket), đồng bộ bằng `change_log`, blob upload/download | P-02, P-20 | XL | **Chưa** |
| P-22 | Web client mail (Svelte 5) | hộp thư, soạn, hội thoại, tìm kiếm, thư mục, tag, cài đặt | P-21 | XL | **Chưa** |
| P-23 | Luật thư của người dùng | Sieve (ManageSieve + JMAP Sieve), tự trả lời/vắng mặt, chuyển tiếp, chữ ký, nhiều danh tính, tag/cờ tự động | P-05, P-21 | L | **Chưa** |
| P-24 | POP3 | POP3S qua cổng vào | P-02 | S-M | **Chưa** |
| P-25 | ACL và chia sẻ folder thực thi | bảng `folder_grants`, `ScopedMailbox`, ánh xạ IMAP ACL/JMAP sharing, lọc tìm kiếm/đồng bộ | P-14, P-04 | L | Có (acl-sharing) |
| P-26 | Gửi thay mặt/ủy quyền | `send_as`/`send_on_behalf`, tích hợp submission | P-25, P-06 | M | **Chưa** (đã ghi ngoài phạm vi ACL) |
| P-27 | Tài khoản bên ngoài (data sources) | kéo thư từ IMAP/POP khác (Zimbra có bảng sẵn), lịch đồng bộ bằng job | P-02 | M-L | **Chưa** |
| P-28 | Xác thực nâng cao | 2FA (TOTP, WebAuthn), mật khẩu ứng dụng, SSO (OIDC/SAML), giới hạn đăng nhập, chống dò | P-14, P-22 | L | **Chưa** |
| **M3** | **Cột mốc: mức A hoàn chỉnh** | web mail đầy đủ, tìm kiếm, bộ lọc, chia sẻ cơ bản | P-20..P-28 | | |

### Giai đoạn 4 - Groupware (mức B)
| ID | Dự án | Nội dung | Phụ thuộc | Cỡ | Spec |
|---|---|---|---|---|---|
| P-30 | Danh bạ | mô hình (vCard), CardDAV, JMAP Contacts, nhóm, nhập/xuất, GAL cơ bản | P-25, P-21 | L | **Chưa** |
| P-31 | Lịch | iCalendar, lặp lại, múi giờ, CalDAV, JMAP Calendars, báo nhắc, chia sẻ | P-25, P-21 | XL | **Chưa** |
| P-32 | Lập lịch và lời mời | iTIP/iMIP (nhận/gửi lời mời qua thư), xem rảnh/bận, tài nguyên (phòng họp, thiết bị) | P-31, P-10 | XL | **Chưa** |
| P-33 | Tác vụ và ghi chú | VTODO/CalDAV tasks, ghi chú | P-31 | M | **Chưa** |
| P-34 | Briefcase (tài liệu) | thư mục tài liệu, phiên bản, chia sẻ, WebDAV/JMAP Files | P-25 | L | **Chưa** |
| P-35 | GAL và thư mục chung | danh bạ toàn tenant, tìm kiếm người dùng, danh sách phân phối động | P-30 | M | **Chưa** |
| P-36 | Web client groupware | lịch, danh bạ, tác vụ, tài liệu trong Svelte 5 | P-30..P-34 | XL | **Chưa** |
| **M4** | **Cột mốc: mức B (tương đương Zimbra FOSS đầy đủ)** | | P-30..P-36 | | |

### Giai đoạn 5 - Vận hành, di chuyển, độ tin cậy
| ID | Dự án | Nội dung | Phụ thuộc | Cỡ | Spec |
|---|---|---|---|---|---|
| P-40 | Sao lưu và khôi phục | Postgres (PITR), blob, system.db, khóa tenant; khôi phục từng mailbox; kiểm thử khôi phục định kỳ tự động | P-13, P-16 | XL | **Chưa** |
| P-41 | Công cụ nhập từ Zimbra | TGZ/IMAP/SOAP admin, ánh xạ folder, thư, lịch, danh bạ, bộ lọc, ACL; chạy lặp lại được | M1 trở đi; đầy đủ ở M4 | L-XL | **Chưa** |
| P-42 | Giám sát và vận hành | metric (Prometheus), nhật ký tập trung, cảnh báo, bảng điều khiển, quản lý hàng đợi, thống kê sử dụng, báo cáo | P-16 | L | **Chưa** |
| P-43 | Chuyển mailbox và phân tầng lưu trữ | job chuyển giữa node, tương đương HSM (thư cũ sang kho rẻ), `S3BlobStore`, đặt tên blob có khóa theo tenant | P-13, P-40 | XL | Một phần (container, Postgres-Valkey §3c.4b, §3e) |
| P-44 | Nhiều node và HA | `store` nhiều bản, Postgres HA, hàng đợi phân tán, kiểm thử bất biến I1-I8 | P-13, P-43 | XL | Có (distributed) |
| P-45 | Kubernetes | manifests, NetworkPolicy, operator Postgres, cert-manager hay `certd`, kho đối tượng | P-44 | L | **Chưa** (cố ý hoãn) |
| **M5** | **Cột mốc: sẵn sàng thay Zimbra thật** | nhập dữ liệu thật, sao lưu/khôi phục kiểm chứng, giám sát, HA | P-40..P-45 | | |

### Giai đoạn 6 - Mở rộng tùy chọn (cần quyết định riêng, mục 6)
| ID | Dự án | Ghi chú |
|---|---|---|
| P-50 | ~~Đồng bộ di động (ActiveSync/EAS)~~ | **RA KHỎI PHẠM VI (người dùng: dùng giao thức tiêu chuẩn là đủ).** Ghi chú cũ:  Giao thức thuộc Microsoft; kiểm tra điều khoản/bằng sáng chế/yêu cầu cấp phép TRƯỚC khi viết mã; nếu có rào cản pháp lý thì báo lại người dùng |
| P-51 | Cơ chế mở rộng (thay Zimlets) | Chọn mô hình an toàn (ví dụ iframe sandbox + API có phạm vi); có thể không làm |
| P-52 | Dịch vụ chat/họp | Zimbra FOSS không tích hợp sẵn theo cách thống nhất; ngoài phạm vi trừ khi người dùng muốn |
| P-53 | Lưu trữ tuân thủ/eDiscovery, DLP, lưu giữ pháp lý | Dành cho khách hàng doanh nghiệp |

## 3. Đồ thị phụ thuộc chính (rút gọn)
```
nền tảng -> P-01 -> P-02 -> { P-03, P-04, P-05 } -> P-06 -> [M1]
P-06 -> P-10 -> { P-11, P-12 }
P-02 -> P-13 -> P-14 -> { P-15, P-16 -> P-17 } -> [M2]
P-13 -> P-20 -> P-21 -> P-22 -> [M3 cùng P-23..P-28]
P-14,P-04 -> P-25 -> { P-26, P-30, P-31, P-34 }
P-31 + P-10 -> P-32 ; P-30..P-34 -> P-36 -> [M4]
P-13,P-16 -> P-40 ; P-13 -> P-43 -> P-44 -> P-45 ; P-41, P-42 -> [M5]
```
Song song hóa hợp lý (vì phụ thuộc độc lập): P-04 (IMAP) với P-05/P-06 (nhận/gửi); P-13 (Postgres) với P-04; P-10/P-11/P-12 với P-13/P-14; P-23, P-24, P-27, P-28 sau M2 chạy song song nếu có đủ nhân lực/agent. Hiện tại quy trình thực thi là tuần tự một agent tại một thời điểm trên cùng kho mã; chạy song song cần worktree riêng và quy tắc hợp nhất migration (đánh số tuần tự dễ xung đột).

## 4. Chiến lược chung
1. **Mỗi dự án con có spec + plan + review riêng**, giữ nguyên quy trình đã dùng (agent triển khai, reviewer độc lập, review phạm vi hẹp sau sửa, review cuối nhánh). Bắt buộc có bộ test âm cho mọi ràng buộc và test cô lập chéo mailbox/tenant.
2. **Sớm có mốc chạy được (M1)** thay vì làm đầy đủ từng tầng: M1 là điều kiện để phát hiện sai lầm thiết kế giao thức, vì hiện toàn bộ thiết kế phía trên `Mailbox` chưa từng chạy với client thật.
3. **Kiểm thử tương thích với client thật** là tiêu chí hoàn thành, không chỉ test đơn vị: Thunderbird, Apple Mail, Outlook (IMAP), K-9; kiểm thử giao thức bằng bộ có sẵn (imaptest, các bộ kiểm tra CalDAV/CardDAV như `caldav-tester`, JMAP test suite) nếu dùng được; ghi rõ cái nào không dùng được.
4. **Benchmark sớm và lặp lại:** benchmark SQLite với Postgres 18/Valkey (đã hoãn chờ máy rảnh) chạy ngay khi P-13 có backend; đặt SLO tạm (độ trễ, thông lượng) cùng người dùng trước M2.
5. **Bảo mật theo chu kỳ:** review bảo mật chuyên sâu cho mỗi dịch vụ giao thức (parser IMAP/SMTP/JMAP là bề mặt tấn công lớn nhất), fuzz bắt buộc cho mọi parser, kiểm thử xâm nhập bên ngoài trước M2 và M5, quy trình vá zero-day (spec container §8) diễn tập thật.
6. **Nợ kỹ thuật đã ghi sổ** (ledger SDD) được xếp vào dự án phù hợp thay vì để trôi: ví dụ phân trang, sổ chuyển id folder, kiểm tra Postgres đồng hồ `zero_since`, bảo vệ GC bằng DB cho `S3BlobStore`.
7. **Kiểm soát phạm vi:** chức năng mới của Zimbra chỉ vào lộ trình khi có người dùng xác nhận; mặc định không sao chép hành vi lạ của Zimbra nếu chuẩn công nghiệp đã có đường tốt hơn.

## 5. Rủi ro lớn của lộ trình
| Rủi ro | Giảm thiểu |
|---|---|
| Khối lượng rất lớn; IMAP, MTA, lịch (lặp lại, múi giờ, iTIP) mỗi cái là dự án nhiều tháng người | Làm M1 sớm, cắt phạm vi theo mức A/B/C, tái sử dụng thư viện Rust chất lượng cao khi giấy phép/bảo trì cho phép (đánh giá từng cái: IMAP/SMTP/JMAP codec, iCalendar, vCard, Sieve, tantivy) thay vì viết từ đầu |
| Phụ thuộc thư viện bên thứ ba trong parser giao thức → rủi ro chuỗi cung ứng | `cargo-deny/audit` đã có; ưu tiên ít phụ thuộc, bản chốt; fuzz thư viện được dùng |
| Thiết kế phía trên `Mailbox` chưa kiểm chứng với client thật | M1 sớm; sẵn sàng đổi API `store` (còn rẻ vì chưa có client) |
| Lịch/iTIP/múi giờ phức tạp, dễ sai lệch với client | Tập trung bộ kiểm thử tương thích; chọn thư viện iCalendar/timezone trưởng thành |
| Tìm kiếm + ACL + quyền riêng tư: dễ rò rỉ qua chỉ mục | Lọc theo quyền ở mọi đường đọc (đã ghi trong spec ACL), test chéo |
| Hai ngôn ngữ (Rust, Go) và nhiều thành phần (Postgres, Valkey, Traefik, rspamd, ClamAV) tăng bề mặt vận hành | Chính sách ngôn ngữ, ghim digest, quét, runbook vá; giới hạn số thành phần ngoài |
| Nhân lực/ngân sách agent có giới hạn (đã chạm giới hạn 5 giờ hai lần) | Chia nhỏ việc, ưu tiên theo cột mốc, tránh chạy review đắt khi không cần |
| Di chuyển từ Zimbra thật phức tạp (lịch, bộ lọc, ACL) | Làm P-41 tăng dần cùng mỗi nhóm tính năng, không dồn về cuối |

## 6. Các quyết định cần người dùng (trước khi mở các dự án tương ứng)
1. **Mức tương đương mong muốn:** A, B hay C (mục 1)? Quyết định cho P-30..P-36 và P-50..P-53.
2. **ActiveSync (P-50):** có cần đồng bộ di động kiểu Exchange không? Hay chấp nhận IMAP/CalDAV/CardDAV và JMAP cho di động?
3. **Zimlets (P-51):** thay bằng cơ chế mở rộng an toàn hay bỏ?
4. **SLO và quy mô mục tiêu:** số người dùng, thư/giây giờ cao điểm, mức sẵn sàng; dùng cho benchmark, kích cỡ HA và mức độ ưu tiên P-44.
5. **Chống thư rác/virus:** dùng rspamd + ClamAV (thành phần ngoài bằng C/C++, chạy container cô lập) hay tự viết bộ lọc nhỏ? Đề xuất dùng thành phần ngoài, sandbox chặt.
6. **Thư viện giao thức:** cho phép dùng thư viện Rust có sẵn (ví dụ họ crate của Stalwart cho IMAP/SMTP/JMAP/Sieve nếu giấy phép phù hợp) hay tự viết toàn bộ? Ảnh hưởng lớn đến cỡ và rủi ro; cần kiểm tra giấy phép (AGPL của một số phần sẽ ảnh hưởng) trước khi quyết.
7. **Định danh/SSO:** có cần tích hợp LDAP/AD bên ngoài để xác thực (Zimbra hỗ trợ) trước khi làm OIDC/SAML?
8. **Thứ tự:** có muốn đưa P-13 (Postgres) lên trước IMAP không, hay đi M1 trên SQLite rồi mới chuyển?

## 7. Bước kế tiếp đề xuất (không cần quyết định lớn)
1. Hoàn tất P-01 (P3 sửa xong, P4, P6, P5) và chạy benchmark ngắn.
2. Người dùng duyệt các spec đã viết (đặc biệt process-model, container, Postgres-Valkey, ACL, TLS, phân tán).
3. Mở **P-02 (`store` dịch vụ)** và **P-04/P-05 (IMAP, nhận thư)** bằng brainstorm + spec để hướng tới M1; đồng thời trả lời câu 6 (thư viện giao thức) vì nó quyết định cách viết P-04/P-05/P-06/P-21.
