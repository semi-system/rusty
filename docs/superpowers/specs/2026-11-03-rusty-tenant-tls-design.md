# Rusty - Quản lý chứng chỉ TLS theo từng domain (ACME tự động hoặc chứng chỉ tùy chỉnh)

Ngày: 2026-11-03. Trạng thái: chờ người dùng duyệt spec (sau đó mới lập kế hoạch triển khai chi tiết).

## 1. Mục tiêu và quyết định
Mỗi tenant (domain) tự chọn cách có chứng chỉ TLS cho các tên máy của mình (webmail, IMAP, POP3, SMTP...):
- **`acme`**: tự động xin và gia hạn chứng chỉ, do **`certd`** thực hiện (mục 6); Traefik chỉ nạp chứng chỉ từ tệp.
- **`custom`**: tenant tải lên chứng chỉ riêng (ví dụ chứng chỉ tổ chức, EV/OV, chứng chỉ do CA nội bộ cấp).

Phụ thuộc vào các spec trước: domain = tenant (spec Postgres–Valkey §3c), cổng vào Traefik (spec container §3b), vai trò `domain-admin`/`superadmin`, khóa mã hóa theo tenant (§3c.4b).

**Quyết định về ngôn ngữ (người dùng, 2026-11-03): `certd` viết bằng Go**, vì hệ sinh thái ACME của Go (`lego`, `certmagic`) trưởng thành hơn; bù lại bằng CI siết chặt (mục 6b) và biện pháp bảo vệ khóa riêng trong bộ nhớ (mục 5). Hệ quả: **việc xin chứng chỉ ACME chuyển từ Traefik sang `certd`**, Traefik không còn dùng `certificatesResolvers`.

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
Thư viện: phân tích X.509 và PEM bằng thư viện chuẩn của Go (`crypto/x509`, `encoding/pem`), không dùng thư viện phân tích bên thứ ba; chạy trong dịch vụ `certd` đã cô lập; fuzz bằng bộ fuzz gốc của Go (`go test -fuzz`) cho mọi hàm nhận đầu vào từ tenant.

## 5. Lưu trữ và bảo vệ khóa riêng
- Khóa riêng của chứng chỉ tùy chỉnh **không bao giờ** nằm dạng rõ trong DB, log, sao lưu, API trả về hay nhật ký kiểm toán. Mã hóa AEAD (XChaCha20-Poly1305 hoặc AES-256-GCM) bằng khóa dẫn xuất theo tenant (`HKDF(master, "tenant:{domain_id}/tls")`); **dữ liệu liên kết (AAD) gồm `domain_id`, `hostname_id`, `fingerprint`, `key_id`** để không hoán đổi được giữa các bản ghi.
- API **không bao giờ trả lại khóa riêng** (chỉ trả dấu vân tay, tên, thời hạn, trạng thái). Tải xuống khóa không được hỗ trợ.
- Giải mã chỉ xảy ra trong `store` khi phục vụ yêu cầu của vai trò `certd` (mục 6) qua kênh mTLS nội bộ; không ghi ra đĩa thường.
- **Giới hạn của Go cần biết:** bộ gom rác của Go có thể sao chép dữ liệu trong bộ nhớ nên **không thể đảm bảo xóa sạch khóa riêng khỏi bộ nhớ** như Rust. Biện pháp giảm thiểu bắt buộc cho `certd`: giữ khóa dưới dạng `[]byte` trong thời gian ngắn nhất, không chuyển sang `string`, ghi đè (`clear`) ngay sau khi dùng; tắt core dump (`RLIMIT_CORE=0`, `PR_SET_DUMPABLE=0`) và không cấp swap cho container (`memswap_limit = mem_limit`); không đưa khóa vào log, lỗi hay metric; hạn chế thời gian sống của tiến trình giữ khóa (khởi động lại định kỳ được phép vì dựng lại từ DB). Rủi ro còn lại (khóa tồn tại trong bộ nhớ tiến trình vài lần) được chấp nhận và ghi vào mô hình đe dọa; nếu không chấp nhận được thì khóa phải chuyển sang KMS/HSM.
- Hủy khóa tenant (crypto-shredding, spec §3c.4b) làm mọi khóa TLS của tenant không đọc lại được; xóa tên máy hoặc chứng chỉ xóa hàng và dọn tệp phục vụ ngay.
- Xoay khóa chính (`key_id`) là job nền: giải mã bằng khóa cũ, mã hóa lại bằng khóa mới, trong một giao dịch từng chứng chỉ.

