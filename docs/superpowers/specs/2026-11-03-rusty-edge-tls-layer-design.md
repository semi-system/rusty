# Rusty - Biên: HAProxy (tầng 4) + lớp TLS riêng (`tlsd`)

Ngày: 2026-11-03. Trạng thái: chờ người dùng duyệt spec. **Thay thế** phần lớn mục 3b (Traefik) của spec container; những yêu cầu không phụ thuộc sản phẩm trong 3b vẫn áp dụng (xem mục 9).

## 0. CẬP NHẬT QUYẾT ĐỊNH (người dùng, 2026-11-03, sau khi xem cấu hình nginx stream tham khảo)
**Triển khai trước: HAProxy làm biên theo cách A** (HAProxy kết thúc TLS ngầm 465/993/995 bằng chứng chỉ nạp từ tệp, mã hóa lại bằng mTLS đã xác minh tới dịch vụ; các cổng STARTTLS 25/587/143/110 chuyển nguyên vẹn, dịch vụ tự làm STARTTLS bằng chứng chỉ riêng; PROXY v2 tới dịch vụ). **Nếu không ổn thì quay lại cách B với `tlsd` (mục 1-10 dưới đây) sau.** Mục 1-10 giữ nguyên làm thiết kế dự phòng.
- Kế hoạch triển khai cách A: `docs/superpowers/plans/2026-11-03-edge-haproxy.md` (thay `2026-11-03-edge-skeleton.md`, không được chạy kế hoạch Traefik cũ).
- "Không ổn" được định nghĩa cụ thể trong Task H3 của kế hoạch đó: báo cáo go/no-go dựa trên bộ test hành vi (PROXY v2 cùng TLS tới backend, xác minh backend bằng mTLS, chống giả mạo PROXY, giới hạn kết nối theo IP, thay chứng chỉ không làm đứt kết nối). Người dùng quyết định cuối cùng.
- Cái giá đã chấp nhận của cách A: HAProxy (C) giữ khóa riêng TLS thư trong bộ nhớ và thấy dạng rõ ở 465/993/995; dịch vụ phải giữ chứng chỉ cho STARTTLS (do `certd` cấp) và chính sách TLS nằm ở hai nơi (đồng bộ bằng một nguồn cấu hình và test phiên bản TLS trên mọi cổng).
- Cấu hình nginx stream mà người dùng dán làm tham chiếu **không** được sao chép nguyên: nó không xác minh chứng chỉ backend, không mTLS, không PROXY, không giới hạn kết nối (xem phân tích trong sổ SDD).

## 1. Quyết định của người dùng
- **HAProxy làm biên** (điểm duy nhất công bố cổng ra host).
- **Một lớp riêng xử lý TLS** (cách "B"): HAProxy chỉ làm tầng 4 cho thư và **không giải mã**; lớp TLS giữ mọi chứng chỉ và xử lý cả TLS ngầm lẫn STARTTLS.
- Hệ quả đã chấp nhận: thêm một thành phần và một chặng mạng; đổi lại chứng chỉ, khóa và chính sách TLS nằm **một nơi duy nhất**, dịch vụ phía sau không cần tự làm STARTTLS, và HAProxy (viết bằng C) không bao giờ thấy mật khẩu hay nội dung thư.

Lý do không dùng thành phần có sẵn cho lớp TLS (kiểm tài liệu 2026-11-03): nginx mail (kết thúc STARTTLS được nhưng bắt buộc qua dịch vụ xác thực HTTP nhận mật khẩu, TLS tới backend không rõ); `stunnel` (có tùy chọn `protocol` cho imap/pop3/smtp nhưng PROXY protocol chỉ nhắc v1, chiều nhận không nói rõ, không có giới hạn kết nối). Chính sách thư viện của roadmap (dùng sẵn trước) vẫn áp dụng ở mức crate: `rustls`/`tokio-rustls`, một crate phân tích PROXY protocol v2 đã được đánh giá, `tokio`; chỉ phần máy trạng thái STARTTLS là mã mới.

