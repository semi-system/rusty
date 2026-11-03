# Rusty - Mục tiêu quy mô và hệ quả kiến trúc

Ngày: 2026-11-03. Trạng thái: chờ người dùng duyệt các con số (đây là **giả định thiết kế có thể chỉnh**, không phải số đo).

## 1. Quyết định của người dùng
- Tải thực tế hiện tại: khoảng 700-800 thư gửi đi và 700-800 thư nhận mỗi ngày cho cả hệ thống.
- **Thiết kế phải nhắm tải siêu lớn**, dù triển khai ban đầu rất nhỏ.
- Hệ quả: các quyết định trước đó (Postgres 18, Valkey, tách `store`, phân tán, container) **được giữ**; đề xuất giảm tải sang SQLite ở lượt trước **bị rút lại**.

Nguyên tắc: **thiết kế cho T3, triển khai nhỏ, kiểm chứng mở rộng bằng benchmark theo bậc**. Không vì thiết kế cho quy mô lớn mà bắt triển khai nhỏ phải chạy đủ mọi thành phần: cấu hình tối thiểu (một node) phải chạy được bằng cùng mã (mục 6).

## 1b. MỤC TIÊU ĐÃ CHỐT (người dùng, 2026-11-03) - thay cho mục tiêu T3 ở lượt trước
- **100.000 hộp thư và 1.000.000 thư mỗi ngày** (hiểu là tổng vào + ra; cần xác nhận nếu là mỗi chiều).
- **Nhân bản Postgres bất đồng bộ** (RPO > 0): xem hệ quả ở mục 1c.
- Quy ra tải: 1M thư/ngày ~ **11,6 thư/s trung bình, ~116 thư/s đỉnh** (hệ số 10). Với khoảng 10 câu lệnh DB mỗi thư vào, đỉnh cỡ ~1.200 câu lệnh/s ghi: nằm trong khả năng của **một primary Postgres** (tôi chưa đo; cổng G2 sẽ kiểm chứng). Số kết nối đồng thời giả định ~30.000 (30% hộp thư có phiên mở; giả định cần xác nhận). Dung lượng ~75 GB/ngày ~ 27 TB/năm với thư 75 KB (giả định, trước nén/dedupe).
- **Kết luận:** mục tiêu này là bậc **T2 (đã nới)**, không phải T3. **T3 trở thành mục tiêu mở rộng (stretch), không phải yêu cầu**. Phân mảnh nhiều cụm, bộ cân bằng tải chuyên dụng, hàng đợi sự kiện riêng **chưa cần** và được hoãn; nhưng **quy tắc khóa phân mảnh theo mailbox (mục 3) vẫn giữ làm quy tắc thiết kế rẻ** để không đóng cửa tương lai.

## 1c. Hệ quả của nhân bản bất đồng bộ (cần hiểu rõ)
- Khi primary hỏng và chuyển sang bản dự phòng, **những giao dịch vừa commit trên primary nhưng chưa tới bản dự phòng sẽ mất** (thường dưới vài giây, tùy độ trễ nhân bản).
- Với thư đến: nếu MTA đã nhận được phản hồi thành công (`250`) cho một thư thuộc cửa sổ bị mất, **thư đó mất thật** mà MTA gửi không gửi lại. Đây là rủi ro mất thư đã xác nhận, có xác suất thấp nhưng khác 0.
- Giảm thiểu (đề xuất, chọn khi triển khai): (a) giữ nhân bản bất đồng bộ cho phần lớn thao tác; (b) với **đường giao thư vào**, đặt `synchronous_commit` đồng bộ cho riêng giao dịch giao nhận khi có bản dự phòng đủ gần (Postgres cho phép cấu hình theo từng giao dịch), chấp nhận độ trễ ghi cao hơn; (c) lưu trữ WAL liên tục ra kho ngoài (PITR) để giới hạn mất dữ liệu ở các sự cố khác; (d) khi failover, ghi nhận khoảng WAL bị mất và tự đối soát (các `delivery_id` đã xác nhận trong nhật ký MTA/proxy nhưng không có trong DB được báo động).
- Đây là RPO đã được người dùng chọn; spec phân tán §7 (hàng "Postgres failover") phải sửa cho khớp: giao dịch đã trả thành công **có thể** mất trong cửa sổ nhân bản.

