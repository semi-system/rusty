# Rusty - KMS: OpenBao sau một lớp bọc (wrapper) chuẩn hóa

Ngày: 2026-11-03. Trạng thái: chờ người dùng duyệt spec (sau đó mới lập kế hoạch triển khai chi tiết).

## 1. Quyết định và kiểm chứng
Quyết định của người dùng: **dùng OpenBao làm KMS, bọc một wrapper để chuẩn hóa kiến trúc.**

Kiểm chứng ngày 2026-11-03 (từ API GitHub và tài liệu trực tuyến của OpenBao qua công cụ tra cứu; cần kiểm lại khi triển khai):
- Giấy phép **MPL-2.0**; kho mã còn hoạt động (cập nhật ngày 2026-11-02, chưa bị lưu trữ), viết bằng Go.
- Engine **Transit** có: mã hóa/giải mã, `rewrap` (mã hóa lại mà không lộ bản rõ), sinh khóa dữ liệu (`datakey`, có thể tắt trả bản rõ), HMAC, ký/xác minh, xoay khóa (thêm phiên bản), dẫn xuất khóa theo ngữ cảnh, mã hóa hội tụ; kiểu khóa `aes256-gcm96` (mặc định) và `chacha20-poly1305`; `min_decryption_version` lưu trữ các phiên bản cũ.
- Niêm phong (seal): mặc định Shamir; tự mở (auto-unseal) qua KMS đám mây, HSM, hoặc khóa tĩnh.
- **Tài liệu tôi đọc KHÔNG nói rõ** (phải kiểm chứng bằng đọc tài liệu đầy đủ và test trước khi dựa vào): quy tắc xóa khóa (`deletion_allowed`) cho crypto-shredding; khả năng xuất khóa (`exportable`); yêu cầu `mlock`; hỗ trợ auto-unseal bằng Transit/PKCS11.

Ngoài phạm vi: HSM thật, nhiều vùng, chia sẻ khóa giữa các cụm OpenBao.

## 2. Vì sao có wrapper
Không dịch vụ nào của Rusty nói chuyện trực tiếp với OpenBao. Mọi thành phần chỉ biết **giao diện KMS của Rusty**. Lợi ích:
- Một nơi duy nhất thực thi chính sách (ai được dùng khóa nào của tenant nào, cho mục đích nào), kiểm toán và giới hạn tốc độ; OpenBao chỉ thấy một danh tính (wrapper), nên mô hình phân quyền của Rusty (vai trò, tenant) không bị trộn với chính sách của OpenBao.
- Đổi được thành phần phía sau (OpenBao, HSM, KMS đám mây, hay bản dev cục bộ) mà không đổi mã nghiệp vụ.
- Hợp đồng ổn định cho kiểm thử: trait Rust có bản giả.
- Nhóm ranh giới bảo mật gọn: chỉ wrapper mới giữ thông tin xác thực tới OpenBao.

## 3. Kiến trúc
```
store ─┐
certd ─┼─(mTLS, vai trò)─► kms-wrapper ─(mTLS, token ngắn hạn)─► OpenBao (Transit)
relay ─┘                    (Rust, không trạng thái)               (container riêng, mạng `kms`)
```
- **Mạng:** OpenBao nằm trong mạng `kms` riêng, **chỉ wrapper** tới được; giao diện web của OpenBao tắt; không công bố cổng ra host.
- **Ngôn ngữ:** wrapper bằng **Rust** (chính sách ngôn ngữ), crate `rusty-kms-api` (trait + kiểu + client) và dịch vụ `kms-wrapper`. OpenBao là thành phần bên thứ ba bằng Go, chạy nguyên bản từ image đã ghim digest.
- **Trait chính:**
```rust
#[async_trait] pub trait KeyManager: Send + Sync {
    async fn generate_data_key(&self, ctx: KeyCtx) -> Result<DataKey>;            // plaintext + wrapped + key_version
    async fn unwrap_data_key(&self, ctx: KeyCtx, wrapped: &WrappedKey) -> Result<Secret>;
    async fn rewrap(&self, ctx: KeyCtx, wrapped: &WrappedKey) -> Result<WrappedKey>;
    async fn rotate(&self, ctx: KeyCtx) -> Result<KeyVersion>;
    async fn hmac(&self, ctx: KeyCtx, data: &[u8]) -> Result<[u8; 32]>;           // khóa không rời KMS
    async fn shred(&self, ctx: KeyCtx) -> Result<()>;                              // crypto-shredding, có phê duyệt
    async fn health(&self) -> KmsHealth;                                           // sealed/unsealed/unavailable
}
pub struct KeyCtx { pub tenant: DomainId, pub purpose: Purpose } // Purpose: Blob, Cache, TlsKey, RelayCreds, AcmeAccount, DnsCreds
```
- **Bản triển khai:** `OpenBaoKeyManager` (qua wrapper), `InMemoryKeyManager` (test), `LocalFileKeyManager` (**chỉ dev**, từ chối chạy khi cấu hình là production).

