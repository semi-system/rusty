# Rusty - Mô hình hệ thống phân tán (định nghĩa trước)

Ngày: 2026-11-03. Trạng thái: chờ người dùng duyệt spec. Đây là **định nghĩa mô hình**, chưa phải kế hoạch triển khai cụm: mục đích là để mã đơn máy hiện tại và các dự án sau không đóng cửa con đường phân tán. Chi tiết Kubernetes hoãn lại theo quyết định của người dùng (xem spec triển khai container §9).

## 1. Nguyên tắc
1. **Mỗi dữ liệu có đúng một nơi là nguồn sự thật.** Mọi bản sao khác (cache, chỉ mục) là dẫn xuất và dựng lại được.
2. **Dịch vụ không giữ trạng thái quyền uy**: nút nào cũng thay thế được; mất một nút chỉ mất các yêu cầu đang chạy.
3. **Không giao dịch phân tán.** Thứ tự ghi cố định (blob trước, metadata sau), mọi bước idempotent, có đối soát và dọn rác.
4. **Điều phối qua Postgres, không tự cài đồng thuận.** Rusty không cài Raft/Paxos/gossip; khóa, hợp đồng thuê (lease) và hàng đợi dùng Postgres. Chuyển đổi dự phòng của Postgres do công cụ/operator đảm nhiệm, Rusty chỉ phải chịu được nó.
5. **Tính đúng không phụ thuộc đồng hồ máy.** Đồng hồ chỉ dùng cho TTL, hạn ân hạn và log, với biên an toàn; lease và thứ tự dùng đồng hồ/bộ đếm của Postgres.
6. **Lỗi thì đóng (fail closed)** với phân quyền, ghi và dữ liệu quyền uy; **chỉ cache được phép mở (fail open)** bằng cách bỏ qua.
7. **Mọi thông điệp giữa các nút được xác thực, mã hóa và có phiên bản.**
8. **Khóa phân mảnh là `domain_id` (tenant).** Dữ liệu người dùng được thiết kế để có thể chia theo tenant sau này (mục 9).

## 2. Vai trò nút
| Vai trò | Trạng thái | Co giãn | Ghi chú |
|---|---|---|---|
| `edge` (proxy) | không | ngang | kết thúc TLS công khai, định tuyến |
| Dịch vụ giao thức (`imap`, `jmap`, `smtp-*`, `dav`, `filter`) | chỉ phiên đang chạy | ngang | chỉ nói chuyện với `store` |
| `store` | không quyền uy (cache và yêu cầu đang chạy) | ngang | cổng duy nhất tới dữ liệu |
| `worker` (GC, chuyển mailbox, xuất/nhập, đánh chỉ mục lại, sao lưu logic) | không (công việc nằm trong DB) | ngang | nhận việc bằng lease |
| Tầng dữ liệu: Postgres, kho đối tượng (hoặc volume blob), Valkey | quyền uy (Postgres, blob) / dẫn xuất (Valkey) | theo từng hệ | |

## 3. Chủ sở hữu dữ liệu
| Dữ liệu | Nguồn sự thật | Bản dẫn xuất |
|---|---|---|
| Tài khoản, domain, plan, mailbox, thư (metadata), `change_log`, bộ đếm, usage | Postgres (primary) | cache Valkey, chỉ mục |
| Nội dung thư (blob) | kho blob (đối tượng hoặc volume), có bản tham chiếu `blobs` trong Postgres | không |
| Cài đặt hệ thống | Postgres (schema hệ thống) khi nhiều nút; SQLite `system.db` khi một nút | cache |
| Hàng đợi thư đi (MTA) | Postgres (`outbound_queue` + lease) | không |
| Công việc nền | Postgres (`jobs`) | không |
| Capability phiên | Postgres (băm + hạn + trạng thái thu hồi) | cache trong tiến trình ngắn hạn |
| Chỉ mục tìm kiếm | dựng lại từ Postgres + blob | thư mục chỉ mục cục bộ |
| Khóa tenant | kho khóa (KMS/HSM; tệp `0400` ban đầu) | không (nhưng sao lưu riêng) |

