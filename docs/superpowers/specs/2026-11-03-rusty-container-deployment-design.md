# Rusty - Triển khai bằng container (không phụ thuộc distro)

Ngày: 2026-11-03. Trạng thái: chờ người dùng duyệt spec (sau đó mới lập kế hoạch triển khai chi tiết).

## 1. Mục tiêu và quyết định
Quyết định của người dùng: dùng Docker/OCI; hệ thống **không phụ thuộc distro của máy chủ**; chọn phương án **A: mỗi dịch vụ một container, `rustyctl` chạy trên máy host**.

- Host chỉ cần: runtime container (Docker hoặc Podman, ưu tiên rootless) và kernel đủ tính năng (cgroup v2, seccomp, user namespace; Landlock nếu có).
- Mọi thành phần của Rusty chạy trong container từ image được build lặp lại được, ghim digest, ký và quét.
- Postgres 18 và Valkey chạy trong container riêng từ image chính thức ghim digest.
- **Không bao giờ mount `docker.sock` (hay socket của Podman) vào bất kỳ container nào**: đó là quyền root tương đương trên host.
- Con đường lên Kubernetes được giữ mở (mục 9), nhưng triển khai mục tiêu đầu tiên là Docker Compose trên một máy.

Ngoài phạm vi: điều phối đa node, HA của Postgres, chọn nhà cung cấp lưu trữ khối/đối tượng cụ thể.

## 2. Hình ảnh (image)
- **Build:** trong một image builder cố định (ghim digest), biên dịch **musl tĩnh** (`x86_64-unknown-linux-musl`, đã có trong `scripts/ci.sh`), có `--locked`, không truy cập mạng ngoài kho crate đã khóa. Kết quả được băm, kèm SBOM và chữ ký (cosign); nguồn gốc build (provenance) được ghi lại.
- **Image chạy:** nền `scratch` hoặc distroless tĩnh: không shell, không trình quản lý gói, không thư viện hệ thống ngoài thứ cần (chứng chỉ CA, `/etc/passwd` tối thiểu). Chỉ một tệp thực thi + cấu hình mẫu.
- **Một image mỗi dịch vụ, binary riêng mỗi dịch vụ:** thay vì một binary multi-call chứa mọi giao thức trong mọi container, build mỗi dịch vụ từ cùng workspace bằng *cargo feature* (`rusty-imap` chỉ có IMAP, không có driver Postgres, không có DKIM...). Container IMAP bị khai thác không chứa sẵn mã của dịch vụ khác. Đây là điều chỉnh so với quyết định "một binary multi-call" của spec mô hình tiến trình; binary multi-call vẫn dùng cho dev và chế độ không container.
- **Postgres, Valkey:** image chính thức; Postgres bản Debian (glibc, đồng nhất sắp xếp chuỗi), Valkey bản chính thức; ghim theo digest, tag chỉ để đọc; cập nhật bằng quy trình có kiểm tra (mục 8).
- **Quét và cập nhật:** CI quét lỗ hổng (Trivy hoặc Grype) cho mọi image và chặn nếu có lỗ hổng mức cao đã có bản vá; có kiểm tra cảnh báo "image cũ": digest đang chạy so với digest mới nhất đã duyệt.

## 3. Dịch vụ và container

| Container | Vai trò | Mạng | Dữ liệu | Ghi chú |
|---|---|---|---|---|
| `store` | duy nhất chạm Postgres/Valkey/blob/system.db | `app`, `data` | volume `blobs`, `sysdb` | mục tiêu giá trị cao nhất; sandbox chặt nhất |
| `imap`, `jmap`, `dav`, `smtp-in`, `smtp-submit`, `lmtp`, `filter`, `gc`, `admin-api` | dịch vụ giao thức/chức năng | `app` (và `edge` cho cổng công khai) | không có | chỉ nói chuyện với `store` |
| `edge` (Traefik, mục 3b) | **cổng công khai duy nhất** (25/465/587/993/995/443/80) | `edge`, `app` | không | chuyển tiếp TCP nguyên vẹn cho thư; kết thúc TLS cho HTTP |
| `postgres` | nguồn sự thật | `data` | volume `pgdata` | không công bố cổng ra host |
| `valkey` | cache | `data` | không (không persistence) | chỉ nghe socket/mạng `data` |
| `backup` | job (pgBackRest hoặc tương đương) + sao lưu system.db thống nhất | `data` | volume `backups` hoặc kho ngoài | chạy theo lịch, vai trò DB sao lưu riêng |
| `dbadmin` | job vận hành: cấp vai trò DB, sinh ACL Valkey, xoay chứng chỉ, điều phối migration | `data` | không | bằng Rust, cùng workspace, nằm ngoài đường dữ liệu |