## 2. Các bậc quy mô (giả định, cần người dùng xác nhận)
| Bậc | Hộp thư | Thư/ngày (vào+ra) | Đỉnh (x10 trung bình) | Kết nối IMAP/JMAP đồng thời | Dung lượng thư/năm (giả định 75 KB/thư, chưa nén/dedupe) |
|---|---|---|---|---|---|
| **T1 - hiện tại** | ~vài chục-trăm | ~1.600 | ~0,2/s | vài chục | ~45 GB |
| **T2 - lớn (≈ MỤC TIÊU ĐÃ CHỐT: 100.000 hộp thư, 1.000.000 thư/ngày, đỉnh ~116/s)** | 100.000 | 2.000.000 (mục tiêu chốt: 1.000.000) | ~230/s (chốt: ~116/s) | ~50.000 (chốt giả định: ~30.000) | ~55 TB (chốt: ~27 TB) |
| **T3 - siêu lớn (mục tiêu mở rộng, KHÔNG phải yêu cầu hiện tại)** | 10.000.000 | 100.000.000 | ~11.600/s | ~2.000.000 | ~2,7 PB |

Ghi chú tính toán: T3 trung bình ~1.160 thư/s, đỉnh ~11.600/s; một thư vào kéo theo nhiều thao tác DB (chèn thư, đếm, `change_log`, thông báo đẩy, chỉ mục), nên tải ghi DB gấp vài lần tải thư. Kích thước thư 75 KB và tỉ lệ vào/ra chỉ là giả định; dung lượng là chi phí lớn nhất ở T3 và phụ thuộc chính sách lưu giữ, nén và dedupe.

Các con số T2 và T3 do tôi đặt để có điểm tựa tính toán; **cần người dùng thay bằng số thật của mục tiêu**.

## 3. Phát hiện quan trọng: khóa phân mảnh phải là mailbox, không phải tenant
Spec phân tán (§9) đề xuất phân mảnh theo `domain_id` (tenant). **Điều này sai ở quy mô lớn:** một tenant (một domain) có thể tự nó có hàng triệu hộp thư, lớn hơn sức chứa của một cụm Postgres, nên không thể "một tenant một mảnh". Zimbra cũng phân mảnh theo mailbox (mỗi mailbox gắn với một máy chủ), không theo domain.

**Quyết định thiết kế (đề xuất):**
- **Khóa phân mảnh dữ liệu mailbox = `mailbox_id` (nhóm hộp thư).** Thư mục tenant (domain, plan, tài khoản, alias, danh sách, cấu hình) là dữ liệu tra cứu nhỏ, đọc nhiều: một cụm riêng ("directory/control cluster") có replica đọc và cache.
- **Tenant vẫn là ranh giới bảo mật** (RLS, quota, khóa mã hóa, quản trị), nhưng **không** là đơn vị đặt dữ liệu. Một tenant có thể trải trên nhiều mảnh.
- Bảng ánh xạ `mailbox -> shard` nằm trong cụm điều khiển và được cache; định tuyến do `store`.
- **Hệ quả cần giải quyết cho các thao tác chéo mảnh:**
  - Khóa ngoại tổng hợp ép "cùng tenant" (ACL, alias, danh sách, `domain_admins`) hiện giả định cùng một DB. Khi tài khoản và mailbox nằm khác mảnh, các ràng buộc này không thể là khóa ngoại DB; phải tách: dữ liệu *quyền và tham chiếu* ở cụm điều khiển (có khóa ngoại), dữ liệu *thư* ở mảnh mailbox. Sửa spec Postgres-Valkey §3c và spec ACL: grant nằm ở cụm điều khiển hay cùng mảnh với mailbox chủ (chủ là nơi lưu), và kiểm tra quyền chéo mảnh qua API `store` có cache.
  - Mở rộng danh sách phân phối lớn (hàng trăm nghìn thành viên): phát qua job nền theo lô, không giải quyết đồng bộ trong một giao dịch; trần `MAX_LIST_MEMBERS` hiện 10.000 cần thay bằng cơ chế phân trang và phát lô.
  - Chuyển mailbox giữa mảnh (cân bằng tải): job nền có fencing (đã thiết kế).
  - Tìm kiếm toàn tenant, GAL: cần chỉ mục riêng cấp thư mục (không phải mỗi mailbox).