## 4. Mô hình nhất quán
- **Tuần tự hóa theo mailbox:** mọi ghi vào một mailbox khóa hàng `mailboxes` trước (spec Postgres–Valkey §3d); `uid`, `msg_id`, `seq` gapless và đơn điệu theo mailbox. Các mailbox khác nhau ghi song song.
- **Ghi tuyến tính hóa tại primary.** Đọc quyền uy từ primary; đọc từ replica chỉ cho dữ liệu không quyết định phân quyền và luôn kèm kiểm tra `min_seq` (nếu replica chưa đuổi kịp `min_seq` thì đọc primary).
- **Đọc được cái vừa ghi:** phản hồi ghi trả `seq`; client gửi `min_seq` ở yêu cầu đọc sau (JMAP state, IMAP MODSEQ).
- **Không có thứ tự toàn cục giữa các mailbox hay tenant.** Không tính năng nào được dựa vào điều đó.
- **Thư đến (giao nhận) là at-least-once, hiệu ứng chính xác một lần:** `Deliver` mang `delivery_id` (băm ổn định của nội dung + người nhận + mã phiên MTA); bảng `deliveries(delivery_id UNIQUE)` làm cho lần giao lặp lại không tạo thư thứ hai. Trả mã tạm thời (SMTP 451) khi không chắc; MTA gửi lại an toàn.
- **Thứ tự ghi append:** `blob.put` (idempotent, đọc-sau-ghi nhất quán là yêu cầu bắt buộc với backend) -> giao dịch Postgres (upsert `blobs`, chèn thư, `change_log`, `deliveries`). Hỏng giữa chừng chỉ để lại blob mồ côi, GC xử lý (hạn ân hạn > độ trễ từ put tới commit tối đa, tính cả lúc Postgres failover).

## 5. Công việc nền, lease và fencing
- Bảng `jobs(id, kind, tenant, state, payload, attempts, lease_owner, lease_expires_at, epoch)`.
- Worker nhận việc bằng `SELECT ... FOR UPDATE SKIP LOCKED`; lease gia hạn định kỳ; **hạn lease tính bằng `now()` của Postgres**, không bằng đồng hồ worker.
- **Fencing:** mỗi lần nhận việc tăng `epoch`; mọi thao tác có tác dụng phụ ngoài DB (xóa blob, chép sang nút đích) mang `epoch`, và bước cuối chỉ thành công nếu `epoch` hiện tại khớp (so sánh trong cùng giao dịch). Worker "ma" (lease đã hết) không thể ghi đè kết quả.
- Mọi bước của job idempotent và tiếp tục được; trạng thái `moving`/`reindexing`... hiển thị trong console.
- **GC:** một chạy tại một thời điểm bằng khóa tư vấn Postgres (`pg_try_advisory_lock`), thay cho `.gc.lock` tệp khi nhiều nút.
- **Hàng đợi thư đi:** `outbound_queue` dùng cùng cơ chế lease; có `next_attempt_at` (đồng hồ DB), backoff, giới hạn số lần, và chuyển `dead` kèm cảnh báo.

## 6. Khám phá và thành viên
- Không có gossip hay dịch vụ đăng ký tự cài: dùng tên DNS/endpoint của nền tảng điều phối; cấu hình tĩnh qua cài đặt hệ thống.
- Mỗi dịch vụ có `health` (sống) và `ready` (sẵn sàng nhận việc: nối được `store`; `store` thì nối được Postgres, kho blob). Nút không ready thì bị gỡ khỏi định tuyến.
- Danh tính nút = chứng chỉ mTLS ngắn hạn mang **vai trò** (spec container §5); CA nội bộ; thu hồi bằng hết hạn nhanh, không dựa CRL.

