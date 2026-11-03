# Rusty - Mô hình tiến trình: multi daemon + `rustyd` + `rustyctl`

Ngày: 2026-11-03. Trạng thái: chờ người dùng duyệt spec (sau đó mới lập kế hoạch triển khai chi tiết). Quyết định của người dùng (2026-11-03): **tách `store` thành dịch vụ riêng ngay từ đầu** (mục 7b).

## 0. Điều chỉnh (2026-11-03, container-per-service)
Người dùng quyết định triển khai bằng Docker/OCI, **mỗi dịch vụ một container, `rustyctl` chạy trên host** (spec `2026-11-03-rusty-container-deployment-design.md`). Điều chỉnh đối với tài liệu này:

- **Trong triển khai container:** runtime (Docker Compose, sau này Kubernetes) làm việc giám sát, khởi động lại và thứ tự phụ thuộc. `rustyd` (mục 4) **chỉ còn dùng cho chế độ dev/không container**. Máy trạng thái, circuit breaker và drain mô tả ở mục 4 vẫn là yêu cầu cho chế độ đó, và nguyên tắc (restart có backoff, dừng crash-loop, drain) được ánh xạ sang cấu hình runtime (restart policy, healthcheck, giới hạn số lần khởi động lại).
- **`rustyctl` (mục 6):** là CLI trên host; nhóm lệnh hạ tầng gọi runtime container, nhóm lệnh ứng dụng (`config`, `backup`, `db`) gọi API quản trị qua `store`/`dbadmin`. **Không container nào được mount socket của runtime.**
- **Kênh điều khiển (mục 5) và API `store` (mục 7b):** có hai transport cùng giao thức và cùng ma trận vai trò: Unix socket + `SO_PEERCRED` (một host, uid cố định theo dịch vụ) và **mTLS qua TCP** (mặc định cho Kubernetes, vai trò lấy từ định danh trong chứng chỉ).
- **Binary:** triển khai container dùng binary riêng cho từng dịch vụ (cargo feature) để mỗi container chỉ chứa mã của mình; binary multi-call `rusty` còn dùng cho dev/không container.
- **Sandbox (mục 2, `rusty-sandbox`):** vẫn áp dụng bên trong container (Landlock/seccomp tiến trình) như lớp bổ sung cho cứng hóa của runtime; các container chạy user không phải root với uid cố định.
- Các mục 7, 7b, 8-11 của tài liệu này (bảng dịch vụ, vai trò, capability, mô hình đe dọa) vẫn đúng; "tiến trình" trong đó nay hiểu là "container" khi triển khai bằng container.

## 1. Bối cảnh và mục tiêu
Zimbra vận hành bằng nhiều daemon, điều khiển bởi `zmcontrol` (start/stop/status theo dịch vụ và thứ tự phụ thuộc). Rusty giữ ý tưởng đó nhưng làm lại theo định hướng **bảo mật trước, chịu zero-day** (spec nền tảng §8b):

- Mỗi giao thức/chức năng chạy **một tiến trình riêng**, user riêng, quyền tối thiểu, sandbox riêng. Lỗi trong parser IMAP không chạm được khóa DKIM hay DB.
- Một supervisor nhỏ `rustyd` quản lý vòng đời; một CLI `rustyctl` điều khiển (tương đương `zmcontrol`).
- Một mã nguồn, một **multi-call binary** `rusty` (`rusty gc`, `rusty smtp`, `rusty imap`...), để vá và phát hành đơn giản, không cần script ghép nhiều ngôn ngữ.

Ngoài phạm vi: nội dung nghiệp vụ của SMTP/IMAP/JMAP (các dự án sau); quản trị dữ liệu kiểu `zmprov` (sẽ là `rustyadm`, dự án admin); cluster nhiều máy.

Nền tảng chỉ Linux (dev chạy trong WSL).

