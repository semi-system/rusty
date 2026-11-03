# Rusty - Dự án 1: Nền tảng (foundation)

Ngày: 2026-11-02. Trạng thái: chờ người dùng duyệt spec.

## 1. Bối cảnh và mục tiêu
Rusty là bản làm lại toàn bộ stack Zimbra FOSS bằng Rust (xem kế hoạch tổng ở `<home>\.claude\plans\review-h-th-ng-v-zesty-moon.md`). Quyết định đã chốt:
- Thiết kế mới, giao thức chuẩn, kèm công cụ import từ Zimbra (dự án 8). Không giữ SOAP/LDAP/MariaDB schema của Zimbra.
- Dự án 1 chỉ cung cấp **thư viện** (không có server mạng): tạo domain/account, cây folder, lưu thư, flag, move, expunge, change-log.
- Hướng lưu trữ: trait `MetadataStore` + `BlobStore`, backend SQLite trước, Postgres sau (cùng lược đồ logic).

Ngoài phạm vi: SMTP, IMAP, JMAP, tìm kiếm, calendar/contacts, proxy, admin UI, import.

## 2. Crate
```
rusty-core       ids, Error, kiểu dùng chung (không phụ thuộc DB)
rusty-store      trait MetadataStore + BlobStore; impl SQLite, impl FS
rusty-directory  Domain/Account/Alias/List/Plan + cấu hình phân cấp + Authenticator
rusty-mime       bọc mail-parser/mail-builder: envelope, header, preview, size
```
Phụ thuộc một chiều: `directory` và `store` dùng `core`; `directory` dùng `store`; `mime` chỉ dùng `core`. Thư viện cấp cao `Mailbox` (trong `rusty-store`) gom store + mime.

## 3. Kiểu dữ liệu
- `AccountId`, `MailboxId`, `MessageId`, `FolderId`: newtype, không lẫn nhau.
- `MessageId` tăng đơn điệu theo mailbox. IMAP UID tăng đơn điệu theo folder.
- `Folder.role`: `inbox | sent | drafts | trash | spam | archive | custom`. Mỗi role (trừ `custom`) chỉ xuất hiện một lần trong mailbox.
- Flag chuẩn là bitset (`seen, flagged, answered, draft, deleted`); keyword tùy ý nằm ở bảng riêng (không giới hạn 64 như bitmask `tags` của Zimbra).

## 4. Lược đồ DB (đã siết ràng buộc)

Nguyên tắc chung:
- Mọi khóa ngoại phải khai báo thật. SQLite: `PRAGMA foreign_keys = ON` bắt buộc cho mỗi kết nối (kiểm tra khi mở pool, lỗi nếu không bật), bảng khai báo `STRICT`. Postgres: dùng FK có `DEFERRABLE INITIALLY IMMEDIATE`.
- Mọi cột `NOT NULL` trừ khi NULL có nghĩa nghiệp vụ (ghi rõ bên dưới).
- Khóa ngoại tổng hợp `(mailbox_id, x)` để một mailbox không thể tham chiếu dữ liệu của mailbox khác.
- Chính sách xóa: `CASCADE` chỉ cho dữ liệu sở hữu trực tiếp (xóa account kéo theo mailbox); `RESTRICT` cho quan hệ tham chiếu (domain, plan, blob, folder còn thư).
- Giá trị rời rạc dùng `CHECK (... IN (...))`; JSON dùng `CHECK (json_valid(...))`; số đếm dùng `CHECK (>= 0)`.
- Địa chỉ email không lưu cột `email` dư thừa: ghép từ `local_part` + `domain.name` để không thể lệch nhau. Tên miền và local part lưu chữ thường (`CHECK (x = lower(x))`).