- **Các spec cần sửa:** phân tán §9 (khóa phân mảnh), Postgres-Valkey §3c.1 (ràng buộc chéo mảnh), ACL §4 (khóa ngoại), roadmap (thêm dự án cụm điều khiển).

## 4. Các điểm nghẽn dự kiến ở T3 và cách đối phó (cần kiểm chứng bằng đo)
| Thành phần | Vấn đề ở T3 | Hướng xử lý (đề xuất) |
|---|---|---|
| Ghi metadata thư | Một primary Postgres không đủ ở ~11.600 thư/s đỉnh | Phân mảnh theo mailbox (mục 3); mỗi mảnh một primary + replica |
| `messages`, `change_log` rất lớn | Bảng hàng tỷ dòng, vacuum, chỉ mục | Phân vùng (partition) theo `mailbox_id` băm và/hoặc thời gian; nén/cắt tỉa `change_log` (đã ghi trong §3d); lưu trữ thư cũ ra bảng lạnh |
| Hàng đợi việc nền và hàng đợi thư đi | `SKIP LOCKED` trên một bảng ở hàng chục nghìn/s | Hàng đợi theo mảnh; cân nhắc broker riêng cho sự kiện/thư đi (NATS JetStream hoặc tương đương) **tại bậc này** — **rút lại phần khuyến nghị "không cần message queue" của lượt trước, vốn chỉ đúng với T1** |
| Thông báo đẩy IDLE/EventSource | Hàng triệu kết nối, fan-out | Cổng kết nối (connection gateway) không trạng thái nặng + kênh phát tin (Valkey/NATS); tín hiệu có thể mất, `change_log` là nguồn sự thật (đã thiết kế) |
| Kết nối đồng thời (~2 triệu) | Giới hạn fd, bộ nhớ, cổng vào | Rust async (tokio) đủ; điều chỉnh kernel; nhiều bản dịch vụ giao thức; bộ cân bằng tải tầng 4 chuyên dụng/anycast thay cho một Traefik đơn lẻ ở T3 (Traefik vẫn dùng được ở T1-T2) |
| Blob | ~PB, nhiều nghìn đối tượng/giây | Kho đối tượng tương thích S3 theo tenant (đã quyết); `S3BlobStore`; chính sách lưu giữ/nén; dedupe chỉ trong tenant (đã quyết, tốn thêm dung lượng) |
| Chỉ mục tìm kiếm | ~10 triệu chỉ mục tantivy, bộ nhớ/đĩa | Chỉ mục theo mailbox trên nút tìm kiếm cục bộ, nạp lười, đẩy ra đĩa lạnh; hoặc phân đoạn theo nhóm hộp thư; đo trước khi chọn |
| Cache Valkey | Số khóa, băng thông | Valkey Cluster, hoặc nhiều cụm theo mảnh; đã thiết kế như cache không quyền uy |
| MTA thư đi | Danh tiếng IP, giới hạn nhà cung cấp, bounce | Nhiều IP gửi theo tenant, làm nóng IP, feedback loop, hàng đợi theo đích; tính năng riêng, không chỉ hiệu năng |
| Thư vào (SMTP) | Gấp nhiều lần thư hợp lệ do thư rác/quét | Chống lạm dụng tầng mạng, rspamd mở rộng ngang, giới hạn theo IP/ASN |
| Cụm điều khiển | Điểm nghẽn đọc toàn hệ thống | Replica đọc + cache nhiều tầng + giữ dữ liệu nhỏ; kiểm tra hiệu lực cache bằng `acl_version`/`settings_version` (đã thiết kế) |
| Khóa tenant, chứng chỉ | Số lượng lớn tên máy/tenant | `certd` mở rộng ngang theo tenant; giới hạn hạn mức ACME; chứng chỉ wildcard (DNS-01) |