## 2. Kiến trúc
```
Internet
  │  25 465 587 993 143 995 110 (thư)           443 / 80 (HTTP)
  ▼
HAProxy  ── tầng 4, KHÔNG giải mã thư ──────────  HTTPS: xem mục 8
  │  PROXY v2 (IP thật), giới hạn theo IP, thời gian chờ, kiểm tra sức khỏe
  ▼
tlsd  (Rust, một container, nhiều cổng nghe nội bộ)
  │  giữ chứng chỉ từ certd; xử lý TLS ngầm và STARTTLS; áp một chính sách TLS duy nhất
  │  PROXY v2 + TLV SSL (đã qua TLS, phiên bản, bộ mã hóa) → mTLS nội bộ
  ▼
dịch vụ (smtp-in, smtp-submit, imap, pop3): chỉ nghe TLS ngầm nội bộ + mTLS, tin PROXY chỉ từ tlsd
```
- Mạng: `edge` (HAProxy công bố cổng), `tls` (HAProxy ↔ tlsd), `app` (tlsd ↔ dịch vụ). Dịch vụ không nối `edge`/`tls`.
- Dịch vụ phía sau **không còn phải cài STARTTLS**: nhận kết nối đã giải mã, được mã hóa lại bằng mTLS giữa `tlsd` và dịch vụ, kèm PROXY v2 mang địa chỉ máy khách và thông tin TLS gốc (TLV SSL). Giao thức ứng dụng (IMAP/POP3/SMTP) trong dịch vụ biết kết nối "đã bảo mật" nhờ PROXY, nên cho phép AUTH.
- Mọi tên máy/chứng chỉ do `certd` cấp; `tlsd` nạp chứng chỉ từ tệp `tmpfs` chỉ-đọc và nạp lại khi đổi (SIGHUP hoặc theo dõi tệp), không làm đứt kết nối đang có.

## 3. HAProxy (tầng 4) - yêu cầu
Các tính năng cần dùng (mới chỉ xác nhận **tên** trong chỉ mục tài liệu, **chưa đọc hành vi**; phải đọc phần chi tiết và kiểm bằng test trước khi dựa vào): bảng `stick-table` với `track-sc`/`src_conn_cur`/`src_conn_rate` và `tcp-request connection reject`; `send-proxy-v2`; các `timeout client/client-fin/client-hs/server/tunnel/connect`; `tcp-request inspect-delay`; kiểm tra sức khỏe `tcp-check connect/send/expect` hoặc `option smtpchk`.
- Chế độ `mode tcp` cho mọi cổng thư; đích là `tlsd` trên mạng `tls`; **không `ssl`/`crt`** trên các cổng thư.
- Giới hạn theo IP nguồn: số kết nối đồng thời, tốc độ kết nối mới, tổng; vượt thì từ chối ngay ở biên.
- Thời gian chờ: bắt tay/nhàn rỗi ngắn, `timeout client-hs` hoặc tương đương, thời gian tối đa của một phiên; chống kết nối chậm.
- Kiểm tra sức khỏe `tlsd` theo giao thức (đọc lời chào, hoặc `smtpchk`), loại khỏi vòng khi hỏng.
- Cấu hình **tĩnh từ tệp** gắn chỉ-đọc; không bảng điều khiển/lệnh chạy-thời-gian (runtime API) ra ngoài; ghi nhật ký JSON; chạy user không phải root, rootfs chỉ đọc, `cap_drop ALL` (+`NET_BIND_SERVICE` nếu bind cổng <1024), `no-new-privileges`, ghim digest; nạp lại cấu hình bằng `reload` liền mạch.
- Ngôn ngữ C: giảm rủi ro bằng cứng hóa container, bề mặt cấu hình nhỏ, theo dõi advisory và nâng cấp nhanh, và vì **HAProxy không giữ khóa TLS thư và không thấy mật khẩu**.