```sql
CREATE TABLE domains (
  id          INTEGER PRIMARY KEY,
  name        TEXT NOT NULL UNIQUE CHECK (length(name) > 0 AND name = lower(name)),
  config      TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(config)),
  created_at  INTEGER NOT NULL
) STRICT;

CREATE TABLE plans (
  id          INTEGER PRIMARY KEY,
  name        TEXT NOT NULL UNIQUE CHECK (length(name) > 0),
  config      TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(config))
) STRICT;

CREATE TABLE accounts (
  id            INTEGER PRIMARY KEY,
  local_part    TEXT NOT NULL CHECK (length(local_part) > 0 AND local_part = lower(local_part)),
  domain_id     INTEGER NOT NULL REFERENCES domains(id) ON DELETE RESTRICT,
  plan_id       INTEGER REFERENCES plans(id) ON DELETE RESTRICT,   -- NULL = dùng mặc định domain
  password_hash TEXT,                                              -- NULL = xác thực ngoài (LDAP/OIDC)
  status        TEXT NOT NULL CHECK (status IN ('active','locked','disabled')),
  config        TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(config)),
  created_at    INTEGER NOT NULL,
  UNIQUE (local_part, domain_id)
) STRICT;

CREATE TABLE aliases (
  id          INTEGER PRIMARY KEY,
  local_part  TEXT NOT NULL CHECK (length(local_part) > 0 AND local_part = lower(local_part)),
  domain_id   INTEGER NOT NULL REFERENCES domains(id) ON DELETE RESTRICT,
  account_id  INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  UNIQUE (local_part, domain_id)
) STRICT;

CREATE TABLE lists (
  id          INTEGER PRIMARY KEY,
  local_part  TEXT NOT NULL CHECK (length(local_part) > 0 AND local_part = lower(local_part)),
  domain_id   INTEGER NOT NULL REFERENCES domains(id) ON DELETE RESTRICT,
  UNIQUE (local_part, domain_id)
) STRICT;

CREATE TABLE list_members (
  list_id     INTEGER NOT NULL REFERENCES lists(id) ON DELETE CASCADE,
  account_id  INTEGER REFERENCES accounts(id) ON DELETE CASCADE,
  external    TEXT,
  CHECK ((account_id IS NULL) <> (external IS NULL)),              -- đúng một trong hai
  UNIQUE (list_id, account_id),
  UNIQUE (list_id, external)
) STRICT;
```
Một địa chỉ (`local_part@domain`) chỉ được thuộc đúng một trong account, alias, list. SQL không biểu diễn được trực tiếp qua nhiều bảng, nên dùng **trigger `BEFORE INSERT/UPDATE`** trên ba bảng để từ chối trùng chéo, cộng test cho từng trường hợp.