Mạng Docker (`edge` ở đây là tên mạng, khác với container `edge`): `edge` (ra ngoài), `app` (nội bộ, dịch vụ với `store`), `data` (nội bộ, chỉ `store`, `dbadmin`, `backup`, `postgres`, `valkey`). Postgres và Valkey **không** gắn vào `app` hay `edge`.

## 3b. Cổng vào (`edge`): Traefik
Quyết định của người dùng: dựng một container nginx hoặc Traefik ở phía trước làm điểm mở cổng ra ngoài. **Chọn Traefik**, với lý do dưới đây; nginx là phương án thay thế nếu sau này cần kết thúc STARTTLS ở biên.

### 3b.1 Vì sao Traefik và vì sao không cần hiểu giao thức thư ở biên
- Các dịch vụ giao thức của Rusty **không giữ trạng thái** và phục vụ được mọi người dùng (spec phân tán §2), nên biên **không cần định tuyến theo người dùng** như `zimbra-proxy` (nginx mail module tra mailbox server theo user). Cân bằng tải L4 là đủ.
- **Thư (SMTP, IMAP, POP3) — QUYẾT ĐỊNH ĐÃ SỬA (2026-11-03, theo yêu cầu của người dùng): biên kết thúc TLS cho các cổng TLS ngầm (465 SMTPS, 993 IMAPS, 995 POP3S) bằng chứng chỉ ACME do chính Traefik xin và gia hạn, rồi mã hóa lại bằng mTLS tới dịch vụ phía sau** (mục 3b.5). Điều này thay cho đề xuất ban đầu là chuyển TCP nguyên vẹn. **Đánh đổi phải chấp nhận:** với các cổng này, biên thấy nội dung và mật khẩu dạng rõ trong bộ nhớ; một biên bị chiếm có thể đọc luồng thư và mật khẩu của các kết nối đi qua nó. Giảm thiểu: biên tối giản và cứng hóa (mục 3b.3), không ghi nội dung, không có quyền tới DB/blob, mTLS tới backend, và có công tắc từng cổng `tls_mode = terminate | passthrough` để chuyển lại sang chuyển nguyên vẹn (khi đó dịch vụ tự giữ chứng chỉ) mà không đổi kiến trúc.
- **Cổng STARTTLS** (25, 587, 143, 110) Traefik không kết thúc được vì không hiểu STARTTLS; chúng (nếu bật) luôn là chuyển TCP nguyên vẹn và dịch vụ phía sau phải giữ chứng chỉ riêng. Theo **RFC 8314** ("Cleartext Considered Obsolete") giao thức người dùng cuối nên dùng TLS ngầm, nên **mặc định chỉ mở 465, 993, 995 (và 25 cho thư giữa các máy chủ)**; 587, 143 và 110 mặc định **tắt** (có thể bật bằng cấu hình cho máy khách cũ, kèm cảnh báo).
- **HTTP (JMAP, CalDAV/CardDAV, webmail, admin console): kết thúc TLS ở biên**, áp dụng giới hạn, rồi **mã hóa lại bằng mTLS** tới dịch vụ phía sau (spec container §5).
- Traefik viết bằng Go (an toàn bộ nhớ, giảm lớp lỗi RCE do tràn bộ nhớ so với C); nginx viết bằng C. Đổi lại Traefik có nhiều tính năng cần tắt (mục 3b.3).
- **Lựa chọn thay thế sau này:** biên viết bằng Rust (`rusty-edge`), hiểu giao thức thư và có thể định tuyến theo người dùng, nếu nhu cầu vượt Traefik. Ngoài phạm vi hiện tại.

