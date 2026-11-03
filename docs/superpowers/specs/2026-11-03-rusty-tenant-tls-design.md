# Rusty - Quản lý chứng chỉ TLS theo từng domain (ACME tự động hoặc chứng chỉ tùy chỉnh)

Ngày: 2026-11-03. Trạng thái: chờ người dùng duyệt spec (sau đó mới lập kế hoạch triển khai chi tiết).

## 1. Mục tiêu và quyết định
Mỗi tenant (domain) tự chọn cách có chứng chỉ TLS cho các tên máy của mình (webmail, IMAP, POP3, SMTP...):
- **`acme`**: tự động xin và gia hạn chứng chỉ (Traefik, spec container §3b.5).
- **`custom`**: tenant tải lên chứng chỉ riêng (ví dụ chứng chỉ tổ chức, EV/OV, chứng chỉ do CA nội bộ cấp).

Phụ thuộc vào các spec trước: domain = tenant (spec Postgres–Valkey §3c), cổng vào Traefik (spec container §3b), vai trò `domain-admin`/`superadmin`, khóa mã hóa theo tenant (§3c.4b).

Ngoài phạm vi: chứng chỉ cho cổng STARTTLS (587/143/110) và cổng 25 (dịch vụ phía sau giữ chứng chỉ riêng, spec container §3b.5); DANE/MTA-STS; chứng chỉ client (mTLS của người dùng); HSM/KMS cho khóa riêng (giai đoạn sau).

## 2. Ràng buộc then chốt: ai được dùng tên máy nào
Nếu không kiểm soát, tenant A có thể đăng ký tên `mail.tenantB.com` và nhận hoặc cản chứng chỉ của B. Quy tắc:
- **Tên máy phải nằm trong cây tên của chính domain của tenant** (bằng đúng tên domain hoặc tên miền con của nó, ví dụ domain `example.com` được `mail.example.com`, `imap.example.com`, `*.example.com`), không phải tên tùy ý. Quyền sở hữu tên máy suy ra từ quyền sở hữu domain.
- **Quyền sở hữu domain phải được xác minh khi tạo domain** (bản ghi DNS TXT `_rusty-verify.<domain>` chứa mã ngẫu nhiên do hệ thống cấp, hoặc bằng chứng tương đương). Domain chưa xác minh **không** được đăng ký tên máy, không được xin chứng chỉ, và không nhận thư. Đây là điều kiện mới cần thêm vào quy trình tạo domain (spec tenant §3c).
- Tên máy duy nhất toàn hệ thống (`UNIQUE (hostname)`). Tên "vanity" ngoài cây tên của tenant (ví dụ `mail.congty.vn` trỏ về domain `example.com`) cần xác minh bổ sung (CNAME cộng TXT) và do `superadmin` phê duyệt; chưa làm ở phiên bản đầu.
- Mỗi tenant bị giới hạn số tên máy (mặc định 20, trần hệ thống cấu hình được) để không làm cạn hạn mức CA và bộ nhớ của biên.

## 3. Mô hình dữ liệu (Postgres)
```sql
CREATE TABLE domain_hostnames (
  id            bigint PRIMARY KEY GENERATED ALWAYS AS IDENTITY,
  domain_id     bigint NOT NULL REFERENCES domains(id) ON DELETE RESTRICT,
  hostname      text   NOT NULL UNIQUE CHECK (hostname = lower(hostname) AND length(hostname) BETWEEN 3 AND 253),
  purpose       text   NOT NULL CHECK (purpose IN ('web','imap','pop3','smtp-submit','admin')),
  tls_mode      text   NOT NULL CHECK (tls_mode IN ('acme','custom')),
  status        text   NOT NULL CHECK (status IN ('pending','active','error','disabled')),
  last_error    text   NULL CHECK (length(last_error) <= 512),
  created_at    bigint NOT NULL,
  UNIQUE (id, domain_id)
);
CREATE TABLE tls_certificates (
  id              bigint PRIMARY KEY GENERATED ALWAYS AS IDENTITY,
  hostname_id     bigint NOT NULL,
  domain_id       bigint NOT NULL,
  kind            text   NOT NULL CHECK (kind IN ('custom')),       -- chứng chỉ ACME do Traefik giữ, không lưu ở đây
  fingerprint     bytea  NOT NULL CHECK (length(fingerprint) = 32), -- SHA-256 của chứng chỉ lá (DER)
  not_before      bigint NOT NULL,
  not_after       bigint NOT NULL CHECK (not_after > not_before),
  sans            text   NOT NULL CHECK (length(sans) <= 4096),     -- JSON mảng tên
  chain_pem       text   NOT NULL CHECK (length(chain_pem) <= 65536), -- chỉ phần công khai
  key_ciphertext  bytea  NOT NULL,                                  -- khóa riêng, AEAD (mục 5)
  key_id          integer NOT NULL,
  active          boolean NOT NULL,
  uploaded_by     bigint NOT NULL,
  created_at      bigint NOT NULL,
  FOREIGN KEY (hostname_id, domain_id) REFERENCES domain_hostnames(id, domain_id) ON DELETE CASCADE
);
CREATE UNIQUE INDEX one_active_cert ON tls_certificates (hostname_id) WHERE active;  -- tối đa một chứng chỉ đang dùng
```
- Chế độ `custom` yêu cầu có một `tls_certificates` đang `active`; chuyển `acme` -> `custom` hoặc ngược lại là thao tác nguyên tử (một giao dịch) và có kiểm toán.
- Giữ tối đa 5 phiên bản chứng chỉ không hoạt động mỗi tên máy (để quay lại), tự dọn cái cũ hơn.
- `domain_hostnames.status` phản ánh trạng thái cấp chứng chỉ (`pending` đang xin, `error` kèm `last_error` đã làm sạch, `active` đã có chứng chỉ hợp lệ).
- Giới hạn: 20 tên máy mỗi tenant (cấu hình), ràng buộc kiểm tra trong cùng giao dịch.