## 2. Quyết định chính
| Chủ đề | Quyết định | Lý do |
|---|---|---|
| Tiến trình | Một tiến trình mỗi dịch vụ, một binary multi-call `rusty <dịch-vụ>` | Cô lập quyền; một artifact để ký/kiểm chứng/vá |
| Giám sát | `rustyd` tự giám sát, được systemd khởi động (một unit duy nhất) | Chạy cả trong container; không phụ thuộc systemd để có thứ tự và health; vẫn tích hợp được journald |
| Điều khiển | `rustyctl` nói chuyện với `rustyd` qua **Unix socket**, không có cổng TCP | Không lộ ra mạng; xác thực bằng uid/gid của peer |
| Định nghĩa dịch vụ | **Bảng dịch vụ biên dịch sẵn** (trong crate `rusty-svc`), tệp cấu hình chỉ bật/tắt và chỉnh tham số có giới hạn | Kẻ ghi được `/etc` không thể bắt `rustyd` (chạy root) thực thi lệnh tùy ý |
| Truy cập dữ liệu | **Chỉ dịch vụ `store` được mở DB và thư mục blob**; mọi dịch vụ khác gọi `store` qua Unix socket với quyền theo vai trò (mục 7b) | Một parser bị khai thác không đọc được DB; kiểm soát truy cập tập trung một nơi |
| Cấp quyền | `rustyd` chạy root chỉ để bind cổng đặc quyền, tạo thư mục, hạ quyền con; mọi dịch vụ hạ quyền ngay khi khởi động | Giảm thiểu mã chạy với quyền cao |
| Sandbox | Landlock (hệ thống tệp) + seccomp (syscall) theo hồ sơ từng dịch vụ; `NoNewPrivileges` | Chặn leo thang khi một parser bị khai thác |
| `unsafe` | Mã của dự án vẫn `#![forbid(unsafe_code)]`; syscall đi qua các crate đã được kiểm chứng (`rustix`, `landlock`, `seccompiler`) | Giữ cam kết §8b, và ghi rõ phần `unsafe` nằm trong phụ thuộc |

## 3. Thành phần
```
crates/
  rusty-ipc       giao thức ctl: kiểu Request/Response, mã hóa, giới hạn kích thước (không phụ thuộc DB)
  rusty-svc       bảng dịch vụ: tên, lệnh con, user, phụ thuộc, hồ sơ sandbox, chính sách restart, health
  rusty-sandbox   hạ quyền (setuid/setgid/groups), landlock, seccomp, rlimit, NoNewPrivileges
  rusty-storeapi  giao thức store: kiểu yêu cầu/đáp, vai trò, capability, truyền luồng; có `RemoteMailbox`/`RemoteDirectory` (client) cho các dịch vụ khác
apps/
  rusty           binary multi-call: `rusty <dịch-vụ> [--config ...]` (gồm `rusty store`, nơi duy nhất dùng `rusty-store` + `rusty-directory`)
  rustyd          supervisor
  rustyctl        CLI điều khiển
```
`rustyd` và `rustyctl` không phụ thuộc `rusty-store` (không chạm DB, không chạm thư): giảm bề mặt tấn công của thành phần có quyền cao.

## 4. `rustyd`: supervisor
**Máy trạng thái mỗi dịch vụ:** `Stopped -> Starting -> Running -> Stopping -> Stopped`, thêm `Failed` (circuit breaker) và `Disabled`.

- **Thứ tự phụ thuộc:** đồ thị có hướng từ bảng dịch vụ, sắp xếp topo; phát hiện chu trình khi tải (lỗi cứng). Khởi động theo thứ tự phụ thuộc, dừng theo thứ tự ngược. Dịch vụ phụ thuộc chỉ bắt đầu khi dịch vụ nó cần đã `Running` (đã báo ready).
- **Ready/health:** mỗi con báo `READY` qua một pipe kế thừa (giống `sd_notify` nhưng riêng) và gửi heartbeat theo chu kỳ. Quá hạn heartbeat thì coi là treo: SIGTERM, hết hạn chờ thì SIGKILL, rồi restart theo chính sách.
- **Restart:** chính sách mỗi dịch vụ (`always | on-failure | never`), backoff mũ có jitter. **Circuit breaker:** quá N lần crash trong M phút thì đánh dấu `Failed`, ngừng thử, ghi sự kiện bảo mật (có thể là dấu hiệu bị khai thác), không tự động tiếp tục.
- **Tắt có kiểm soát:** `Stopping` gửi SIGTERM, dịch vụ ngừng nhận kết nối mới và hoàn tất việc đang chạy (drain) trong hạn chót, sau đó SIGKILL.
- **Reload:** `reload` gửi SIGHUP; mỗi dịch vụ tự tải lại cấu hình, lỗi cấu hình thì giữ cấu hình cũ và báo `reload failed`.
- **Log:** stdout/stderr của con được `rustyd` thu qua pipe, đóng gói JSON một dòng (dịch vụ, mức, thời gian), ghi ra journald hoặc tệp xoay vòng. Giới hạn độ dài dòng; không bao giờ ghi mật khẩu/hash/nội dung thư.
- **Thế hệ:** mỗi lần restart tăng `generation`; nhờ đó `rustyctl status` thấy dịch vụ đã khởi động lại bao nhiêu lần.
- **Bảng tài nguyên:** rlimit (số fd, bộ nhớ, tiến trình con) theo hồ sơ; cgroup là mục mở rộng sau.