```sql
CREATE TABLE blobs (
  hash      TEXT PRIMARY KEY CHECK (length(hash) = 64 AND hash NOT GLOB '*[^0-9a-f]*'),
  size      INTEGER NOT NULL CHECK (size >= 0),
  refcount  INTEGER NOT NULL DEFAULT 0 CHECK (refcount >= 0),
  zero_since INTEGER CHECK (zero_since IS NULL OR zero_since >= 0)  -- migration 0002; xem dưới
) STRICT;

CREATE TABLE mailboxes (
  id               INTEGER PRIMARY KEY,
  account_id       INTEGER NOT NULL UNIQUE REFERENCES accounts(id) ON DELETE CASCADE,
  next_msg_id      INTEGER NOT NULL DEFAULT 1 CHECK (next_msg_id >= 1),
  next_change_seq  INTEGER NOT NULL DEFAULT 1 CHECK (next_change_seq >= 1),
  uid_validity     INTEGER NOT NULL CHECK (uid_validity > 0)
) STRICT;

CREATE TABLE folders (
  mailbox_id    INTEGER NOT NULL REFERENCES mailboxes(id) ON DELETE CASCADE,
  id            INTEGER NOT NULL CHECK (id >= 1),
  parent_id     INTEGER,                                           -- NULL = gốc
  name          TEXT NOT NULL CHECK (length(name) > 0 AND instr(name, '/') = 0),
  role          TEXT NOT NULL DEFAULT 'custom'
                CHECK (role IN ('inbox','sent','drafts','trash','spam','archive','custom')),
  uid_validity  INTEGER NOT NULL CHECK (uid_validity > 0),
  next_uid      INTEGER NOT NULL DEFAULT 1 CHECK (next_uid >= 1),
  PRIMARY KEY (mailbox_id, id),
  CHECK (parent_id IS NULL OR parent_id <> id),
  FOREIGN KEY (mailbox_id, parent_id) REFERENCES folders(mailbox_id, id) ON DELETE NO ACTION
) STRICT;
-- NO ACTION (kiểm tra cuối câu lệnh) thay vì RESTRICT (kiểm tra từng hàng): xóa trực tiếp folder cha còn con
-- vẫn bị từ chối, nhưng xóa account kéo theo cả cây folder (CASCADE) thì thành công.
CREATE UNIQUE INDEX folders_sibling_name ON folders (mailbox_id, COALESCE(parent_id, 0), name);
CREATE UNIQUE INDEX folders_role ON folders (mailbox_id, role) WHERE role <> 'custom';

CREATE TABLE messages (
  mailbox_id     INTEGER NOT NULL,
  id             INTEGER NOT NULL,
  folder_id      INTEGER NOT NULL,
  uid            INTEGER NOT NULL CHECK (uid >= 1),
  blob_hash      TEXT NOT NULL REFERENCES blobs(hash) ON DELETE RESTRICT,
  size           INTEGER NOT NULL CHECK (size > 0),
  internal_date  INTEGER NOT NULL,
  subject        TEXT NOT NULL DEFAULT '',
  from_addr      TEXT NOT NULL DEFAULT '',
  to_addrs       TEXT NOT NULL DEFAULT '',
  message_id_hdr TEXT,                                             -- NULL nếu thư không có Message-ID
  thread_id      INTEGER NOT NULL,
  flags          INTEGER NOT NULL DEFAULT 0 CHECK (flags BETWEEN 0 AND 31),
  deleted_at     INTEGER,                                          -- NULL = chưa xóa mềm
  PRIMARY KEY (mailbox_id, id),
  FOREIGN KEY (mailbox_id) REFERENCES mailboxes(id) ON DELETE CASCADE,
  FOREIGN KEY (mailbox_id, folder_id) REFERENCES folders(mailbox_id, id) ON DELETE NO ACTION,  -- như trên
  UNIQUE (mailbox_id, folder_id, uid)
) STRICT;
CREATE INDEX messages_thread ON messages (mailbox_id, thread_id);
CREATE INDEX messages_msgid  ON messages (mailbox_id, message_id_hdr);
CREATE INDEX messages_deleted_at ON messages (deleted_at) WHERE deleted_at IS NOT NULL;  -- 0002, cho purge

CREATE TABLE keywords (
  mailbox_id  INTEGER NOT NULL,
  message_id  INTEGER NOT NULL,
  keyword     TEXT NOT NULL CHECK (length(keyword) BETWEEN 1 AND 255 AND keyword = lower(keyword)),
  PRIMARY KEY (mailbox_id, message_id, keyword),
  FOREIGN KEY (mailbox_id, message_id) REFERENCES messages(mailbox_id, id) ON DELETE CASCADE
) STRICT;

CREATE TABLE change_log (
  mailbox_id  INTEGER NOT NULL REFERENCES mailboxes(id) ON DELETE CASCADE,
  seq         INTEGER NOT NULL CHECK (seq >= 1),
  kind        TEXT NOT NULL CHECK (kind IN ('message','folder')),
  item_id     INTEGER NOT NULL,                                    -- không FK: item có thể đã bị xóa
  op          TEXT NOT NULL CHECK (op IN ('create','update','destroy')),
  PRIMARY KEY (mailbox_id, seq)
) STRICT;
```