## 4. Kiểm tra chứng chỉ tùy chỉnh khi tải lên (bắt buộc, từ chối nếu vi phạm bất kỳ điều nào)
1. Kích thước: PEM tối đa 64 KiB, chuỗi tối đa 10 chứng chỉ, khóa riêng tối đa 8 KiB; định dạng PEM hợp lệ (không nhận DER thô hay định dạng lạ); không nhận khóa mã hóa bằng mật khẩu (yêu cầu giải mã trước khi tải, hoặc hỗ trợ có kiểm soát sau).
2. **Khóa riêng khớp khóa công khai** của chứng chỉ lá (ký thử một thông điệp ngẫu nhiên).
3. **Chuỗi:** chứng chỉ lá đứng đầu, các chứng chỉ trung gian đúng thứ tự, mỗi chứng chỉ do chứng chỉ kế tiếp ký; không chứa chứng chỉ gốc tự ký ở cuối thừa (cảnh báo, loại bỏ); không chứa khóa riêng khác.
4. **Tên:** SAN (hoặc CN khi không có SAN, cảnh báo) phải **phủ tên máy đang gán**; các SAN thừa được phép nhưng không bao giờ được dùng cho tên máy khác của hệ thống; wildcard chỉ hợp lệ khi tenant sở hữu domain gốc.
5. **Thời hạn:** `notBefore` không ở tương lai quá 1 ngày, `notAfter` còn ít nhất 1 ngày; thời hạn hiệu lực tối đa 398 ngày (cảnh báo, không từ chối nếu CA nội bộ; cấu hình được).
6. **Thuật toán:** RSA >= 2048 bit, ECDSA P-256/P-384, Ed25519; từ chối chữ ký MD5/SHA-1; từ chối khóa yếu hoặc tham số lạ.
7. **Loại chứng chỉ:** lá phải `CA:FALSE`, có `serverAuth` trong EKU (nếu EKU có mặt) và KeyUsage phù hợp; từ chối chứng chỉ CA được dùng làm chứng chỉ máy chủ.
8. **Tin cậy (tùy chọn, bật được):** chuỗi dẫn tới gốc nằm trong kho tin cậy cấu hình được; mặc định chỉ cảnh báo (nhiều tổ chức dùng CA nội bộ).
9. Mọi lỗi trả thông báo cố định theo mã lý do (`key_mismatch`, `san_mismatch`, `expired`, ...), không chép lại nội dung chứng chỉ hay khóa.
Thư viện: phân tích X.509 và PEM bằng crate Rust an toàn bộ nhớ đã được bảo trì (ví dụ `x509-parser`, `rustls-pemfile`/`rustls`), chạy trong dịch vụ `certd` đã cô lập; fuzz bộ phân tích đầu vào.

## 5. Lưu trữ và bảo vệ khóa riêng
- Khóa riêng của chứng chỉ tùy chỉnh **không bao giờ** nằm dạng rõ trong DB, log, sao lưu, API trả về hay nhật ký kiểm toán. Mã hóa AEAD (XChaCha20-Poly1305 hoặc AES-256-GCM) bằng khóa dẫn xuất theo tenant (`HKDF(master, "tenant:{domain_id}/tls")`); **dữ liệu liên kết (AAD) gồm `domain_id`, `hostname_id`, `fingerprint`, `key_id`** để không hoán đổi được giữa các bản ghi.
- API **không bao giờ trả lại khóa riêng** (chỉ trả dấu vân tay, tên, thời hạn, trạng thái). Tải xuống khóa không được hỗ trợ.
- Giải mã chỉ xảy ra trong `store` khi phục vụ yêu cầu của vai trò `certd` (mục 6) qua kênh mTLS nội bộ; không ghi ra đĩa thường.
- Hủy khóa tenant (crypto-shredding, spec §3c.4b) làm mọi khóa TLS của tenant không đọc lại được; xóa tên máy hoặc chứng chỉ xóa hàng và dọn tệp phục vụ ngay.
- Xoay khóa chính (`key_id`) là job nền: giải mã bằng khóa cũ, mã hóa lại bằng khóa mới, trong một giao dịch từng chứng chỉ.