### 3b.2 Cổng và định tuyến
| Cổng | Chế độ | Đích |
|---|---|---|
| 25 | TCP, chuyển nguyên vẹn | `smtp-in` |
| 465 | TLS ngầm, **kết thúc ở biên (ACME)**, mTLS tới đích | `smtp-submit` |
| 993 | TLS ngầm, **kết thúc ở biên (ACME)**, mTLS tới đích | `imap` |
| 995 | TLS ngầm, **kết thúc ở biên (ACME)**, mTLS tới đích; chỉ nếu POP3 được bật | `pop3` |
| 587 | mặc định **tắt**; nếu bật: TCP chuyển nguyên vẹn (STARTTLS bắt buộc ở dịch vụ) | `smtp-submit` |
| 143, 110 | mặc định **tắt**; nếu bật: TCP chuyển nguyên vẹn (STARTTLS bắt buộc, cấm đăng nhập trước TLS) | `imap`, `pop3` |
| 443 | HTTPS, kết thúc TLS, định tuyến theo `Host` và đường dẫn | `jmap`, `dav`, `webmail`, `admin-console` (host riêng) |
| 80 | chỉ chuyển hướng sang HTTPS và thử thách ACME HTTP-01 | `edge` |
- **PROXY protocol v2** gửi tới các đích TCP để dịch vụ thấy IP thật của máy khách (giới hạn tốc độ, nhật ký, chống dò mật khẩu). Dịch vụ chỉ chấp nhận PROXY protocol từ địa chỉ của `edge` và từ chối kết nối thiếu/giả; HTTP dùng `X-Forwarded-*` chỉ từ `edge`, không tin giá trị do máy khách tự gửi.
- Admin console nằm ở host riêng, giới hạn theo danh sách IP cho phép và bật 2FA khi có; không dùng cùng host với webmail.
- Thư mục hóa theo tenant (host riêng của từng domain, chứng chỉ riêng): định tuyến theo `Host` và SNI; việc cấp chứng chỉ theo domain (ACME DNS-01 hoặc TLS-ALPN-01) được định nghĩa ở bước sau.

### 3b.3 Cứng hóa Traefik (bắt buộc)
- Cấu hình **tĩnh từ tệp** (file provider) gắn chỉ-đọc; **tắt Docker provider** và mọi provider cần socket runtime (vi phạm quy tắc không mount `docker.sock`); không bật `--api.insecure`; dashboard/API tắt, hoặc chỉ nghe trên mạng quản trị nội bộ sau xác thực.
- Không có plugin hay mã nạp lúc chạy; image ghim digest, `scratch`/distroless, chạy user không phải root, `read_only: true`, `cap_drop: [ALL]` (+ `NET_BIND_SERVICE` nếu bind cổng <1024), `no-new-privileges`, seccomp mặc định hoặc riêng, giới hạn tài nguyên.
- TLS: tối thiểu TLS 1.2 (ưu tiên 1.3), bộ mã hóa hiện đại, HSTS cho HTTP, không hỗ trợ giao thức cũ; mTLS tới backend với CA nội bộ; chứng chỉ của biên lấy từ bí mật dạng tệp (hoặc ACME với lưu trữ trên volume riêng, quyền `0600`).
- Chống lạm dụng ở biên: giới hạn số kết nối đồng thời mỗi IP và tổng (TCP in-flight), thời gian chờ đọc/ghi/nhàn rỗi ngắn (chống slowloris), giới hạn kích thước header và thân yêu cầu HTTP (khớp trần tải lên của JMAP), giới hạn tốc độ HTTP theo IP; cập nhật Go/Traefik nhanh khi có advisory (rapid-reset HTTP/2 và loại tương tự).
- Chống dò mật khẩu **không** làm ở biên (biên không thấy xác thực thư): là bộ đếm thất bại theo IP/tài khoản trong dịch vụ giao thức và `store`, dựa trên IP thật từ PROXY protocol, với khóa tạm và độ trễ lũy tiến.
- Nhật ký truy cập JSON, không ghi chuỗi truy vấn, `Authorization`, cookie hay token; ghi IP, cổng, kết quả, thời gian.
- Duy nhất `edge` công bố cổng ra host; kiểm tra tự động (mục 10) bảo đảm không container nào khác làm vậy.

### 3b.5 Chứng chỉ TLS tự động (ACME) cho HTTPS, IMAPS, POP3S, SMTPS
Quyết định của người dùng: Traefik tích hợp xin và gia hạn chứng chỉ SSL/TLS để dùng cho HTTPS và các giao thức thư TLS ngầm (IMAPS, POP3S, SMTPS).