Ràng buộc kèm theo (trigger, kiểm tra bằng test):
- `blobs.refcount` được trigger `AFTER INSERT/DELETE/UPDATE OF blob_hash` trên `messages` cập nhật; code ứng dụng không tự ghi. Job dọn chỉ xóa blob có `refcount = 0` và quá hạn ân hạn.
- `blobs.zero_since` (migration `0002_blob_zero_since.sql`): `NULL` khi `refcount > 0`, ngược lại là unix giây lúc `refcount` về 0 lần gần nhất. Chỉ trigger ghi: `blobs_zero_since_ins` (`AFTER INSERT`, `refcount = 0` → `unixepoch()`), `blobs_zero_since_released` (`AFTER UPDATE OF refcount`, `NEW = 0 AND OLD <> 0` → `unixepoch()`), `blobs_zero_since_referenced` (`AFTER UPDATE OF refcount`, `NEW > 0` → `NULL`); các trigger refcount trên `messages` kích hoạt chúng. Migration backfill một lần `zero_since = unixepoch()` cho hàng `refcount = 0` có sẵn.
- Cấm tạo chu trình trong cây folder: trigger `BEFORE UPDATE OF parent_id` đi ngược lên chuỗi cha, từ chối nếu gặp lại chính nó (độ sâu tối đa 64).
- Folder có role khác `custom` là folder hệ thống: tầng `Mailbox` không cho xóa, đổi tên hay đổi role.
- Cấm xóa folder còn con hoặc còn thư (khóa ngoại `NO ACTION`; tầng `Mailbox` trả `Conflict`). Việc chuyển thư vào `trash` trước khi xóa là việc của tầng giao thức (IMAP/JMAP), không phải của `Mailbox`. Thư đã expunge trong folder được xóa thật cùng folder.
- `uid_validity` của folder mới phải **lớn hơn** mọi giá trị đã cấp trước đó trong mailbox (RFC 3501/9051 §2.3.1.1, vì id folder có thể được tái dùng). Cài đặt: `mailboxes.uid_validity` làm cột high-water mark; folder mới nhận `max(now_secs, mailboxes.uid_validity + 1)` và cột được cập nhật trong cùng transaction; vượt `u32::MAX` thì `Quota`.
- `Mailbox` chạy mọi thao tác ghi trong **một transaction**, bao gồm tăng `next_*`, ghi `change_log` (seq lấy từ `next_change_seq`). Test kiểm tra không có lỗ hổng/trùng `seq`.
- Migration bằng `sqlx::migrate!`; mỗi migration có test "schema sau migrate khớp snapshot" và test vi phạm từng ràng buộc (INSERT sai phải lỗi).

## 5. Trait lưu trữ
```rust
#[async_trait] pub trait BlobStore {
    async fn put(&self, bytes: Bytes) -> Result<BlobHash>;   // idempotent, SHA-256; nội dung đã có → làm mới mtime
    async fn get(&self, h: &BlobHash) -> Result<Bytes>;
    async fn delete(&self, h: &BlobHash) -> Result<()>;
    // Hỗ trợ GC:
    async fn list_shard(&self, shard: u8) -> Result<Vec<BlobEntry>>;   // 1/256 kho; bỏ qua tên lạ, symlink, tmp, .gcq
    async fn list(&self) -> Result<Vec<BlobEntry>>;                    // mặc định: gộp 256 shard
    async fn delete_if_older(&self, h: &BlobHash, cutoff: SystemTime) -> Result<bool>;
    async fn sweep_temp_before(&self, cutoff: SystemTime) -> Result<u64>; // chỉ `.*.tmp`, không bao giờ `.gcq`
    async fn sweep_temp(&self, older_than: Duration) -> Result<u64>;      // mặc định: sweep_temp_before(now - d)
    async fn heal_quarantine(&self, cutoff: SystemTime) -> Result<HealReport>;
    async fn try_lock_gc(&self) -> Result<GcLock>;                      // Conflict("gc already running")
}
// BlobEntry { hash, size, modified_unix }
#[async_trait] pub trait MetadataStore {
    async fn tx(&self) -> Result<Box<dyn Tx>>;               // mọi thao tác ghi qua Tx
}
// Tx (đã thu gọn theo quyết định lúc triển khai): chỉ `commit()` và `rollback()`.
// Các thao tác nghiệp vụ (create_folder, append, set_flags, move_message, expunge,
// list_folder, changes_since) nằm ở `Mailbox`/`Directory`. Ranh giới backend mức phương thức
// (trait cho Mailbox và Directory) sẽ làm ở dự án Postgres (spec Postgres-Valkey §3d).
```
`Mailbox::append` ghi blob trước, rồi metadata trong transaction. Nếu transaction hỏng thì blob mồ côi, job dọn xử lý (blob chưa có `blobs` row hoặc `refcount = 0`).