## 5. Thang benchmark và cổng kiểm chứng
Mỗi bậc có "cổng": không tuyên bố hỗ trợ bậc đó nếu chưa qua.
| Cổng | Thử nghiệm | Tiêu chí đạt (đề xuất, chỉnh theo SLO) |
|---|---|---|
| G1 (T1) | Test hiện có + benchmark SQLite/Postgres ghi một mailbox và nhiều mailbox song song (1/4/16 người ghi, thư 2 KiB và 1 MiB) | ghi nhận đường cơ sở; không hồi quy giữa các lần |
| G2 | Postgres 18 một primary: 100.000 mailbox, tải 230 thư/s đỉnh + 5.000 phiên đọc | p99 giao thư < 200 ms; p99 `list_folder` < 50 ms; không mất/ trùng thư; không tăng lag replica bất thường |
| G3 | Phân mảnh: 8 mảnh mailbox, 1 cụm điều khiển, tải tổng 5.000 thư/s, 200.000 kết nối giữ | tuyến tính theo số mảnh (>= 70% hiệu quả khi nhân đôi mảnh); các bất biến I1-I8 giữ nguyên |
| G4 (T3 mô phỏng) | Tải tổng hợp ~11.600 thư/s, 2 triệu kết nối, 10 triệu mailbox (dữ liệu sinh, chỉ mục thưa) trên cụm thử nghiệm | đạt SLO; chaos (giết nút, phân vùng, failover) không mất dữ liệu; chi phí/thư được đo và báo cáo |
- Dữ liệu và bộ sinh tải tái lập (seed cố định), báo cáo lưu trong repo; so sánh giữa các lần chạy; thay đổi lớn phải chạy lại cổng liên quan.
- Phần cứng thử nghiệm: chưa có; việc dựng môi trường G3-G4 (nhiều máy hoặc dịch vụ đám mây tạm) là chi phí cần người dùng chấp thuận riêng.

## 6. Triển khai nhỏ vẫn phải gọn
Cấu hình tối thiểu cho T1: một máy, các container: `edge`, một `store` kèm cơ sở dữ liệu (Postgres một node trong cùng máy, hoặc SQLite cho chế độ "nhỏ" nếu giữ backend đó), các dịch vụ giao thức, Valkey tùy chọn (tắt được: `NoCache`), `certd`, không kho đối tượng (dùng volume), không cụm điều khiển tách riêng (gộp cùng DB, schema tách). Mọi thành phần phân tán đều **tắt mặc định** và bật bằng cấu hình khi cần; cùng một mã, cùng một giao thức nội bộ. Tiêu chí: người vận hành quy mô T1 không phải chạy Kubernetes, broker hay cụm nhiều node.

## 7. Thay đổi so với các kết luận trước
| Kết luận trước | Nay |
|---|---|
| SQLite đủ cho tải 1.600 thư/ngày; Postgres/Valkey tùy chọn | **Rút lại**: giữ Postgres 18 + Valkey là đích; SQLite chỉ cho dev/test/hệ thống nhỏ và system DB |
| Không cần message queue | Đúng ở T1, **không đủ ở T3**: đánh giá lại ở G3 |
| Phân mảnh theo `domain_id` (tenant) | **Sai ở T3**: phân mảnh theo mailbox (mục 3) |
| Hạ ưu tiên P-43/P-44/P-45 (phân tầng lưu trữ, nhiều node, Kubernetes) | **Khôi phục** như dự án chính; thêm dự án cụm điều khiển và phân mảnh (roadmap) |
| P6 (lô và tối ưu truy vấn) giảm ưu tiên | **Khôi phục**: hiệu năng ghi theo lô và truy vấn ít vòng là điều kiện để đạt G2-G3 |
| Traefik làm cổng vào | Giữ ở T1-T2; T3 dùng bộ cân bằng tải tầng 4 chuyên dụng (Traefik chỉ còn cho HTTP) |