## 6. Thành phần `certd` và tích hợp với Traefik
Traefik tự xin chứng chỉ ACME cho tên máy trong danh sách cho phép, nhưng **chứng chỉ tùy chỉnh và danh sách tên máy động theo tenant** cần một mặt phẳng điều khiển. Thêm dịch vụ **`certd`** (Rust, container riêng, mạng `app`, không công bố cổng):
- Đọc từ `store` (vai trò mới `certd`, chỉ đọc): danh sách `domain_hostnames` (đang `active`/`pending`), chứng chỉ tùy chỉnh đang `active` (khóa đã giải mã trong bộ nhớ qua kênh mTLS).
- **Sinh cấu hình động cho Traefik** vào một volume `tmpfs`: với tên máy `acme`, tạo router với `certResolver`; với tên máy `custom`, ghi PEM chuỗi + khóa và khai báo `tls.certificates`; mỗi tên máy thuộc **đúng một** chế độ (không vừa có `certResolver` vừa có chứng chỉ tùy chỉnh). Ghi nguyên tử (tệp tạm rồi đổi tên), quyền `0400`, thuộc user của `certd`, gắn **chỉ-đọc** vào `edge`. Traefik dùng file provider có `watch`, nên áp dụng ngay không cần khởi động lại.
- **Khóa riêng chỉ tồn tại trong bộ nhớ của `store`/`certd` và trên `tmpfs` của volume chia sẻ với `edge`**, không bao giờ trên đĩa bền; `certd` khởi động lại thì dựng lại toàn bộ từ DB.
- Giám sát hạn dùng: quét chứng chỉ tùy chỉnh và (bằng kết nối TLS thử tới chính tên máy công khai) cả chứng chỉ ACME; cảnh báo cho `domain-admin` ở còn 30, 14, 7, 1 ngày và khi hết hạn; khi chứng chỉ tùy chỉnh đã hết hạn, **không phục vụ chứng chỉ hết hạn lâu dài**: trạng thái `error`, và nếu tenant bật `fallback_to_acme` thì chuyển sang ACME (mặc định tắt, vì chuyển chế độ âm thầm có thể bất ngờ).
- Giới hạn tốc độ xin ACME theo tenant và toàn hệ thống (hàng đợi, backoff, tôn trọng giới hạn của CA); trạng thái lỗi cấp chứng chỉ hiển thị trong console.
- `certd` là **thành phần giá trị cao**: nắm khóa riêng của mọi tenant có chứng chỉ tùy chỉnh. Cô lập chặt (user riêng, rootfs chỉ đọc, `cap_drop ALL`, sandbox, không ra Internet, chỉ nối `store` và ghi volume tmpfs), phạm vi mã nhỏ, fuzz bộ phân tích, review bảo mật ưu tiên.
- Khi nhiều bản `edge` (HA), `certd` giữ vai trò duy nhất phân phối chứng chỉ (và, nếu cần, tự làm ACME); xem hạn chế ACME của Traefik ở spec container §3b.5.

Vai trò mới cho ma trận của `store` (spec mô hình tiến trình §7b):
| Vai trò | Được làm | Không được làm |
|---|---|---|
| `certd` | đọc tên máy và chứng chỉ đang dùng (kể cả khóa giải mã, chỉ qua mTLS), ghi trạng thái cấp chứng chỉ và hạn đã quan sát | đọc thư, tài khoản, đổi cấu hình tenant |
| `domain-admin(D)` | thêm/xóa tên máy của D, chọn chế độ, tải lên/xóa/kích hoạt chứng chỉ của D, xem trạng thái và cảnh báo | xem khóa riêng, thao tác tên máy hoặc chứng chỉ của tenant khác (trả `NotFound`) |
| `superadmin` | mọi thứ trên mọi tenant (trừ xem khóa riêng), đặt trần và kho tin cậy, phê duyệt tên vanity | xem khóa riêng |