**Dọn rác (`rusty_store::gc`):**
- `purge_deleted(store, older_than)`: xóa thật thư có `deleted_at < older_than`, tối đa 1000 hàng mỗi transaction `BEGIN IMMEDIATE`; keyword cascade, refcount theo trigger, `change_log` giữ nguyên.
- `collect_blobs(store, blobs, grace)`: `grace` tối thiểu 1 giờ (`MIN_GC_GRACE`, nhỏ hơn → `Invalid`), mặc định khuyến nghị 24 giờ (`DEFAULT_GC_GRACE`). **`grace` phải lớn hơn độ trễ lớn nhất từ `put` tới commit của một `append`**: `put` ghi (hoặc với nội dung đã có, làm mới mtime) file trước khi transaction chèn hàng, nên file sắp có hàng luôn trẻ hơn `grace`. (`collect_blobs_at(.., now)` nhận `grace` bất kỳ, `#[doc(hidden)]`, chỉ cho test.)
- Mỗi lần chạy giữ khóa tư vấn `<blob root>/.gc.lock` (`File::try_lock`): chỉ một GC trên một blob root; GC thứ hai trả `Conflict("gc already running")` ngay. Khóa nhả khi drop (kể cả khi panic/return sớm).
- Thứ tự: (0) chữa quarantine; (1) hàng `refcount = 0 AND zero_since < now - grace`: xóa hàng trong `BEGIN IMMEDIATE` có kiểm tra lại, commit, rồi mới xóa file nếu mtime cũ hơn `grace`; (2) file không có hàng và mtime cũ hơn `grace` (kiểm tra lại không có hàng ngay trước khi xóa); (3) `sweep_temp_before`. Lỗi của từng blob được ghi vào `GcReport.errors` (chỉ hash, không đường dẫn/nội dung) và chạy tiếp. Không bao giờ xóa blob có `refcount > 0` (FK `RESTRICT` chặn cả khi refcount bị hỏng).
- `delete_if_older` đổi tên file sang `.<hash>.gc<pid>.<n>.gcq` (quarantine) trong cùng thư mục, kiểm tra mtime trên file đã đổi tên rồi xóa hoặc trả lại; trả lại không bao giờ ghi đè bản mới (hard link, `AlreadyExists` → bỏ bản quarantine). `put` sau khi làm mới mtime kiểm tra lại đường dẫn; thiếu thì ghi bản mới. Hệ quả: trong khe race hẹp này, `get` một blob đang được tham chiếu có thể trả `NotFound` trong chốc lát rồi blob được trả lại; không retry.
- Chữa quarantine lúc khởi đầu mỗi lần chạy: đường dẫn blob thiếu → trả bản quarantine về; đường dẫn blob có và bản quarantine cũ hơn `grace` → xóa bản quarantine; còn lại để yên.
- `FsBlobStore` mở file bằng `O_NOFOLLOW | O_NONBLOCK` (qua `rustix`, không `unsafe`) và kiểm tra `fstat` là file thường; duyệt thư mục không theo symlink; tên file không hợp lệ (kể cả UTF-8 nhiều byte) bị bỏ qua, không panic.