- **Một bộ cấp chứng chỉ duy nhất:** `certificatesResolvers` ACME của Traefik, dùng chung cho router HTTP và router TCP (`HostSNI`) của cổng 465/993/995. Mỗi tên máy (ví dụ `mail.example.com`, `imap.example.com`) có một chứng chỉ; tên máy được khai báo **tường minh** trong cấu hình (danh sách cho phép), **không bao giờ cấp theo SNI tùy ý do máy khách gửi** (chống dùng cạn hạn mức của CA và tấn công làm tràn việc cấp).
- **Loại thử thách (challenge):**
  - `HTTP-01` qua cổng 80 (mặc định, đơn giản, không cần bí mật DNS).
  - `TLS-ALPN-01` qua cổng 443 (không cần cổng 80).
  - `DNS-01` khi cần chứng chỉ wildcard hoặc biên không với tới từ Internet. **Thông tin xác thực API DNS là bí mật giá trị cao:** token giới hạn phạm vi theo vùng (chỉ ghi bản ghi TXT thử thách), nằm trong tệp bí mật `0400` chỉ `edge` đọc được, không đưa vào biến môi trường hay image.
- **CA và hạn mức:** CA mặc định cấu hình được (`caServer`); mặc định production dùng CA công cộng, còn dev/test **bắt buộc** dùng môi trường staging hoặc máy chủ ACME thử nghiệm để không dùng cạn hạn mức thật. Khuyến nghị bản ghi DNS `CAA` chỉ cho CA đã chọn.
- **Lưu trữ khóa:** tệp lưu chứng chỉ ACME (`acme.json` hoặc tương đương) chứa khóa riêng và khóa tài khoản ACME; nằm trên volume riêng chỉ `edge` gắn, quyền `0600`, thuộc user của `edge`; sao lưu phải mã hóa; không bao giờ nằm trong image hay kho mã. Mất tệp này chỉ buộc cấp lại chứng chỉ (không mất dữ liệu thư), nhưng khóa bị lộ phải được thu hồi và cấp lại.
- **Gia hạn:** tự động trước hạn (Traefik mặc định gia hạn khi còn khoảng 30 ngày; kiểm tra đúng ngưỡng ở phiên bản ghim); cảnh báo khi chứng chỉ còn dưới 14 ngày mà chưa gia hạn được (`rustyctl check` đọc hạn chứng chỉ qua một kết nối TLS thử tới chính cổng công khai, không đọc tệp khóa).
- **Một bản `edge`:** Traefik bản mã nguồn mở không chia sẻ trạng thái ACME an toàn giữa nhiều bản sao. Khi cần nhiều bản `edge` (HA), phải chuyển việc cấp chứng chỉ ra ngoài (một dịch vụ cấp chứng chỉ riêng hoặc `cert-manager` trên Kubernetes) và cho `edge` chỉ nạp chứng chỉ từ bí mật chỉ đọc. Ghi nhận như hạn chế đã biết của lựa chọn hiện tại.
- **Chứng chỉ cho cổng STARTTLS (nếu bật) và cổng 25:** dịch vụ phía sau cần chứng chỉ riêng. Hai cách, chọn khi bật: (a) xuất chứng chỉ từ kho ACME của `edge` bằng một bộ xuất nhỏ, chỉ-đọc, chạy tách biệt và ghi PEM ra volume chỉ-đọc cho dịch vụ (thêm một thành phần phải cứng hóa); hoặc (b) dịch vụ tự xin chứng chỉ bằng thư viện ACME của chính nó. Cổng 25 giữa các máy chủ thư dùng TLS cơ hội nên có thể chấp nhận chứng chỉ tự ký nội bộ trong giai đoạn đầu; quyết định DANE/MTA-STS để sau.
- **Chính sách TLS cho tất cả cổng kết thúc ở biên:** tối thiểu TLS 1.2 (ưu tiên 1.3), chỉ bộ mã hóa hiện đại, không SSLv3/TLS 1.0/1.1; ALPN `imap`/`pop3`/`smtp` không bắt buộc; HSTS cho HTTP; cùng một `tls.options` dùng cho cả HTTP và TCP.
- **Mã hóa lại tới backend (mTLS):** biên giữ chứng chỉ client ngắn hạn do CA nội bộ cấp và xác minh chứng chỉ máy chủ của backend (cấu hình `serversTransport` có kiểm tra tên); **phải xác minh bằng tài liệu và test của đúng phiên bản Traefik đã ghim rằng router TCP hỗ trợ TLS tới backend cùng với PROXY protocol v2** (thứ tự: tiêu đề PROXY gửi trước, rồi bắt tay TLS). Nếu phiên bản đó không hỗ trợ, không được hạ xuống plaintext âm thầm: dừng lại, ghi rõ trong báo cáo, và chọn một trong hai cách: dùng đường plaintext trên mạng `app` biệt lập kèm chấp nhận rủi ro bằng văn bản, hoặc chuyển cổng đó sang `passthrough`.
- **Kiểm thử bằng máy chủ ACME thử nghiệm:** môi trường test dùng **Pebble** (máy chủ ACME thử nghiệm chính thức của Let's Encrypt, image ghim digest) và `challtestsrv`; không gọi CA thật trong test (mục 3b.6).

### 3b.6 Kiểm thử chứng chỉ
- `edge` xin được chứng chỉ từ Pebble cho từng tên máy khai báo (HTTP-01 và TLS-ALPN-01; DNS-01 qua `challtestsrv`), và trình bày đúng chứng chỉ đó trên 443, 993, 995, 465 (kiểm tra bằng máy khách TLS thật: tên, chuỗi, ngày hết hạn, phiên bản TLS tối thiểu).
- Tên máy lạ (SNI không có trong danh sách) **không** kích hoạt việc cấp chứng chỉ và bị từ chối bắt tay hoặc nhận chứng chỉ mặc định không khớp.
- Gia hạn: rút ngắn hạn chứng chỉ ở Pebble, chứng minh `edge` gia hạn tự động và máy khách nhận chứng chỉ mới mà không đứt kết nối đang có.
- CA ngừng hoạt động: `edge` tiếp tục phục vụ chứng chỉ hiện có, không crash, ghi cảnh báo; khi CA trở lại thì gia hạn thành công.
- Mất tệp lưu trữ ACME: `edge` cấp lại, không phục vụ chứng chỉ hỏng.
- Bí mật: `acme.json` quyền `0600`, thuộc đúng user, không nằm trong image; thông tin DNS-01 chỉ ở tệp bí mật `0400`; nhật ký không chứa khóa hay token.
- Cổng 587/143/110 mặc định đóng (kiểm tra bằng kết nối thật); bật bằng cấu hình thì chuyển nguyên vẹn.
- Cấm giao thức cũ: máy khách TLS 1.0/1.1/SSLv3 bị từ chối trên mọi cổng kết thúc ở biên.

### 3b.4 Kiểm thử cổng vào
- Khung `deploy/compose/` dựng `edge` cùng backend giả (Rust, tĩnh) cho TCP và HTTP; kiểm tra: chuyển nguyên vẹn các luồng nhị phân (không đổi byte), STARTTLS đi qua nguyên vẹn, PROXY protocol v2 đến đích với IP đúng và bị từ chối khi giả mạo, định tuyến theo `Host`/đường dẫn, kết thúc và mã hóa lại TLS, giới hạn kích thước/thời gian/số kết nối bị áp dụng, dashboard và API không truy cập được từ ngoài, không có Docker provider, rootfs chỉ đọc, không `docker.sock`.
- Tập lệnh kiểm tra cấu hình (`rustyctl check`/`scripts/check-compose.sh`) từ chối mọi compose mà dịch vụ ngoài `edge` công bố cổng.

## 4. Cứng hóa mỗi container (nền tảng bắt buộc)
- Chạy bằng user không phải root, **uid/gid cố định theo dịch vụ** (bảng uid trong repo, ví dụ `store`=10001, `imap`=10002...), không có shell.
- `read_only: true` cho rootfs; ghi chỉ vào volume khai báo hoặc `tmpfs` (kích thước giới hạn, `noexec,nosuid,nodev`).
- `cap_drop: [ALL]` (chỉ thêm lại `NET_BIND_SERVICE` cho `proxy` nếu cần cổng <1024; ưu tiên cổng cao và ánh xạ ở host), `security_opt: [no-new-privileges:true]`.
- Hồ sơ seccomp riêng cho từng dịch vụ (dựng từ chế độ `audit` rồi siết), không dùng `unconfined`; AppArmor/SELinux nếu host có. Landlock trong tiến trình khi kernel hỗ trợ (spec mô hình tiến trình §2).
- Giới hạn tài nguyên: `mem_limit`, `cpus`, `pids_limit`, `ulimits` (nofile), để một dịch vụ không làm cạn host.
- **Không** `privileged`, không `network_mode: host`, không mount `hostPath` nhạy cảm, không `/var/run/docker.sock`.
- Healthcheck mỗi dịch vụ (lệnh con `health` của binary, không dùng curl/shell), `restart: unless-stopped`, thứ tự phụ thuộc bằng `depends_on` có điều kiện `service_healthy`.
- Bí mật: **tệp** gắn từ Docker secrets/tmpfs (quyền `0400`, thuộc uid dịch vụ), không bao giờ biến môi trường, không bake vào image. Có quy trình xoay vòng.
- Log ra stdout/stderr dạng JSON một dòng, không chứa bí mật/nội dung thư; driver log có giới hạn kích thước.
- Rootless: mục tiêu là chạy toàn bộ bằng Docker rootless hoặc Podman rootless; ghi rõ khác biệt (cổng thấp, cgroup, uid ánh xạ) và test trên cả hai.

## 5. Kênh nội bộ: hai chế độ truyền tải
Unix socket qua volume chỉ chạy được trên **một host**; lên Kubernetes thì các pod nằm trên nhiều node nên không dùng chung socket được. Vì vậy API nội bộ của `store` (spec mô hình tiến trình §7b) phải có hai transport với **cùng giao thức và cùng ma trận vai trò**:

1. **Unix socket + `SO_PEERCRED`** (Compose một host): nhận diện vai trò bằng uid cố định theo bảng ở mục 4. Volume socket gắn chỉ-đọc cho client, ghi cho `store`.
2. **mTLS qua TCP** (mặc định cho k8s và khi tách host): vai trò lấy từ định danh trong chứng chỉ client (SAN/SPIFFE ID), chứng chỉ ngắn hạn do CA nội bộ cấp, `store` xác minh chuỗi và vai trò; không có xác thực = ngắt.

Mã chọn transport bằng cấu hình; kiểm thử ma trận vai trò chạy trên cả hai.

## 6. `rustyctl` trên host
- Là CLI chạy ngoài container, trên tài khoản quản trị host. Nhóm lệnh hạ tầng gọi runtime container: `status`, `start|stop|restart [dịch-vụ|all]`, `logs`, `ps`, `pull/verify` (xác minh chữ ký và digest trước khi triển khai), `up/down` theo thứ tự phụ thuộc, `check`.
- Nhóm lệnh ứng dụng (`config ...`, `backup`, `db ...`) nói chuyện với `store`/`dbadmin` qua API quản trị (UDS hoặc mTLS), cùng vai trò `superadmin`/`domain-admin` và kiểm toán như đã thiết kế.
- Tài khoản dùng `rustyctl` hạ tầng cần quyền điều khiển runtime (nhóm `docker` = root tương đương trên host): hạn chế bằng cách dùng Podman/Docker rootless cho tài khoản `rusty-ops` chuyên dụng, không chia sẻ. Ghi rõ trong tài liệu vận hành.
- `rustyctl check` kiểm tra: phiên bản kernel và tính năng (cgroup v2, seccomp, user namespace, Landlock), runtime ở chế độ rootless hay không, mọi image đúng digest đã duyệt, không container nào mount socket runtime/`privileged`/`host` network, quyền tệp bí mật, cổng chỉ công bố ở `proxy`, hồ sơ seccomp đã nạp, thời gian chứng chỉ còn lại.
- `rustyd` (supervisor tiến trình trong spec mô hình tiến trình) chỉ còn dùng cho **chế độ dev/không container**; trong triển khai container, runtime làm việc giám sát.

## 7. Dữ liệu và lưu trữ

### 7.1 Cái gì cần lưu trữ gì
| Dữ liệu | Đặc điểm | Nơi lưu (Compose một host) | Khi lên k8s |
|---|---|---|---|
| Postgres (`pgdata`) | cần khối lưu trữ nhanh, `fsync` đúng nghĩa, một người ghi | volume cục bộ (ext4/xfs) | PVC `ReadWriteOnce` trên lưu trữ khối (không NFS) hoặc operator (ví dụ CloudNativePG) |
| Blob thư (`blobs`) | tệp bất biến đặt tên theo hash; nhiều và lớn | volume cục bộ | **kho đối tượng tương thích S3** (xem 7.3) |
| `system.db` (SQLite) | một người ghi, dùng khóa tệp | volume cục bộ | PVC `ReadWriteOnce`, **không bao giờ để trên NFS** (khóa tệp SQLite trên NFS không an toàn) |
| Valkey | cache, không persistence | không có | không cần PV |
| Sao lưu | nhiều, ít đọc | volume hoặc kho ngoài | kho đối tượng |

### 7.2 Có nên dựng image NFS hoặc iSCSI tự làm để scale lên k8s?
**Không.** Lý do:
- **iSCSI** là lưu trữ *khối*, mỗi volume chỉ gắn được cho một node cùng lúc (RWO), nên không giải quyết bài toán nhiều `store` dùng chung blob. Phía server (iSCSI target) cần module kernel và quyền cao; không có cách an toàn để chạy trong container thường.
- **NFS tự dựng** làm máy chủ NFS trong container cần `privileged` hoặc cấp thêm capability và module kernel `nfsd`; nó trở thành **điểm lỗi đơn** và mâu thuẫn với "cứng hóa" và "không phụ thuộc distro/kernel". Ngoài ra bản thân thiết kế blob của Rusty (đổi tên nguyên tử, hard link khi khôi phục, mtime làm cửa sổ ân hạn, khóa `flock` cho GC — Task 3 và Task 7) dựa vào ngữ nghĩa tệp cục bộ; trên NFS các cơ chế này (cache thuộc tính, khóa tư vấn, rename) hoạt động khác và đã được xác định là rủi ro mất dữ liệu nếu sai.
- Trên k8s người ta không tự dựng máy chủ lưu trữ: dùng **CSI driver** của hạ tầng (Ceph/Rook, Longhorn, lưu trữ của nhà cung cấp cloud) hoặc dịch vụ đối tượng.

### 7.3 Hướng scale đúng cho blob
Blob của Rusty là **đối tượng bất biến đặt tên theo hash**, phù hợp tự nhiên với kho đối tượng tương thích S3. Lộ trình:
1. **Hiện tại:** `FsBlobStore` trên volume cục bộ, đủ cho Compose một host.
2. **Bước sau:** thêm backend `S3BlobStore` sau trait `BlobStore`. Việc cần làm trước:
   - Chuyển cơ chế bảo vệ chống xóa nhầm (hiện dựa vào mtime tệp, đổi tên cách ly, `flock`) sang **cơ sở dữ liệu**: cột `blobs.last_put_at` do `put` cập nhật trong DB và `zero_since` đã có; GC xóa đối tượng chỉ khi cả hai quá hạn ân hạn, xóa hàng trước rồi mới xóa đối tượng; khóa GC bằng khóa tư vấn trong Postgres (`pg_try_advisory_lock`) thay vì tệp.
   - `put` dùng ghi có điều kiện (không ghi đè nếu đã có); `get` đọc theo khóa hash; liệt kê theo tiền tố hash.
   - Khóa đối tượng có tiền tố tenant: `t/{domain_id}/aa/bb/{hash}` hoặc kho riêng cho từng tenant; mã hóa phía máy khách bằng khóa theo tenant (spec Postgres–Valkey §3c.4) để lộ kho đối tượng không lộ nội dung thư.
   - Phiên bản kho đối tượng cụ thể (Ceph RGW, SeaweedFS, Garage, hoặc cloud) chọn sau, kiểm tra giấy phép và mức bảo trì thời điểm đó.
3. **Giải pháp trung gian nếu cần chia sẻ tệp sớm:** RWX PVC từ CSI (CephFS hoặc NFS-CSI do hạ tầng quản lý), kèm kiểm thử đồng thời bắt buộc cho toàn bộ giao thức put/GC trên chính hệ thống tệp đó, và chỉ cho một phiên bản GC chạy tại một thời điểm. Không khuyến nghị làm đường chính.
4. **Mô hình khác (như Zimbra):** gắn mỗi mailbox vào một node `store` cố định (định tuyến theo mailbox), mỗi node có volume cục bộ; không cần kho chung nhưng khó di chuyển mailbox khi node hỏng. Ghi nhận như phương án dự phòng.

Quyết định hiện tại: **không dựng image lưu trữ chia sẻ**; giữ ranh giới `BlobStore` và dọn đường cho `S3BlobStore` (việc đầu tiên: chuyển bảo vệ GC sang DB).

## 8. Chuỗi cung ứng và quy trình vá zero-day
- Toàn bộ phiên bản nền (builder, Postgres, Valkey, distroless) ghim digest trong tệp `deploy/images.lock` (do CI cập nhật qua PR có người duyệt).
- Mỗi image Rusty: build lặp lại được, SBOM, chữ ký cosign, quét lỗ hổng; `rustyctl pull` từ chối image không có chữ ký hợp lệ hoặc digest ngoài `images.lock`.
- **Quy trình vá khẩn:** cảnh báo bảo mật -> cập nhật `images.lock` (hoặc nâng phụ thuộc Rust) -> CI build, quét, ký -> `rustyctl pull && rustyctl restart` theo thứ tự; mục tiêu thời gian được ghi trong runbook. Có lệnh liệt kê container đang chạy digest cũ.
- Kho đăng ký (registry) nội bộ hoặc bản sao lưu image để triển khai lại không phụ thuộc Internet ở thời điểm sự cố.

## 9. Đường lên Kubernetes (chuẩn bị, chưa làm)
| Compose | Kubernetes |
|---|---|
| service | `Deployment` (không trạng thái) / `StatefulSet` (nếu giữ FS cục bộ) |
| mạng `edge/app/data` | `NetworkPolicy` mặc định từ chối, chỉ mở đường cần thiết |
| hardening mục 4 | `securityContext` + Pod Security `restricted`, `readOnlyRootFilesystem`, seccomp `RuntimeDefault` hoặc riêng |
| Docker secrets | `Secret` (mã hóa etcd) hoặc trình quản lý bí mật ngoài |
| UDS | mTLS (mục 5) |
| `depends_on` | readiness/startup probe + init |
| Postgres | operator (ví dụ CloudNativePG) hoặc dịch vụ managed |
| blob | kho đối tượng (7.3) |
Không dùng `hostPath`, không cho ServiceAccount quyền thừa; `rustyctl` hạ tầng khi đó thành lớp bọc `kubectl`/API với RBAC tối thiểu.

## 10. Kiểm thử và tiêu chí
- Compose mẫu trong `deploy/compose/` dựng toàn bộ stack; `rustyctl check` xanh.
- Test tự động: không container nào chạy root, không `privileged`, không mount socket runtime, rootfs chỉ đọc, `cap_drop ALL`, không cổng nào công bố ngoài `proxy`, Postgres/Valkey không với tới được từ `app`/`edge` (kiểm tra bằng kết nối thật từ container `imap`).
- Test chaos: `valkey` dừng -> yêu cầu vẫn thành công; `postgres` dừng -> dịch vụ trả lỗi tạm thời, tự phục hồi khi DB lên lại; `store` restart -> client nối lại.
- Kiểm thử transport UDS và mTLS cùng ma trận vai trò (spec mô hình tiến trình §7b), gồm chứng chỉ sai vai trò/hết hạn.
- Benchmark chạy trong chính môi trường Compose này (image Postgres 18 và Valkey ghim digest).

## 11. Rủi ro và điểm mở
- Quyền điều khiển runtime = quyền root host: dùng rootless và tài khoản chuyên dụng; ghi rõ trong tài liệu vận hành.
- Image Postgres/Valkey chính thức mang userland của distro riêng (Debian): "không phụ thuộc distro" ở mức host; phần userland trong image là một phần chuỗi cung ứng cần quét và vá.
- Kernel vẫn là phụ thuộc chung của mọi container: tính năng Landlock/seccomp/userns phụ thuộc kernel; `rustyctl check` phải báo rõ và từ chối chạy production khi thiếu (mặc định), có cờ chỉ dành cho dev.
- Binary theo dịch vụ (cargo feature) tăng độ phức tạp của build/CI; bù lại giảm bề mặt tấn công từng container. Cần quyết định có thật sự tách binary ngay từ đầu hay bắt đầu bằng một binary rồi tách dần (đề xuất: tách ngay cho `store`, `imap`, `smtp-in`, vì chúng khác nhau nhiều nhất).
- Docker rootless có hạn chế (cổng thấp, hiệu năng mạng/ghi, ánh xạ uid): cần đo và ghi lại.
- Thiết kế hai transport (UDS, mTLS) thêm công việc và bề mặt cần fuzz/kiểm thử.