## 5. Giao thức điều khiển (`rusty-ipc`)
- Unix socket `/run/rusty/ctl.sock`, quyền `0660`, thuộc `root:rusty-admin`; **xác thực bằng `SO_PEERCRED`**: chỉ root hoặc thành viên nhóm `rusty-admin` được gọi lệnh ghi (start/stop/restart/reload); lệnh chỉ đọc (status/logs) cho nhóm `rusty-ops` nếu có.
- Mã hóa: khung dài 4 byte + JSON (serde), có `version`, giới hạn 64 KiB mỗi khung, giới hạn số kết nối đồng thời, hạn chót đọc/ghi. Từ chối trường lạ (`deny_unknown_fields`).
- Lệnh: `Status { services }`, `Start/Stop/Restart/Reload { services }`, `Logs { service, follow, lines }`, `Check`, `Version`.
- Mọi lệnh ghi được ghi nhật ký kiểm toán: ai (uid, pid), làm gì, kết quả.

## 6. `rustyctl`: CLI (tương đương `zmcontrol`)
```
rustyctl status [dịch-vụ... | all] [--json]
rustyctl start|stop|restart|reload [dịch-vụ... | all]
rustyctl logs <dịch-vụ> [-f] [-n 100]
rustyctl check              # kiểm tra cấu hình, quyền tệp, hỗ trợ sandbox của kernel, phiên bản các daemon
rustyctl version
rustyctl config list|get|set|history|export|import|reload   # cài đặt hệ thống (system DB); xem spec Postgres+Valkey §3b
rustyctl backup                                             # sao lưu thống nhất Postgres + system DB
```
- `status` in bảng dịch vụ: tên, trạng thái, pid, thời gian chạy, số lần restart, ready; mã thoát khác 0 nếu có dịch vụ bắt buộc không `Running` (dùng được trong giám sát).
- `restart all` thực hiện theo thứ tự phụ thuộc; `restart <dịch-vụ>` kèm các dịch vụ phụ thuộc vào nó nếu cần (tùy chọn `--with-dependents`).
- Kết quả có màu khi là TTY, ổn định và parse được khi `--json`.
- Không bao giờ in bí mật; thông báo lỗi không lộ đường dẫn nhạy cảm.

## 7. Bảng dịch vụ ban đầu
Chỉ khai báo những gì đang có; còn lại là chỗ giữ chỗ (`planned`, không khởi động được):

| Dịch vụ | Trạng thái | Phụ thuộc | Ghi chú |
|---|---|---|---|
| `gc` | có (Task 7) | — | dọn thư đã xóa, blob mồ côi, tệp tạm |
| `store` | có (giai đoạn 8b, trước mọi dịch vụ giao thức) | — | người truy cập DB và blob duy nhất; chạy `Mailbox`/`Directory` đã có |
| `gc` | có | `store` | gọi các thao tác dọn qua API vai trò `gc` (không mở DB trực tiếp) |
| `lmtp`, `smtp-in`, `smtp-submit`, `imap`, `jmap`, `dav`, `proxy`, `filter`, `admin-api` | planned | `store` | thêm khi làm các dự án 2 trở đi; chỉ nói chuyện với `store` |

Mỗi dịch vụ khai báo: user riêng (`rusty-gc`, `rusty-imap`...), thư mục dữ liệu được phép (Landlock), danh sách syscall (seccomp), cổng, rlimit, restart, heartbeat.

## 7b. Dịch vụ `store` (tách riêng)
`rusty store` là tiến trình duy nhất có quyền mở file DB (SQLite (journal mặc định hiện tại; WAL sau benchmark), sau này kết nối Postgres) và thư mục blob. Nó chạy lại nguyên thư viện đã có (`Mailbox`, `Directory`, GC) phía sau một API hẹp.

**Kênh:** Unix socket `/run/rusty/store.sock`, quyền `0660` thuộc nhóm `rusty-store-clients`; nhận diện peer bằng `SO_PEERCRED` -> **vai trò** theo uid dịch vụ (bảng cấp quyền biên dịch sẵn, không đọc từ tệp cấu hình). Khung 4 byte + mã hóa nhị phân/JSON có phiên bản, mỗi khung tối đa 1 MiB, thư lớn truyền theo luồng nhiều khung (mỗi khối <= 256 KiB, kèm tổng kích thước đã khai báo và kiểm tra giới hạn 50 MiB), hạn chót đọc/ghi, giới hạn kết nối đồng thời theo vai trò.