## 6. Directory và cấu hình
- Cấu hình phân cấp: global → domain → plan → account; ưu tiên account > plan > domain > global. Mỗi cấp là một JSON object; hiện mới kiểm tra "là object" khi ghi (chưa có schema theo khóa). Schema theo khóa, giới hạn kích thước và trần hệ thống sẽ do registry cài đặt đảm nhiệm (spec Postgres-Valkey §3b và §3c.2).
- Password hash argon2id. Trait `Authenticator` cho phép cắm LDAP/OIDC sau.
- `rusty.toml` chỉ chứa thứ cần lúc khởi động (đường dẫn DB, thư mục blob, log).

## 7. Lỗi và test
- `rusty_core::Error` (thiserror): `NotFound, Conflict, Quota, Invalid, Storage`, giữ `source`. Vi phạm ràng buộc DB được ánh xạ sang `Conflict`/`Invalid`, không lộ lỗi SQL thô.
- Test: property test cho tính đơn điệu của id/uid/seq; integration trên SQLite in-memory; test vi phạm ràng buộc (mỗi FK, CHECK, trigger có ít nhất một test âm); test crash giữa blob và metadata; round-trip MIME (hiện có 4 mẫu tổng hợp; cần bổ sung một corpus thư thật đã ẩn danh trước khi coi là đủ).

## 8. Tiêu chí hoàn thành
Tạo domain/account; cây folder; append 10.000 thư; đổi flag; move; expunge; đọc `changes_since`; mọi test xanh với `cargo test --workspace`; `cargo clippy -- -D warnings` sạch; test âm xác nhận DB từ chối dữ liệu sai quan hệ.

## 8b. Tư thế bảo mật (định hướng của dự án, do người dùng đặt)
Rusty ưu tiên bảo mật và khả năng chịu được zero-day. Nguyên tắc áp cho mọi crate, kể cả các dự án sau:
- **Không `unsafe` trong mã của dự án**: mỗi crate khai báo `#![forbid(unsafe_code)]`. Ngoại lệ cần ghi lý do trong spec.
- **Mọi đầu vào ngoài là thù địch**: parser (MIME, địa chỉ, sau này IMAP/SMTP/JMAP) không panic, có giới hạn kích thước, độ sâu, số phần; được fuzz.
- **Giới hạn tài nguyên mặc định**: kích thước thư tối đa (mặc định 50 MiB, cấu hình được, vượt thì `Error::Quota`), độ dài mật khẩu, độ sâu cây, số keyword mỗi thư.
- **Fail-closed**: lỗi thì từ chối, không "cho qua".
- **Chuỗi cung ứng**: ghim phiên bản (`Cargo.lock` commit), `cargo-deny` (giấy phép, nguồn, advisory) và `cargo-audit` trong CI, ít phụ thuộc, ưu tiên crate được bảo trì tốt.
- **Cô lập quyền (định hướng cho các dịch vụ mạng)**: mỗi giao thức chạy tiến trình riêng, quyền tối thiểu, sandbox (seccomp/landlock), parser chạy ở tiến trình ít quyền nhất. Dự án 1 chỉ chuẩn bị bằng cách giữ tách lớp rõ ràng.
- **Giảm thiệt hại khi lọt lỗi**: ràng buộc quan hệ ở DB (đã có), không lưu bí mật dạng rõ, log không chứa mật khẩu/hash/nội dung thư, cập nhật nóng nhờ restart từng dịch vụ.
- **Kiểm thử đối kháng**: fuzz (`cargo-fuzz`) cho `parse_envelope` và chuẩn hóa địa chỉ, test âm cho mỗi ràng buộc.

## 9. Rủi ro đã biết
- Ràng buộc "một địa chỉ thuộc một loại" dựa vào trigger, phải có bản Postgres tương đương.
- `COALESCE(parent_id,0)` trong index unique yêu cầu id folder bắt đầu từ 1 (đã đảm bảo bằng `CHECK`/sinh id từ 1).
- Chưa đối chiếu thuộc tính LDAP/TGZ của Zimbra (thuộc dự án 8).