## 7b. Cập nhật theo mục tiêu đã chốt (mục 1b)
| Mục | Trạng thái |
|---|---|
| Postgres 18 + Valkey là đích sản xuất, **backend Postgres làm trước** | Giữ |
| Một primary + replica bất đồng bộ + lưu trữ WAL | Đủ cho mục tiêu; kiểm chứng ở G2 |
| Phân mảnh nhiều cụm theo mailbox, cụm điều khiển tách riêng (P-13b) | **Hoãn** (post-MVP, chỉ khi vượt mục tiêu); giữ quy tắc "không ràng buộc chéo mailbox" |
| Hàng đợi sự kiện riêng | Chưa cần ở mục tiêu này |
| Traefik làm cổng vào | Đủ cho mục tiêu này (bộ cân bằng tầng 4 chuyên dụng chỉ khi tới T3) |
| Kho đối tượng S3 / Kubernetes | Hoãn; volume cục bộ + sao lưu đủ cho giai đoạn đầu, nhưng ~27 TB/năm cần kế hoạch dung lượng và `S3BlobStore` sớm trong vòng 1-2 năm |
| P6 (lô, tối ưu truy vấn) | Giữ ở mức vừa phải; không còn là điều kiện cứng cho G2 |
| Bộ test G3-G4 (đa mảnh, T3) | **Hoãn**; chỉ G1-G2 là bắt buộc |

## 8. Việc phải làm tiếp (đưa vào lộ trình)
1. **Xác nhận con số mục 2** (hoặc thay bằng số thật), đặt SLO (độ trễ, sẵn sàng, RPO/RTO).
2. **Sửa các spec liên quan** theo mục 3: phân tán §9, Postgres-Valkey §3c/§3d, ACL §4; viết spec **cụm điều khiển và phân mảnh mailbox** (dự án mới `P-13b`).
3. **Benchmark G1** ngay (đã hoãn) để có đường cơ sở trước khi viết backend Postgres.
4. Viết backend Postgres (P-13) với **ngay từ đầu**: khóa phân mảnh `mailbox_id` trong khóa chính/chỉ mục, không khóa ngoại chéo mailbox ngoài cụm điều khiển, phân vùng bảng lớn, đồng hồ do ứng dụng cấp.
5. Trong code hiện tại, thay đổi rẻ nên làm sớm để khỏi chặn phân mảnh: không thêm ràng buộc hay truy vấn nối bảng bắc qua `mailbox_id` khác nhau; mọi truy vấn dữ liệu thư đã có `mailbox_id` (đã đúng); id toàn cục tăng dần (vd `INTEGER PRIMARY KEY` của SQLite) không được lộ ra ngoài API (dùng id theo mailbox hoặc id không đoán được) vì khi phân mảnh sẽ không còn duy nhất toàn cục.
6. Quyết định công cụ (Citus, phân mảnh ứng dụng, broker) chỉ sau G2-G3, dựa trên số đo.

## 9. Rủi ro và điểm mở
- Chi phí xây và vận hành T3 rất lớn so với tải thực tế hiện tại; đây là quyết định có chủ ý của người dùng, nhưng cần nhớ rằng **mã và thiết kế cho T3 chưa được kiểm chứng ở quy mô đó**; chỉ cổng G3-G4 mới chứng minh.
- Phân mảnh theo mailbox làm phức tạp hóa ACL, danh sách, GAL, tìm kiếm chéo mailbox và báo cáo theo tenant; đã nêu ở mục 3.
- Con số T2/T3 là giả định; nếu mục tiêu thật lớn hơn nhiều (hàng trăm triệu hộp thư), cần thiết kế lại các phần lưu trữ và định tuyến.
- Phần cứng và chi phí môi trường thử nghiệm chưa có.