## 6. Thành phần `certd` và tích hợp với Traefik
Tên máy và chứng chỉ thay đổi theo tenant lúc chạy, nên cần một mặt phẳng điều khiển. Thêm dịch vụ **`certd`** (**Go**, thư viện ACME `lego` hoặc `certmagic`, container riêng, mạng `app`, không công bố cổng). `certd` **vừa xin/gia hạn chứng chỉ ACME vừa quản lý chứng chỉ tùy chỉnh**; Traefik chỉ nạp chứng chỉ từ tệp (file provider), không có `certificatesResolvers`. Lợi ích: khóa tài khoản ACME và thông tin DNS-01 nằm ở thành phần nội bộ thay vì ở `edge` hướng Internet; hạn mức theo tenant kiểm soát tập trung; nhiều bản `edge` dùng chung được một nguồn chứng chỉ (hết giới hạn HA của ACME trong Traefik OSS).
- Đọc từ `store` (vai trò mới `certd`, chỉ đọc): danh sách `domain_hostnames` (đang `active`/`pending`), chứng chỉ tùy chỉnh đang `active` (khóa đã giải mã trong bộ nhớ qua kênh mTLS).
- **Sinh cấu hình động cho Traefik** vào một volume `tmpfs`: với **cả hai** chế độ `acme` và `custom`, ghi PEM chuỗi + khóa và khai báo `tls.certificates` (chứng chỉ ACME do chính `certd` lấy được); mỗi tên máy thuộc **đúng một** chế độ.
- **ACME trong `certd`:** loại thử thách **HTTP-01** (Traefik có một router cố định chuyển `/.well-known/acme-challenge/` của các tên máy trong danh sách cho phép tới máy chủ thử thách nội bộ của `certd`) và **DNS-01** (cho wildcard hoặc khi biên không với tới từ Internet; token DNS giới hạn theo vùng, nằm trong tệp bí mật `0400` chỉ `certd` đọc). **TLS-ALPN-01 không dùng** (đòi hỏi biên xử lý ALPN đặc biệt, phức tạp hơn lợi ích). Khóa tài khoản ACME, khóa chứng chỉ ACME và trạng thái nằm trong `store` (mã hóa như mục 5) hoặc kho riêng của `certd`, không nằm ở `edge`. Ghi nguyên tử (tệp tạm rồi đổi tên), quyền `0400`, thuộc user của `certd`, gắn **chỉ-đọc** vào `edge`. Traefik dùng file provider có `watch`, nên áp dụng ngay không cần khởi động lại.
- **Khóa riêng chỉ tồn tại trong bộ nhớ của `store`/`certd` và trên `tmpfs` của volume chia sẻ với `edge`**, không bao giờ trên đĩa bền; `certd` khởi động lại thì dựng lại toàn bộ từ DB.
- Giám sát hạn dùng: quét chứng chỉ tùy chỉnh và (bằng kết nối TLS thử tới chính tên máy công khai) cả chứng chỉ ACME; cảnh báo cho `domain-admin` ở còn 30, 14, 7, 1 ngày và khi hết hạn; khi chứng chỉ tùy chỉnh đã hết hạn, **không phục vụ chứng chỉ hết hạn lâu dài**: trạng thái `error`, và nếu tenant bật `fallback_to_acme` thì chuyển sang ACME (mặc định tắt, vì chuyển chế độ âm thầm có thể bất ngờ).
- Giới hạn tốc độ xin ACME theo tenant và toàn hệ thống (hàng đợi, backoff, tôn trọng giới hạn của CA); trạng thái lỗi cấp chứng chỉ hiển thị trong console.
- `certd` là **thành phần giá trị cao**: nắm khóa riêng của mọi tenant có chứng chỉ tùy chỉnh. Cô lập chặt (user riêng, rootfs chỉ đọc, `cap_drop ALL`, sandbox, chỉ nối `store` và ghi volume tmpfs), phạm vi mã nhỏ, fuzz bộ phân tích, review bảo mật ưu tiên. **Đường ra Internet bị giới hạn bằng danh sách cho phép:** chỉ tới các máy chủ ACME của CA đã cấu hình và (nếu dùng DNS-01) API của nhà cung cấp DNS đó, thực thi bằng mạng egress riêng/chính sách mạng; mọi đích khác bị chặn.
- Nhiều bản `edge` (HA): `certd` là nguồn chứng chỉ duy nhất; với các `edge` cùng một máy dùng chung volume tmpfs, với `edge` khác máy thì lấy chứng chỉ qua API xác thực mTLS của `certd` (chi tiết ở giai đoạn HA).