**Vai trò và quyền (mỗi vai trò chỉ gọi được các thao tác sau):**

| Vai trò | Được làm | Không được làm |
|---|---|---|
| `lmtp` / `smtp-in` | `Resolve(address)`, `Deliver(address, thư)`: chỉ thêm vào Inbox (hoặc folder do bộ lọc chỉ định) | đọc thư, liệt kê, xóa, xác thực |
| `imap` / `jmap` / `dav` | `Authenticate(address, password) -> Capability`; mọi thao tác mailbox cần capability | truy cập mailbox mà không có capability; liệt kê tài khoản |
| `smtp-submit` | `Authenticate`, `AppendSent(capability, thư)`, `Resolve` | đọc mailbox ngoài việc ghi Sent |
| `filter` | `GetForScan(blob)`, `SetVerdict` trên thư đang giao | duyệt mailbox |
| `gc` | `PurgeDeleted`, `CollectBlobs` | đọc nội dung |
| `admin-api` (superadmin) | thao tác `Directory` trên mọi tenant, domain, plan hệ thống, trần, cài đặt hệ thống | đọc nội dung thư |
| `admin-api` (domain-admin cho domain D) | thao tác `Directory` **chỉ trong tenant D** (capability mang `domain_id`), quota/giới hạn trong trần | thấy hay đoán tenant khác (trả `NotFound`), đọc nội dung thư |

**Capability (chống "một dịch vụ bị chiếm có thể đòi mọi mailbox"):** `Authenticate` chạy bên trong `store` (argon2 như hiện có); kết quả là một capability ngẫu nhiên 256 bit, gắn với (vai trò, `AccountId`, hạn dùng ngắn, kết nối). `store` chỉ giữ băm của nó, hết hạn tự động, thu hồi được (đổi mật khẩu, khóa tài khoản, `Logout`). Dịch vụ giao thức bị khai thác chỉ truy cập được các tài khoản **đang đăng nhập qua nó**, không phải cả cơ sở dữ liệu. `Deliver` không cần capability vì chỉ cho phép thêm vào Inbox của địa chỉ hợp lệ, đã có giới hạn kích thước/tần suất.

**Hợp đồng lỗi:** `store` trả đúng `rusty_core::Error` (đã được làm sạch, không lộ SQL). Mất kết nối tới `store` -> dịch vụ gọi trả lỗi tạm thời (SMTP `451`, IMAP `NO [UNAVAILABLE]`), không bao giờ "cho qua".

**Truy cập từ phía dịch vụ:** các trait `MetadataStore`/`Mailbox` hiện có là ranh giới; `rusty-storeapi` cung cấp `RemoteMailbox`/`RemoteDirectory` cùng giao diện nên mã nghiệp vụ trong các dịch vụ không phải viết lại. Cài đặt trực tiếp vẫn giữ cho test và cho chế độ `all-in-one` ở dev.

**Phiên bản và vá:** `Hello` trao đổi phiên bản giao thức; lệch phiên bản thì từ chối. Vá lỗi logic dữ liệu chỉ cần restart `store` (và các dịch vụ phụ thuộc nối lại).

**Hệ quả hiệu năng:** thêm một bước IPC cho mỗi thao tác; giảm thiểu bằng pipeline nhiều yêu cầu trên một kết nối, truyền luồng, và danh sách có phân trang (hoàn thành minor M1 của Task 6: `list_folder`/`changes_since` nhận `limit` + cursor trong API này).

## 8. Mô hình đe dọa (tóm tắt, chi tiết trong `SECURITY.md` ở Task 9)
- Kẻ khai thác parser của một dịch vụ: bị nhốt bởi user, Landlock, seccomp; không đọc được khóa của dịch vụ khác; không mở được DB; không gọi được `rustyd` (socket chỉ cho root/nhóm admin, con không thuộc nhóm); với `store` chỉ làm được các thao tác của vai trò đó, trên các tài khoản đang đăng nhập qua nó.
- Kẻ khai thác `store`: là thành phần có giá trị cao nhất; thu nhỏ mã (không có parser giao thức, chỉ có `rusty-mime` cho `parse_envelope`), sandbox chặt nhất, không có quyền mạng; fuzz và review bảo mật ưu tiên cao nhất.
- Kẻ ghi được tệp cấu hình: không thể đổi lệnh thực thi (bảng dịch vụ biên dịch sẵn); giá trị cấu hình được xác thực.
- Dịch vụ crash-loop: circuit breaker, cảnh báo, không loop vô hạn.
- Cập nhật/vá: thay binary `rusty` bằng tệp mới rồi `rustyctl restart all`; `rustyd` kiểm tra phiên bản giao thức giữa các thành phần, từ chối chạy khi lệch.