## 7. Quản trị
- `rustyctl tls host add|list|remove|set-mode`, `rustyctl tls cert upload|show|activate|delete|history`, `rustyctl tls status [--expiring N]`, `rustyctl tls verify <hostname>` (kết nối thử và so khớp chứng chỉ đang được phục vụ); admin console có các màn tương ứng.
- Mọi thao tác ghi `admin_audit` (ai, tenant, tên máy, hành động, dấu vân tay; **không bao giờ khóa hay PEM**).

## 8. Kiểm thử
- **Ma trận kiểm tra chứng chỉ (mục 4):** khóa không khớp, hết hạn, chưa có hiệu lực, SAN không phủ tên máy, `CA:TRUE`, RSA 1024, chữ ký SHA-1, PEM hỏng, đầu vào khổng lồ, khóa mã hóa mật khẩu, chuỗi sai thứ tự, chuỗi thiếu trung gian; mỗi trường hợp có mã lý do cố định, không lộ nội dung; và trường hợp hợp lệ (RSA, ECDSA, Ed25519, wildcard).
- **Sở hữu tên máy:** tên ngoài cây tên của tenant bị từ chối; domain chưa xác minh không đăng ký được tên máy; trùng tên giữa hai tenant bị từ chối bằng ràng buộc DB; vượt giới hạn tên máy mỗi tenant -> `Quota`.
- **Cô lập chéo tenant:** A không xem, tải lên, kích hoạt hoặc xóa chứng chỉ/tên máy của B; ngay cả khi biết id (trả `NotFound`); không gán chứng chỉ của A cho tên máy của B (khóa ngoại tổng hợp).
- **Khóa riêng không rò:** quét toàn bộ cột DB, log, phản hồi API, `admin_audit` và đĩa của `certd`/`edge` (ngoài tmpfs) không tìm thấy byte của khóa; kiểm tra AAD (hoán đổi `key_ciphertext` giữa hai hàng thì giải mã thất bại); crypto-shredding làm chứng chỉ không dùng được.
- **Chế độ loại trừ:** một tên máy không thể vừa `acme` vừa `custom`; chuyển chế độ nguyên tử.
- **Phục vụ thật (Compose + Pebble):** `custom` trả đúng chứng chỉ tải lên trên 443/993/995/465 (máy khách TLS thật kiểm tra dấu vân tay); `acme` trả chứng chỉ của Pebble; thay chứng chỉ tùy chỉnh có hiệu lực không đứt kết nối đang có; xóa tên máy ngừng phục vụ; `certd` khởi động lại dựng lại đúng; chứng chỉ hết hạn -> `error` và (nếu bật) rơi về ACME; không có tệp khóa nào trên đĩa bền; quyền tệp `0400`, tmpfs `noexec`.
- **Cảnh báo hạn:** mốc 30/14/7/1 ngày và hết hạn; chống gửi trùng.
- **Fuzz:** bộ phân tích PEM/X.509 và bộ kiểm tra tải lên (không panic, giới hạn bộ nhớ).

## 9. Phân kỳ triển khai (sau khi duyệt)
1. Xác minh quyền sở hữu domain (TXT) và trạng thái domain `verified` (điều kiện cho mọi thứ dưới).
2. Migration `domain_hostnames`, `tls_certificates`, ràng buộc, giới hạn; test âm.
3. Thư viện kiểm tra chứng chỉ tải lên + fuzz.
4. Mã hóa khóa riêng theo tenant (AEAD, AAD, xoay khóa).
5. API `store` và vai trò `certd`, `domain-admin`; kiểm toán.
6. `certd`: sinh cấu hình Traefik (tmpfs, nguyên tử), giám sát hạn, cảnh báo.
7. Tích hợp với `edge` và Pebble (mở rộng task E2b); `rustyctl tls ...`; console.

## 10. Rủi ro và điểm mở
- `certd` và `store` cùng giữ khóa riêng của tenant; một lỗ hổng ở đó lộ khóa TLS. Giảm bằng cô lập, mã hóa lúc lưu, tmpfs và không bao giờ trả khóa qua API công khai; chấp nhận rủi ro còn lại hoặc chuyển sang KMS/HSM sau.
- Tenant tải lên chứng chỉ do CA lạ hoặc thu hồi: chỉ cảnh báo; không kiểm tra thu hồi (OCSP/CRL) ở phiên bản đầu.
- Hạn mức CA công cộng khi nhiều tenant dùng `acme`: cần giới hạn theo tenant và bộ nhớ đệm trạng thái; xem xét chứng chỉ wildcard (DNS-01) cho domain gốc để giảm số chứng chỉ.
- Phụ thuộc hạn chế HA của ACME trong Traefik OSS: khi cần nhiều `edge`, chuyển việc ACME sang `certd`.
- Tên "vanity" (ngoài cây tên của tenant), chứng chỉ cho cổng STARTTLS, DANE/MTA-STS: để sau.