Vai trò mới cho ma trận của `store` (spec mô hình tiến trình §7b):
| Vai trò | Được làm | Không được làm |
|---|---|---|
| `certd` | đọc tên máy và chứng chỉ đang dùng (kể cả khóa giải mã, chỉ qua mTLS), ghi trạng thái cấp chứng chỉ và hạn đã quan sát | đọc thư, tài khoản, đổi cấu hình tenant |
| `domain-admin(D)` | thêm/xóa tên máy của D, chọn chế độ, tải lên/xóa/kích hoạt chứng chỉ của D, xem trạng thái và cảnh báo | xem khóa riêng, thao tác tên máy hoặc chứng chỉ của tenant khác (trả `NotFound`) |
| `superadmin` | mọi thứ trên mọi tenant (trừ xem khóa riêng), đặt trần và kho tin cậy, phê duyệt tên vanity | xem khóa riêng |

## 6b. Ràng buộc CI và chuỗi cung ứng cho thành phần Go (siết chặt, bắt buộc)
Vì `certd` là thành phần Go duy nhất của dự án (cho tới nay) và giữ khóa riêng của mọi tenant, CI của nó phải chặt ít nhất bằng CI của phần Rust:
- **Phiên bản cố định:** `go.mod` khai báo `go` và `toolchain` cụ thể; image builder ghim digest; nâng cấp có chủ đích kèm chạy toàn bộ CI.
- **Phụ thuộc:** `go mod verify` và `GOFLAGS=-mod=readonly` (hoặc `-mod=vendor` với thư mục `vendor/` được commit và kiểm tra khớp `go.sum`); không dùng `replace` tới đường dẫn ngoài; sumdb mặc định bật; danh sách phụ thuộc trực tiếp có hạn mức (mặc định tối đa 15 module trực tiếp, thêm cái mới cần lý do trong PR); chính sách giấy phép cho phép (MIT, BSD-2/3, Apache-2.0, ISC, MPL-2.0) kiểm bằng `go-licenses`.
- **Lỗ hổng:** `govulncheck ./...` **thất bại CI nếu có lỗ hổng đến được từ mã của ta**; chạy theo lịch hằng ngày (không chỉ lúc có commit) vì lỗ hổng mới xuất hiện sau khi phát hành; cảnh báo vào kênh bảo mật.
- **Phân tích tĩnh:** `go vet`, `staticcheck`, `golangci-lint` bật tối thiểu `errcheck` (không bỏ qua lỗi), `gosec`, `govet`, `ineffassign`, `bodyclose`, `noctx`, `forbidigo`; lint thất bại là CI thất bại, không có `//nolint` không giải thích.
- **Kiểm thử:** `go test -race -count=1 ./...` bắt buộc; bao phủ nhánh lỗi; test thuộc tính cho kiểm tra chứng chỉ; **fuzz gốc của Go** (`go test -fuzz`) tối thiểu 60 giây cho mỗi hàm phân tích đầu vào từ tenant (PEM, X.509, kiểm tra tải lên, tên máy), corpus hạt giống được commit.
- **Cấm có kiểm tra tự động:** import `unsafe` hoặc `cgo` trong mã của dự án (quét mã nguồn trong CI; thư viện chuẩn và phụ thuộc có `unsafe` được chấp nhận và liệt kê), `reflect` để vượt kiểm soát kiểu, `os/exec`, `net/http/pprof` hay `expvar` trong bản phát hành, biến toàn cục có thể thay đổi giữ bí mật.
- **Bản dựng:** `CGO_ENABLED=0 go build -trimpath -ldflags="-s -w -buildid="` cho binary tĩnh, tái lập được; SBOM; chữ ký cosign; image `scratch` hoặc distroless tĩnh; cùng cứng hóa container như các dịch vụ Rust (mục 4 spec container).
- **Chạy lúc thực thi:** tắt core dump, không swap, `GOMAXPROCS`/giới hạn bộ nhớ đặt theo cgroup, `GOGC` mặc định; tiến trình không bao giờ bật biên dịch động hay nạp plugin.
- **Báo cáo:** mọi cảnh báo CI của `certd` hiển thị cùng bảng điều khiển với phần Rust; một bản phát hành chỉ ký khi toàn bộ cổng trên xanh.