## 7. Mô hình lỗi và hành vi
| Sự cố | Hành vi bắt buộc |
|---|---|
| Nút dịch vụ/`store` chết | yêu cầu đang chạy lỗi tạm thời; client gửi lại an toàn nhờ idempotency; không cần phục hồi trạng thái |
| `store` mất kết nối Postgres | từ chối ghi và đọc quyền uy với lỗi tạm thời (SMTP 451, IMAP `NO [UNAVAILABLE]`); **không** phục vụ dữ liệu cũ cho quyết định phân quyền |
| Postgres failover | giao dịch đang chạy bị hủy, client thử lại; không mất giao dịch đã commit (đồng bộ) hoặc chấp nhận mất tối đa theo cấu hình nhân bản (phải ghi rõ khi chọn async); cửa sổ nhiễu của hạn ân hạn GC được tính |
| Phân vùng mạng giữa `store` và kho blob | từ chối append/get tạm thời; metadata không bao giờ trỏ tới blob chưa ghi |
| Valkey chết/chậm/hỏng | bỏ qua cache (spec Postgres–Valkey §5 và §6); không lỗi người dùng |
| Worker chết giữa job | lease hết hạn, worker khác tiếp tục nhờ idempotent; `epoch` chặn worker cũ |
| Lệch đồng hồ giữa nút | không ảnh hưởng tính đúng; hạn ân hạn >= lệch tối đa đo được + biên; cảnh báo khi lệch vượt ngưỡng (kiểm tra bằng `now()` của DB) |
| Mất khóa tenant | mất dữ liệu tenant (có chủ ý); do đó sao lưu/ký quỹ khóa là yêu cầu vận hành |

**Không** có cam kết "luôn sẵn sàng khi phân vùng": hệ thống ưu tiên **nhất quán và an toàn** hơn sẵn sàng (lựa chọn CP) cho dữ liệu quyền uy; sẵn sàng được tăng bằng HA của Postgres/kho blob và bằng việc dịch vụ không trạng thái.

## 8. Phiên bản và nâng cấp cuốn chiếu
- Giao thức nội bộ có số phiên bản; `Hello` thương lượng; hỗ trợ N và N-1; khi lệch quá thì từ chối nối.
- Di chuyển CSDL theo kiểu **mở rộng rồi thu hẹp** (expand/contract): bước 1 thêm cột/bảng tương thích ngược, bước 2 triển khai mã mới, bước 3 mới bỏ phần cũ. Không migration nào khiến mã phiên bản N-1 hỏng.
- Cờ tính năng trong cài đặt hệ thống để bật dần.

## 9. Phân mảnh theo tenant (chuẩn bị, chưa làm)
- Mọi bảng dữ liệu người dùng có `domain_id` hoặc truy ra được qua `mailbox_id`; khóa chính/chỉ mục bắt đầu bằng khóa phân mảnh khi thích hợp.
- **Cấm khóa ngoại hay join giữa tenant**, trừ tham chiếu tới dữ liệu tham chiếu toàn cục nhỏ (plan hệ thống, trần hệ thống) được nhân bản cho mọi mảnh.
- Nhờ đó có thể chia sau này: mỗi mảnh = một cụm Postgres (và một tập tiền tố blob); `store` định tuyến theo `domain_id` qua bảng ánh xạ tenant -> mảnh (nằm ở cơ sở điều khiển nhỏ). Di chuyển tenant giữa mảnh là job (mục 5).
- Chưa chọn công cụ (Citus, phân mảnh ở tầng ứng dụng...). Quyết định khi benchmark chứng minh một primary không đủ.