## 9. Kiểm thử
- Đơn vị: sắp xếp topo và phát hiện chu trình; máy trạng thái và backoff với đồng hồ giả; circuit breaker; mã hóa/giải mã ctl với khung sai, quá lớn, trường lạ.
- Tích hợp (không cần root): dịch vụ giả `rusty testsvc` có chế độ `ok`, `crash`, `hang`, `slow-ready`, `ignore-sigterm`; kiểm tra thứ tự start/stop, restart, circuit breaker, drain, reload, từ chối khi thiếu quyền (dùng uid hiện tại với socket do test tạo).
- Bảo mật: fuzz bộ giải mã `rusty-ipc`; test `SO_PEERCRED` từ chối peer không thuộc nhóm; test sandbox (Landlock chặn đọc ngoài danh sách), bỏ qua với thông báo rõ ràng khi kernel không hỗ trợ.
- `rustyctl check` có test kiểm tra từng lỗi cấu hình.

## 10. Phân kỳ triển khai (sẽ thành kế hoạch chi tiết sau khi duyệt)
1. `rusty-ipc`: kiểu, mã hóa, giới hạn, fuzz.
2. `rusty-svc`: bảng dịch vụ, đồ thị phụ thuộc, kiểm tra chu trình.
2b. `rusty-storeapi` (kiểu, vai trò, capability, luồng, fuzz) — làm sau mục 7, trước mục 8.
3. `rustyd` lõi: spawn, máy trạng thái, backoff, circuit breaker, drain, với dịch vụ giả `testsvc`.
4. Ready/heartbeat và log.
5. Máy chủ ctl: Unix socket, `SO_PEERCRED`, kiểm toán.
6. `rustyctl`: `status`, `start|stop|restart|reload`, `logs`, `check`, `--json`, mã thoát.
7. `rusty-sandbox`: hạ quyền, rlimit, `NoNewPrivileges`, Landlock, seccomp.
8. Tích hợp dịch vụ thật đầu tiên: `rusty store` (bao `Mailbox`/`Directory`, hồ sơ sandbox riêng, API theo vai trò, capability), rồi `rusty gc` gọi `store` qua vai trò `gc`.
8b. `RemoteMailbox`/`RemoteDirectory`, phân trang `list_folder`/`changes_since`, test vai trò và thu hồi capability.
9. Gói triển khai: systemd unit cho `rustyd`, tạo user/nhóm, thư mục, tài liệu vận hành.
10. Củng cố: fuzz, bài kiểm tra musl (static) và vòng review bảo mật.

## 11. Rủi ro và điểm mở
- Seccomp theo syscall dễ vỡ khi thay đổi phụ thuộc: cần chế độ `audit` (ghi log thay vì giết) để dựng hồ sơ, và test hồi quy cho mỗi hồ sơ.
- Landlock cần kernel ≥ 5.13 (ABI mới hơn có thêm tính năng); `rustyctl check` phải báo rõ mức hỗ trợ và chính sách khi thiếu (mặc định **từ chối chạy ở chế độ production** nếu không có sandbox, cho phép `--insecure-dev` ở dev).
- `store` thành điểm lỗi đơn: cần restart nhanh, kết nối lại tự động, và (sau này) chạy nhiều bản với Postgres. Đây là cái giá có chủ ý của việc tách.
- Thiết kế API `store` theo từng thao tác nghiệp vụ (không phải SQL thô) để vai trò kiểm soát được; thao tác mới cần cập nhật bảng vai trò, test âm cho mọi vai trò không được phép.
- Truyền thư lớn qua socket tốn bộ nhớ nếu làm vụng: bắt buộc truyền luồng và có giới hạn; cân nhắc truyền file descriptor (SCM_RIGHTS qua `rustix`) cho blob lớn ở bước sau.
- Chuyển nóng (socket handoff) để restart không rớt kết nối: để sau, không nằm trong phạm vi này.