## 4. `tlsd` (Rust) - yêu cầu
**Phạm vi:** một dịch vụ tối thiểu làm đúng hai việc: kết thúc TLS và xử lý đoạn **trước-TLS** của STARTTLS cho IMAP, POP3, SMTP; sau đó chuyển byte nguyên vẹn. Không phân tích nội dung thư, không xác thực.

**Cổng và chế độ** (cổng nghe nội bộ ánh xạ từ HAProxy):
| Cổng công khai | Chế độ trong `tlsd` | Ghi chú |
|---|---|---|
| 465, 993, 995 | TLS ngầm | bắt tay TLS ngay, rồi chuyển tiếp; lời chào backend được chuyển cho máy khách |
| 587, 143, 110 | STARTTLS **bắt buộc** | trước TLS chỉ cho phép tập lệnh tối thiểu (mục 5); không có AUTH trước TLS |
| 25 | STARTTLS **cơ hội** (SMTP giữa máy chủ) | cho phép tiếp tục không TLS theo chính sách; vẫn đưa vào dịch vụ nhận thư kèm cờ "không TLS" trong PROXY TLV |

**Chính sách TLS duy nhất:** TLS tối thiểu 1.2 (ưu tiên 1.3), chỉ bộ mã hóa hiện đại, cùng một cấu hình cho mọi cổng, đọc từ một tệp; test phiên bản TLS trên **mọi** cổng.

**Chứng chỉ:** chọn chứng chỉ theo SNI từ tệp do `certd` ghi; SNI lạ hoặc không có SNI → chứng chỉ mặc định không khớp hoặc từ chối (cấu hình được, mặc định từ chối với cổng người dùng); tuyệt đối không cấp chứng chỉ theo yêu cầu.

**Kết nối ngược tới dịch vụ:** mTLS với chứng chỉ ngắn hạn do CA nội bộ cấp (vai trò `tlsd` trong chứng chỉ); gửi PROXY v2 (kèm TLV SSL: có TLS, phiên bản, bộ mã hóa, SNI) trước khi bắt tay mTLS; **dịch vụ chỉ chấp nhận PROXY từ danh tính `tlsd`**. Cần kiểm chứng thứ tự "PROXY trước rồi TLS" đúng như dịch vụ (đã có quy tắc dừng-và-báo ở kế hoạch cổng vào).

**Tài nguyên:** giới hạn số kết nối, bộ nhớ mỗi kết nối, kích thước dòng lệnh trước TLS (ví dụ 512 byte, theo từng giao thức), thời gian tối đa cho bắt tay TLS và cho giai đoạn trước TLS, tốc độ bắt tay (chi phí mật mã) theo IP thật (từ PROXY của HAProxy).

**Cô lập:** user riêng, rootfs chỉ đọc, `cap_drop ALL`, không `unsafe` (cấm trong mã dự án), seccomp/landlock, mạng chỉ tới `tls` (nhận) và `app` (gọi dịch vụ), không ra Internet, không truy cập DB hay `store`; chứng chỉ và khóa chỉ trong `tmpfs` chỉ-đọc, không ghi đĩa bền; tắt core dump.