## 4. Thứ bậc khóa
- Mỗi cặp (tenant, mục đích) có **một khóa Transit riêng**, đặt tên cố định `t{domain_id}-{purpose}`; không dùng chung giữa mục đích để một lỗi ở mục đích này không lộ mục đích khác.
- Dữ liệu được mã hóa bằng **khóa dữ liệu (DEK)** do KMS sinh: bản rõ chỉ nằm trong bộ nhớ của người gọi, bản đã bọc (`wrapped`, kèm `key_version`) lưu trong DB cạnh dữ liệu (mã hóa phong bì). Dữ liệu liên kết (AAD) gồm tenant, mục đích, định danh bản ghi để chống hoán đổi.
- **Các mục đích và chỗ dùng:**
  | Mục đích | Dùng cho | Kiểu dùng |
  |---|---|---|
  | `Blob` | khóa HMAC đặt tên blob theo tenant (spec Postgres-Valkey §3c.4b) | DEK được cache trong bộ nhớ `store` (TTL, mục 6) hoặc gọi `hmac` |
  | `Cache` | khóa mã hóa giá trị cache Valkey | DEK cache trong bộ nhớ |
  | `TlsKey` | khóa riêng của chứng chỉ TLS tùy chỉnh (tenant-tls §5) | DEK bọc từng khóa |
  | `RelayCreds` | thông tin xác thực SMTP relay | DEK bọc từng bản ghi |
  | `AcmeAccount`, `DnsCreds` | khóa tài khoản ACME, token DNS-01 của `certd` | DEK bọc |
- **Không cần** khóa chính nằm trong tệp: các spec trước dùng "HKDF từ khóa chính ở tệp `0400`" nay được thay bằng KMS này (mục 9).

## 5. Phân quyền và xác thực
- Danh tính người gọi = vai trò trong chứng chỉ mTLS ngắn hạn (spec container §5). **Ma trận (biên dịch sẵn trong wrapper):**
  | Vai trò | Được phép |
  |---|---|
  | `store` | `generate_data_key`, `unwrap_data_key`, `rewrap`, `hmac` cho `Blob`, `Cache`, `RelayCreds`, `TlsKey` của mọi tenant |
  | `certd` | `generate_data_key`, `unwrap_data_key` cho `TlsKey`, `AcmeAccount`, `DnsCreds` |
  | `kms-admin` (chỉ qua `rustyctl kms ...` có xác thực mạnh) | `rotate`, `shred`, xem trạng thái, kiểm toán; **không** `unwrap` |
  | còn lại | không có |
- Wrapper giữ **một danh tính tới OpenBao** (AppRole hoặc mTLS/JWT; chọn khi triển khai) với chính sách Transit tối thiểu: `encrypt`, `decrypt`, `datakey`, `rewrap`, `hmac`, `keys/<prefix>` đọc, `rotate` cho vai trò admin; **không** quyền xuất khóa, không xóa khóa (trừ đường `shred` có phê duyệt), không tạo engine/chính sách. Token ngắn hạn, tự gia hạn và thu hồi khi wrapper thoát.
- Tenant A không thể giải mã khóa của B: khóa Transit khác nhau theo tenant, AAD ràng buộc tenant, và wrapper kiểm tra `KeyCtx.tenant` khớp bản ghi bọc. Có test chéo tenant (mục 8).