## 10. Bảo mật trong môi trường phân tán
- mTLS giữa mọi dịch vụ và với Postgres/Valkey/kho blob; không có kênh nội bộ rõ ràng.
- Mỗi vai trò chỉ có quyền tối thiểu ở tầng dữ liệu (vai trò DB, chính sách bucket theo tiền tố tenant); thông tin xác thực ngắn hạn.
- Capability phiên: chuỗi ngẫu nhiên 256 bit, chỉ lưu băm, hết hạn ngắn, thu hồi tức thì (đổi mật khẩu, khóa tài khoản), mọi bản `store` cùng xác thực được vì băm nằm trong Postgres.
- Nhật ký kiểm toán quản trị và sự kiện bảo mật (giải mã cache thất bại, circuit breaker mở, từ chối vai trò) đẩy về một nơi tập trung; log không chứa nội dung thư, khóa, mật khẩu, và trace id không mang dữ liệu người dùng.
- Cô lập tenant được kiểm thử lại ở chế độ nhiều nút (mục 11).

## 11. Bất biến và cách kiểm thử
| Bất biến | Kiểm thử |
|---|---|
| I1 `uid`/`msg_id`/`seq` mỗi mailbox gapless, đơn điệu, kể cả nhiều `store` đồng thời | N `store` chèn song song vào cùng mailbox; kiểm tra hoán vị liên tục |
| I2 Giao nhận lặp không tạo thư thứ hai | gửi cùng `delivery_id` nhiều lần, từ nhiều nút, có ngắt giữa chừng |
| I3 Không bao giờ mất blob đang được tham chiếu khi GC chạy cùng put/append trên nhiều nút | stress put/GC đa nút, chèn lỗi ở mọi bước, kiểm tra mọi thư đọc được |
| I4 Một job chỉ có một chủ hiệu lực; worker hết lease không ghi đè được | ép hết lease giữa chừng, hai worker cùng chạy, kiểm tra `epoch` |
| I5 Cô lập tenant giữ nguyên với nhiều nút và cache chung | ma trận chéo tenant (spec Postgres–Valkey §3c.5) chạy ở cụm nhiều nút |
| I6 Postgres failover không làm mất giao dịch đã trả thành công | giết primary giữa tải ghi; so sánh số đã xác nhận và số có trong DB |
| I7 Phân vùng store<->blob/Postgres chỉ gây lỗi tạm thời, không dữ liệu hỏng | chèn phân vùng bằng công cụ mạng, kiểm tra tính toàn vẹn sau khi nối lại |
| I8 Nâng cấp cuốn chiếu N -> N+1 không mất yêu cầu | chạy tải liên tục trong lúc thay từng nút |

Công cụ: bộ chèn lỗi (kill, trễ, mất gói, phân vùng) trong môi trường Compose; kiểm tra mô hình đơn giản cho các bước idempotent; các test I1-I3 chạy được cả trên một máy (nhiều tiến trình `store` thật).

## 12. Việc chuẩn bị rẻ có thể làm sớm trong mã
1. `delivery_id` trong `Mailbox::append`/`Deliver` + bảng `deliveries` (idempotency).
2. Mẫu lease/fencing dùng đồng hồ DB, bảng `jobs` và `outbound_queue`, thư viện chung cho worker.
3. Capability lưu băm trong Postgres.
4. Không để `store` giữ trạng thái quyền uy trong bộ nhớ (kiểm tra khi viết dịch vụ).
5. Trait `SystemSettings` có backend Postgres.
6. Số phiên bản giao thức nội bộ ngay từ khung đầu tiên.

## 13. Ngoài phạm vi và điểm mở
- Nhiều vùng địa lý, Postgres active-active, nhân bản liên vùng.
- Chọn kho đối tượng cụ thể; chọn công cụ phân mảnh.
- Mức nhân bản đồng bộ hay bất đồng bộ của Postgres (đổi giữa mất dữ liệu có giới hạn và độ trễ ghi): quyết định khi benchmark.
- SLO cụ thể (độ trễ, sẵn sàng) chưa đặt.
- Hợp nhất `jobs`, `outbound_queue` thành một cơ chế hàng đợi chung hay giữ riêng.