## 5. Xử lý STARTTLS an toàn (phần rủi ro nhất, cần fuzz và review riêng)
Mỗi giao thức có một máy trạng thái nhỏ, quy tắc chung:
1. **Tự sinh phản hồi trước-TLS, không nối backend trước khi có TLS.** `tlsd` tự gửi lời chào và danh sách khả năng trước TLS; chỉ sau bắt tay TLS thành công mới mở kết nối tới dịch vụ.
2. **Tập lệnh trước-TLS tối thiểu, bất kỳ thứ gì ngoài đó bị từ chối rồi ngắt:** IMAP: `CAPABILITY`, `STARTTLS`, `NOOP`, `LOGOUT` (lời chào quảng cáo `STARTTLS` và `LOGINDISABLED`); POP3: `CAPA`, `STLS`, `QUIT`; SMTP: `EHLO/HELO` (trả `STARTTLS`, không quảng cáo `AUTH`), `STARTTLS`, `QUIT`, `NOOP`, `RSET` (cổng 25 cơ hội cho phép thêm `MAIL/RCPT/DATA` theo chính sách không TLS: chuyển qua backend dịch vụ nhận thư với cờ không-TLS).
3. **Chống chèn lệnh (CVE-2011-0411 và họ hàng):** sau khi gửi trả lời cho `STARTTLS`, nếu bộ đệm đọc còn **bất kỳ byte nào** từ máy khách trước bắt tay TLS thì **ngắt kết nối** (không xử lý, không chuyển tiếp); không dùng lại bộ đệm trước-TLS sau bắt tay; mọi trạng thái giao thức trước-TLS bị bỏ sau khi nâng cấp (máy khách phải gửi lại `EHLO`/`CAPABILITY`).
4. **Không đọc quá một dòng/lệnh mỗi bước**, giới hạn độ dài dòng, từ chối ký tự điều khiển, `NUL`, và dòng kết thúc không hợp lệ; không có pipelining trước khi nhận trả lời `STARTTLS`.
5. **Sau TLS:** với TLS ngầm, chuyển lời chào của dịch vụ cho máy khách; với STARTTLS, **nuốt** lời chào của dịch vụ (máy khách đã nhận lời chào từ `tlsd`) và để máy khách bắt đầu lại giao thức. Kết nối phía sau luôn là TLS ngầm nội bộ.
6. **Hạ cấp:** cổng bắt buộc TLS không có đường không-TLS; trên 25 mọi thư không-TLS phải mang cờ rõ ràng tới dịch vụ.
7. Test dựa trên các mẫu tấn công đã biết (chèn lệnh sau `STARTTLS` trong cùng gói TCP; gửi `AUTH` trước TLS; lệnh lạ; dòng dài; chia nhỏ byte từng ký tự; đóng giữa chừng bắt tay), và fuzz máy trạng thái bằng `cargo-fuzz`.

## 6. Chống lạm dụng - phân vai rõ
| Tầng | Trách nhiệm |
|---|---|
| HAProxy | Giới hạn kết nối/tốc độ theo IP, thời gian chờ thô, kiểm tra sức khỏe, loại IP bị chặn, bảo vệ SYN ở mức ứng dụng |
| `tlsd` | Giới hạn bắt tay TLS và đọc trước-TLS theo IP thật, kích thước/dòng lệnh, thời gian giai đoạn trước-TLS |
| Dịch vụ giao thức | **Vẫn bắt buộc** (spec container §3b.3b): giới hạn theo IP/tài khoản, thời gian chờ ứng dụng, chống dò mật khẩu, nhật ký giao thức; không tin tầng trước |

## 7. Kiểm thử (bộ kiểm thử biên viết theo hành vi, độc lập sản phẩm)
- Chuyển nguyên vẹn nhị phân qua cả chuỗi HAProxy → `tlsd` → dịch vụ giả; STARTTLS nâng cấp đúng và máy khách TLS thật kiểm chứng chứng chỉ, tên, phiên bản TLS, trên **mọi** cổng thư.
- IP thật tới dịch vụ qua chuỗi hai PROXY (HAProxy→`tlsd`→dịch vụ); PROXY giả mạo từ máy khách bị từ chối ở HAProxy và ở `tlsd`; dịch vụ từ chối PROXY không từ `tlsd`.
- Cổng 25 cơ hội: thư không TLS mang cờ; cổng người dùng không bao giờ cho AUTH trước TLS.
- Chèn lệnh sau `STARTTLS`, lệnh trước-TLS ngoài tập cho phép, dòng quá dài, byte-từng-ký-tự, đóng giữa chừng.
- Giới hạn: kết nối chậm (slowloris) ở từng cổng bị đóng, một IP mở hàng nghìn kết nối bị HAProxy cắt (đo hành vi `stick-table` thật), bắt tay TLS hàng loạt bị giới hạn.
- Chứng chỉ: SNI lạ/không SNI, thay chứng chỉ khi chạy không đứt kết nối, chứng chỉ hết hạn.
- Cứng hóa: HAProxy không có `ssl`/`crt` trên cổng thư; không container nào ngoài HAProxy công bố cổng; `tlsd` và HAProxy chạy không root, rootfs chỉ đọc, không `docker.sock`.
- Fuzz máy trạng thái STARTTLS của `tlsd` cho ba giao thức; chạy cùng bộ kiểm thử biên trên một triển khai thay thế (ví dụ Traefik hoặc nginx cho HTTP) để giữ khả năng đổi sản phẩm.