## 6. Khả dụng, bộ nhớ đệm và vận hành
- **Tính sẵn sàng phụ thuộc OpenBao:** OpenBao đang niêm phong hoặc không với tới thì `unwrap`, `generate_data_key` và `hmac` thất bại với `Unavailable`; mọi người gọi phải **đóng khi lỗi** (không chạy mà không có khóa). Ví dụ: không giải mã được thông tin xác thực relay thì thư đi qua relay đó vào hàng đợi thử lại; không có khóa blob thì từ chối ghi thư (SMTP 451) chứ không ghi tên blob sai.
- **Bộ nhớ đệm DEK trong tiến trình** (cho đường nóng như đặt tên blob, mã hóa cache): TTL ngắn mặc định **15 phút**, kích thước giới hạn, xóa an toàn khi hết hạn, không ghi ra đĩa. Đánh đổi có chủ ý: **độ trễ crypto-shredding và thu hồi khóa tối đa bằng TTL**; sau `shred` hoặc xoay khóa, bản cache cũ còn hiệu lực tối đa 15 phút. Wrapper cung cấp phiên bản khóa để người gọi phát hiện khóa cũ.
- **Niêm phong khi khởi động (quyết định vận hành):** mặc định **Shamir (ví dụ 3 trên 5 phần chia)** do con người giữ; sau mỗi lần OpenBao khởi động lại, hệ thống **không giải mã được gì cho đến khi mở niêm phong thủ công**. Chấp nhận ở giai đoạn đầu (tải nhỏ, có runbook). Về sau cân nhắc auto-unseal (khóa tĩnh, KMS đám mây hoặc HSM theo tài liệu), nhưng mỗi cách chuyển rủi ro sang nơi giữ khóa của lớp tự mở; cần quyết định riêng khi sản xuất.
- **Lưu trữ OpenBao:** lưu trữ Raft tích hợp (một nút, sau đó ba nút khi cần HA), volume riêng mã hóa lúc nghỉ nếu có, quyền `0700`; sao lưu bằng snapshot Raft **cùng với** quy trình giữ các phần chia khóa (sao lưu vô dụng nếu mất khóa mở niêm phong).
- **Cứng hóa container OpenBao:** image ghim digest, user không phải root, rootfs chỉ đọc (volume dữ liệu riêng), `no-new-privileges`, giao diện web tắt, không cổng ra host, giới hạn tài nguyên, TLS bắt buộc. **Chi tiết `mlock`**: tài liệu tôi đọc không nói; theo trí nhớ, OpenBao/Vault dùng `mlock` để chống swap và hoặc cần capability `IPC_LOCK` (mâu thuẫn `cap_drop: ALL`), hoặc tắt `mlock` kèm tắt swap trên host. Phải kiểm tra tài liệu và chọn một cách (đề xuất: tắt swap + `disable_mlock` có ghi lý do, hoặc chỉ cấp `IPC_LOCK` cho riêng container này) trước khi triển khai.
- **Kiểm toán hai lớp:** (a) thiết bị kiểm toán của OpenBao ghi mọi yêu cầu vào một đích chỉ-thêm (tệp trên volume riêng hoặc socket), (b) wrapper ghi nhật ký riêng: người gọi (vai trò), tenant, mục đích, thao tác, kết quả, độ trễ; **không bao giờ ghi khóa, bản rõ, hay bản bọc đầy đủ**. Nếu không ghi được kiểm toán thì thao tác bị từ chối (đóng khi lỗi).
- **Xoay khóa:** `rotate` thêm phiên bản mới của khóa Transit; công việc nền (hàng đợi `jobs`) `rewrap` các DEK đã lưu sang phiên bản mới theo lô, và chỉ khi xong mới nâng `min_decryption_version`. Xoay định kỳ mặc định 90 ngày, hoặc ngay khi nghi ngờ lộ.
- **Crypto-shredding (`shred`):** vô hiệu hóa/xóa khóa Transit của tenant (cơ chế chính xác cần kiểm chứng, vì tài liệu tôi đọc không nói về `deletion_allowed`), yêu cầu phê duyệt hai người (hai danh tính `kms-admin`), kiểm toán, và ghi trong `admin_audit`. Sau shred các DEK đã bọc của tenant không giải mã được (chấp nhận cửa sổ TTL của mục này cho bộ nhớ đệm).
- **Dự phòng thảm họa khóa:** mất toàn bộ phần chia khóa niêm phong = mất mọi dữ liệu được bảo vệ bằng khóa ở đây (có chủ ý). Quy trình ký quỹ phần chia khóa là yêu cầu vận hành, được ghi trong runbook.

## 7. Giao diện wrapper (API nội bộ)
- Giao thức nội bộ như các dịch vụ khác của Rusty (khung có phiên bản, thương lượng phiên bản, giới hạn kích thước 64 KiB, hạn chót, mTLS; spec container §5). Các lệnh: `GenerateDataKey`, `Unwrap`, `Rewrap`, `Hmac`, `Rotate`, `Shred`, `Health`.
- Kiểu `Secret` không `Debug`/`Display`/`Serialize`, xóa bộ nhớ khi hủy (giới hạn của việc xóa trong Rust như đã ghi ở P4); `WrappedKey` là chuỗi không rõ có tiền tố phiên bản và `key_version`.
- Giới hạn tốc độ theo (vai trò, tenant); phát hiện bất thường (số lần `Unwrap` thất bại, truy cập hàng loạt) báo động và có thể tự khóa vai trò.
- Lỗi trả về cố định, không lộ chi tiết OpenBao hay chi tiết khóa.