## 6c. Dashboard hạn chứng chỉ và công tắc failover ACME (yêu cầu của người dùng, 2026-11-03)
- **Dashboard của admin tenant** (chỉ tenant của họ) và **của super admin** (toàn hệ thống, lọc theo tenant) hiển thị mọi tên máy cùng: chế độ (`acme`/`custom`), nhà cấp, ngày hết hạn, số ngày còn lại, trạng thái (`active/pending/error`), lỗi cấp gần nhất (đã làm sạch); sắp xếp mặc định theo hạn tăng dần; làm nổi bật mức cảnh báo 30/14/7/1 ngày và đã hết hạn; có bộ lọc "sắp hết hạn trong N ngày".
- **Nút bật/tắt failover ACME** (`fallback_to_acme`) cho từng tên máy `custom`: bật thì khi chứng chỉ tùy chỉnh hết hạn hoặc sắp hết hạn (ngưỡng cấu hình, mặc định 7 ngày) `certd` tự xin chứng chỉ ACME cho tên máy đó và chuyển sang phục vụ nó, ghi `admin_audit` và gửi thông báo; **mặc định vẫn là TẮT**. Chuyển về chứng chỉ tùy chỉnh khi tenant tải lên chứng chỉ mới hợp lệ (có tùy chọn tự quay lại). Tenant admin bật/tắt cho tên máy của mình; super admin có thể đặt **chính sách tối thiểu** (cho phép hoặc cấm failover ACME theo toàn hệ thống hoặc theo tenant).
- Dữ liệu cho dashboard lấy từ `store` (hạn đã quan sát bởi `certd`), API theo vai trò; không bao giờ trả khóa riêng.
- Kiểm thử: dashboard chỉ thấy tên máy của tenant (chéo tenant trả NotFound), thứ tự theo hạn, mốc cảnh báo, công tắc đổi trạng thái có kiểm toán, failover chạy đúng bằng Pebble (chứng chỉ tùy chỉnh hết hạn -> chuyển ACME; tắt công tắc -> không chuyển, trạng thái `error`).

## 6d. KMS (yêu cầu của người dùng: dựng sẵn như một microservice)
Khóa chính của tenant, khóa mã hóa giá trị bí mật (khóa riêng TLS, thông tin xác thực SMTP relay, khóa tài khoản ACME) và khóa dẫn xuất cho cache **không còn nằm trong tệp**: được bảo vệ bởi dịch vụ `kms`. Thiết kế chi tiết sẽ viết thành spec riêng (đang chờ quyết định: tự viết `kms` tối thiểu bằng Rust hay triển khai OpenBao). Yêu cầu chung: mã hóa phong bì (envelope), khóa dữ liệu bọc theo tenant và phiên bản, API `wrap/unwrap/derive/rotate` có xác thực mTLS và vai trò, kiểm toán mọi lần dùng, khóa gốc được niêm phong và mở bằng thủ tục có kiểm soát, không bao giờ xuất khóa gốc.

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
- Hạn chế HA của ACME trong Traefik OSS đã được giải quyết bằng cách đặt ACME trong `certd`; rủi ro mới: `certd` có đường ra Internet (giới hạn bằng danh sách cho phép) và giữ khóa ACME.
- Tên "vanity" (ngoài cây tên của tenant), chứng chỉ cho cổng STARTTLS, DANE/MTA-STS: để sau.