## 8. HTTPS (webmail, JMAP, DAV, admin console)
**ĐÃ XÁC NHẬN (người dùng, 2026-11-03): "edge proxy nên giữ chứng chỉ, đó là bản chất của nó, không sao"** - HAProxy giữ chứng chỉ và khóa riêng và kết thúc TLS cho cả HTTPS lẫn các cổng thư TLS ngầm (cách A); không cần đưa HTTPS qua `tlsd`. Phần dưới giữ lại làm mô tả thiết kế; những chỗ ghi "cần bạn xác nhận" nay đã được xác nhận. Giả định gốc: HAProxy kết thúc TLS HTTP (chế độ `mode http`, chứng chỉ từ `certd`, `Host`/đường dẫn để định tuyến, giới hạn tốc độ/kích thước), rồi mã hóa lại bằng mTLS tới dịch vụ HTTP. Đây khiến HAProxy giữ khóa TLS của các tên máy web và thấy nội dung HTTP (khác với thư), nên cần bạn xác nhận: chấp nhận hay muốn HTTPS cũng đi qua `tlsd`. Đi qua `tlsd` được cho HTTP/1.1 và HTTP/2 nếu `tlsd` chỉ chuyển byte, nhưng định tuyến theo `Host` cần HAProxy hiểu HTTP, nên sẽ cần HAProxy ở sau `tlsd` trong chế độ http (thêm một chặng). Để sau.

## 9. Những phần của spec container 3b còn áp dụng
Cổng và quyền: chỉ biên công bố cổng, không `docker.sock`, cứng hóa container (mục 4); PROXY v2 và danh sách IP tin cậy; chính sách TLS tối thiểu; chứng chỉ do `certd` cấp theo danh sách tên máy tường minh (ACME HTTP-01/DNS-01, Let's Encrypt production, Pebble cho test); hạn chứng chỉ trên dashboard và công tắc failover ACME (tenant-tls §6c); phân vai chống lạm dụng của 3b.3b. Những phần riêng cho Traefik (file provider, tắt Docker provider, `certificatesResolvers`) **không còn áp dụng** và sẽ được thay bằng cấu hình HAProxy khi viết kế hoạch triển khai.

## 10. Rủi ro và điểm mở
- **`tlsd` là mã mới chạm luồng chưa mã hóa và giữ mọi khóa TLS thư**: thành phần giá trị cao; cần fuzz, review bảo mật riêng, sandbox chặt, và bộ kiểm thử tấn công STARTTLS.
- **Điểm lỗi đơn và chặng thêm**: hiệu năng bắt tay TLS tập trung ở một thành phần; mở rộng ngang được vì không giữ trạng thái (nhiều bản `tlsd`, HAProxy cân bằng).
- **Hành vi HAProxy chưa xác nhận** (mới biết tên từ khóa): bộ test hành vi sẽ quyết định; nếu thiếu, có phương án thay (Traefik/Envoy cho tầng 4, nhưng Traefik không có giới hạn theo IP đã xác nhận).
- **PROXY chuỗi hai chặng** (HAProxy→`tlsd`→dịch vụ) và PROXY trước mTLS chưa kiểm chứng.
- HTTPS chưa quyết (mục 8).
- Cần crate PROXY v2 đã được đánh giá (giấy phép, bảo trì, fuzz) hoặc tự viết bộ phân tích nhỏ.