## 8. Kiểm thử
- **Hợp đồng chung** cho mọi `KeyManager` (bản giả, bản OpenBao, bản cục bộ) bằng một bộ test chung.
- **Tích hợp với OpenBao thật** (Docker, image ghim digest, chế độ dev hoặc khởi tạo thật với Shamir 1/1 trong test): sinh/giải mã khóa dữ liệu, `rewrap`, xoay khóa và rewrap hàng loạt rồi nâng `min_decryption_version`, `hmac` ổn định giữa các lần gọi, khóa của tenant A không giải mã được dữ liệu bọc của B (và AAD sai thì thất bại), shred làm dữ liệu không giải mã được, **OpenBao niêm phong/dừng giữa chừng thì người gọi đóng khi lỗi** (kịch bản chaos), khởi động lại và mở niêm phong thì phục hồi, token wrapper hết hạn thì tự gia hạn, kiểm toán ghi đủ trường và không chứa bản rõ (quét), từ chối khi không ghi được kiểm toán.
- **Phân quyền:** mỗi vai trò chỉ gọi được thao tác của nó (test âm cho mọi ô còn lại của ma trận); chứng chỉ sai vai trò/hết hạn bị từ chối.
- **Rò rỉ:** quét log, lỗi, phản hồi, `docker inspect`, biến môi trường của mọi container tìm bản rõ khóa; `Secret` không in được.
- **Hiệu năng:** đo độ trễ `GenerateDataKey/Unwrap/Hmac` và hiệu quả bộ nhớ đệm DEK ở tải 116 thư/s (cổng G2); không để KMS thành điểm nghẽn.
- **Fuzz** bộ giải mã khung API và `WrappedKey`.
- **Cứng hóa:** test cấu hình container (rootfs, capability, mạng chỉ wrapper tới OpenBao, UI tắt) tích hợp vào checker compose.

## 9. Tác động tới các spec khác (cần sửa khi duyệt)
- **Tenant TLS** (§5, §6d): khóa riêng bọc bằng DEK từ KMS (`TlsKey`), không dùng "HKDF từ khóa chính ở tệp"; `certd` gọi KMS cho `TlsKey`, `AcmeAccount`, `DnsCreds`.
- **Postgres-Valkey** (§3c.4, §3c.4b): khóa HMAC đặt tên blob và khóa mã hóa cache lấy từ KMS (`Blob`, `Cache`); bỏ "khóa chính ở tệp `0400`".
- **SMTP relay** (roadmap, mục 0c): thông tin xác thực `RelayCreds`.
- **Process model / container:** thêm hai dịch vụ `kms-wrapper` và `openbao`, mạng `kms`, vai trò `kms-admin` cho `rustyctl kms`.
- **Phân tán:** thêm KMS vào danh sách điểm lỗi đơn và vào ma trận lỗi (OpenBao niêm phong thì hệ thống ngừng giải mã nhưng không mất dữ liệu).

## 10. Phân kỳ triển khai (sau khi duyệt)
1. `rusty-kms-api`: trait, kiểu `KeyCtx`, `Secret`, `WrappedKey`, bản giả và bộ test hợp đồng.
2. Dựng OpenBao trong Compose (cứng hóa, Raft một nút, kiểm toán), script khởi tạo/mở niêm phong cho dev, chính sách Transit tối thiểu.
3. `kms-wrapper`: giao thức, mTLS, ma trận vai trò, kiểm toán, giới hạn tốc độ, bộ nhớ đệm DEK ở phía client, ánh xạ `OpenBaoKeyManager`.
4. Tích hợp: `store` (Blob, Cache, RelayCreds), `certd` (TlsKey, ACME, DNS).
5. Xoay khóa và rewrap bằng `jobs`; shred có phê duyệt; `rustyctl kms`.
6. Chaos, rò rỉ, hiệu năng, runbook (mở niêm phong, mất phần chia khóa, khôi phục).

## 11. Rủi ro và điểm mở
- **OpenBao là thành phần lớn bằng Go** (thêm bề mặt tấn công, thêm chuỗi cung ứng, phải theo dõi advisory và nâng cấp nhanh); lựa chọn của người dùng, giảm bằng cô lập mạng, chính sách tối thiểu, wrapper và bản ghim digest.
- **Mở niêm phong thủ công** làm hệ thống ngừng giải mã sau mỗi lần khởi động lại; cần quyết định auto-unseal khi lên sản xuất.
- **Điểm lỗi đơn** của KMS (một nút): HA ba nút Raft ở giai đoạn sau.
- Các điểm chưa kiểm chứng ở mục 1 (xóa khóa, xuất khóa, `mlock`, các loại auto-unseal) có thể buộc đổi thiết kế `shred` hoặc cấu hình container.
- Tính di động của wrapper: chỉ có bản OpenBao; bản HSM/KMS đám mây là dự án sau.
- Độ trễ bộ nhớ đệm DEK (15 phút) là đánh đổi giữa hiệu năng và tốc độ thu hồi khóa; có thể giảm cho `TlsKey`, `RelayCreds`.
