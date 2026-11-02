# Rusty Foundation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Thư viện Rust (không có server mạng) tạo domain/account, cây folder, lưu thư, đổi flag, move, expunge và đọc change-log, trên SQLite với ràng buộc quan hệ chặt.

**Architecture:** Cargo workspace 4 crate (`rusty-core`, `rusty-store`, `rusty-directory`, `rusty-mime`). `rusty-store` giữ trait `MetadataStore`/`BlobStore`, impl SQLite + filesystem, và facade `Mailbox`. Lược đồ SQL do migration `sqlx` tạo, mọi bất biến nằm trong DB (FK, CHECK, trigger) và được test bằng test âm.

**Tech Stack:** Rust stable, tokio, sqlx (sqlite), async-trait, thiserror, serde/serde_json, argon2, sha2, mail-parser, proptest, tempfile. Phát triển và chạy trong WSL (`wsl.exe -e bash -lc`), repo ở `/mnt/d/code/Rusty`.

**Spec:** `docs/superpowers/specs/2026-11-02-rusty-foundation-design.md`

## Global Constraints

- Lược đồ SQL đúng như spec §4 (tên bảng, cột, CHECK, FK, index, trigger). Mọi bảng `STRICT`.
- Mỗi kết nối SQLite phải có `PRAGMA foreign_keys = ON`; mở pool mà tắt thì trả `Error::Storage`.
- `blobs.hash` là SHA-256 hex thường, đúng 64 ký tự. `messages.size > 0`.
- Domain, `local_part`, keyword lưu chữ thường. Email không lưu cột riêng.
- `blobs.refcount` chỉ do trigger cập nhật; code ứng dụng không ghi.
- Mọi thao tác ghi của `Mailbox` nằm trong một transaction, gồm tăng `next_*` và ghi `change_log`.
- Lỗi: một enum `rusty_core::Error` (`NotFound, Conflict, Quota, Invalid, Storage`); vi phạm ràng buộc DB ánh xạ sang `Conflict`/`Invalid`, không lộ lỗi SQL thô.
- Ngoài phạm vi: SMTP, IMAP, JMAP, tìm kiếm, calendar/contacts, proxy, import.
- Bảo mật là ưu tiên hàng đầu (spec §8b): mỗi crate có `#![forbid(unsafe_code)]`; đầu vào ngoài không được gây panic; kích thước thư tối đa mặc định 50 MiB (vượt thì `Error::Quota`); fail-closed.
- Hoàn thành khi: `cargo test --workspace` xanh, `cargo clippy --workspace -- -D warnings` sạch.

## Review Focus

- Email viết hoa/Unicode (`Bob@Example.COM`, `bob@münchen.de`): chuẩn hóa chữ thường, so khớp không phân biệt hoa thường; địa chỉ rỗng bị `Invalid`.
- Thư rỗng (0 byte) hoặc không có header `Message-ID`: rỗng bị `Invalid` (vì `size > 0`); thiếu Message-ID vẫn lưu được với `message_id_hdr = NULL`.
- Hai `append` đồng thời vào cùng folder: UID và `seq` không trùng, không lỗ hổng.
- Xóa account/mailbox: `refcount` blob giảm đúng, blob dùng chung với account khác không bị xóa.
- Folder: tên chứa `/`, trùng tên cùng cha, tạo chu trình khi đổi cha, xóa folder hệ thống hoặc folder còn thư đều bị từ chối.

---

### Task 1: Workspace và `rusty-core`

**Files:**
- Create: `Cargo.toml` (workspace), `rust-toolchain.toml`, `.gitignore`, `crates/rusty-core/{Cargo.toml,src/lib.rs,src/ids.rs,src/error.rs}`
- Test: `crates/rusty-core/tests/ids.rs`

**Interfaces:**
- Produces: `AccountId(pub i64)`, `MailboxId`, `FolderId`, `MessageId`, `DomainId`, `PlanId` (newtype, `Copy + Eq + Hash + Debug`); `BlobHash(String)` với `BlobHash::of(&[u8]) -> BlobHash`, `BlobHash::parse(&str) -> Result<BlobHash>`, `as_str()`; `Flags` (bitset u8: `SEEN=1, FLAGGED=2, ANSWERED=4, DRAFT=8, DELETED=16`, `bits()`, `from_bits(u8) -> Option<Flags>`); `FolderRole` enum (`Inbox, Sent, Drafts, Trash, Spam, Archive, Custom`, `as_str()`/`FromStr`); `Error`, `Result<T>`.

- [ ] **Step 1: Khởi tạo** `git init` ở gốc repo, tạo workspace + crate, commit rỗng.
- [ ] **Step 2: Viết test** trong `tests/ids.rs`: `blob_hash_of_known_input` (SHA-256 của `b"abc"` = `ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad`), `blob_hash_parse_rejects_uppercase_and_wrong_length`, `flags_from_bits_rejects_over_31`, `folder_role_roundtrip`.
- [ ] **Step 3: Chạy test, thấy FAIL** — `cargo test -p rusty-core` → lỗi không biên dịch (kiểu chưa có).
- [ ] **Step 4: Implement** các kiểu ở mục Interfaces; ids bằng macro `newtype_id!`; `Error` bằng `thiserror` có `#[source]`.
- [ ] **Step 5: Chạy test, PASS** — `cargo test -p rusty-core`.
- [ ] **Step 6: Commit** `feat(core): ids, blob hash, flags, error`.

### Task 2: Migration và lược đồ ràng buộc

**Files:**
- Create: `crates/rusty-store/Cargo.toml`, `crates/rusty-store/migrations/0001_init.sql`, `src/lib.rs`, `src/sqlite.rs`
- Test: `crates/rusty-store/tests/schema_constraints.rs`

**Interfaces:**
- Consumes: `rusty_core::{Error, Result}`.
- Produces: `SqliteStore::open(url: &str) -> Result<SqliteStore>` (hỗ trợ `sqlite::memory:`; chạy `sqlx::migrate!`, bật FK trên từng kết nối rồi xác nhận bằng `PRAGMA foreign_keys`); `SqliteStore::pool(&self) -> &sqlx::SqlitePool`.

- [ ] **Step 1: Viết test âm** (mỗi cái chạy SQL thô và `assert!(result.is_err())`): `rejects_uppercase_domain`, `rejects_account_with_missing_domain` (FK), `rejects_delete_domain_with_accounts` (RESTRICT), `rejects_list_member_with_both_account_and_external` và `...with_neither`, `rejects_duplicate_folder_name_under_same_parent`, `rejects_duplicate_folder_name_at_root` (index COALESCE), `rejects_two_trash_folders`, `allows_many_custom_folders`, `rejects_folder_parent_equal_self`, `rejects_folder_cycle` (trigger), `rejects_message_referencing_other_mailbox_folder` (FK tổng hợp), `rejects_message_size_zero`, `rejects_flags_over_31`, `rejects_invalid_blob_hash`, `rejects_negative_refcount`, `rejects_delete_folder_with_messages`, `rejects_address_shared_by_account_and_alias`, `..._alias_and_list`, `..._account_and_list` (trigger), `rejects_invalid_json_config`, `rejects_change_log_bad_op`, `open_fails_when_foreign_keys_cannot_be_enabled` (nếu không giả lập được, ghi rõ trong test và kiểm tra `PRAGMA foreign_keys` = 1 trên cả 4 kết nối của pool).
- [ ] **Step 2: Chạy, thấy FAIL** — `cargo test -p rusty-store --test schema_constraints`.
- [ ] **Step 3: Viết `0001_init.sql`** đúng spec §4, thêm trigger: tính duy nhất địa chỉ chéo bảng (3 bảng × insert/update), chống chu trình folder (độ sâu ≤ 64), và `blobs.refcount` (AFTER INSERT/DELETE/UPDATE OF blob_hash trên `messages`). Implement `SqliteStore::open`.
- [ ] **Step 4: Chạy, PASS** toàn bộ test trên.
- [ ] **Step 5: Test refcount** `refcount_follows_message_insert_update_delete` (insert message → 1; đổi `blob_hash` → hai blob cập nhật; xóa → 0); PASS.
- [ ] **Step 6: Commit** `feat(store): schema, constraints, migrations`.

### Task 3: `BlobStore` trên filesystem

**Files:**
- Create: `crates/rusty-store/src/blob.rs`
- Test: `crates/rusty-store/tests/blob_fs.rs`

**Interfaces:**
- Consumes: `BlobHash`, `Error`.
- Produces: `#[async_trait] trait BlobStore { async fn put(&self, bytes: Bytes) -> Result<BlobHash>; async fn get(&self, h: &BlobHash) -> Result<Bytes>; async fn delete(&self, h: &BlobHash) -> Result<()>; }`; `FsBlobStore::new(root: PathBuf)`. Đường dẫn: `root/ab/cd/<hash>`.

- [ ] **Step 1: Test:** `put_is_idempotent_and_content_addressed`, `get_missing_is_not_found`, `delete_missing_is_ok`, `put_writes_via_temp_file_then_rename` (không để lại file `.tmp` sau put; không thấy file một phần nếu put bị hủy — dùng temp file cùng thư mục + `rename`).
- [ ] **Step 2: Chạy, FAIL** — `cargo test -p rusty-store --test blob_fs`.
- [ ] **Step 3: Implement** `FsBlobStore` (tokio::fs, sha2, fsync file trước rename).
- [ ] **Step 4: Chạy, PASS.**
- [ ] **Step 5: Commit** `feat(store): filesystem blob store`.

### Task 4: `rusty-mime`

**Files:**
- Create: `crates/rusty-mime/{Cargo.toml,src/lib.rs}`, `tests/fixtures/{plain.eml,multipart.eml,no_message_id.eml,bad_charset.eml}`
- Test: `crates/rusty-mime/tests/parse.rs`

**Interfaces:**
- Produces: `parse_envelope(raw: &[u8]) -> Result<Envelope>`; `Envelope { subject: String, from: String, to: String, message_id: Option<String>, in_reply_to: Option<String>, references: Vec<String>, date: Option<i64> }` (địa chỉ ở dạng chữ thường, nối bằng `, `). Thư rỗng → `Error::Invalid`; header hỏng vẫn trả giá trị rỗng, không panic.

- [ ] **Step 1: Test:** `plain_message_fields`, `multipart_subject_decoded` (RFC 2047), `missing_message_id_is_none`, `empty_input_is_invalid`, `garbage_bytes_do_not_panic` (proptest với bytes ngẫu nhiên).
- [ ] **Step 2: Chạy, FAIL.**
- [ ] **Step 3: Implement** `parse_envelope` bọc `mail-parser`.
- [ ] **Step 4: Chạy, PASS.**
- [ ] **Step 5: Commit** `feat(mime): envelope parsing`.

### Task 5: `rusty-directory`

**Files:**
- Create: `crates/rusty-directory/{Cargo.toml,src/lib.rs,src/config.rs,src/auth.rs}`
- Test: `crates/rusty-directory/tests/{directory.rs,config.rs,auth.rs}`

**Interfaces:**
- Consumes: `SqliteStore`, ids, `Error`.
- Produces:
  - `Directory::new(store: SqliteStore) -> Directory` với `create_domain(&self, name: &str) -> Result<DomainId>`, `create_plan(&self, name: &str, config: serde_json::Value) -> Result<PlanId>`, `create_account(&self, local: &str, domain: DomainId, password: Option<&str>, plan: Option<PlanId>) -> Result<AccountId>`, `add_alias(&self, local: &str, domain: DomainId, account: AccountId) -> Result<()>`, `create_list(...)`, `add_list_member(...)`, `resolve(&self, addr: &str) -> Result<Resolved>` với `Resolved::{Account(AccountId), List(Vec<Resolved>)}`.
  - `create_account` cũng tạo hàng `mailboxes` (uid_validity ngẫu nhiên > 0) và các folder hệ thống `inbox, sent, drafts, trash, spam` (id tăng từ 1) trong cùng transaction.
  - `effective_config(&self, account: AccountId, key: &str) -> Result<Option<serde_json::Value>>` ưu tiên account > plan > domain > global (global lưu ở bảng `plans` tên đặc biệt `__global__` hoặc file; chọn bảng `plans`).
  - `trait Authenticator { async fn verify(&self, addr: &str, password: &str) -> Result<Option<AccountId>>; }` và `LocalAuthenticator` (argon2id).

- [ ] **Step 1: Test:** `create_account_makes_mailbox_and_system_folders`, `address_is_normalized_to_lowercase`, `empty_local_part_is_invalid`, `alias_cannot_reuse_account_address` (Conflict), `resolve_alias_returns_account`, `resolve_list_expands_members`, `config_precedence_account_over_plan_over_domain_over_global`, `invalid_config_json_rejected`, `verify_correct_password`, `verify_wrong_password_is_none`, `verify_locked_account_is_none`, `verify_account_without_hash_is_none`.
- [ ] **Step 2: Chạy, FAIL.**
- [ ] **Step 3: Implement** theo Interfaces. Chuẩn hóa địa chỉ bằng `to_lowercase()` sau `trim()`; lỗi ràng buộc DB ánh xạ ở một hàm `map_db_error` dùng chung (đặt trong `rusty-store`, export).
- [ ] **Step 4: Chạy, PASS.**
- [ ] **Step 5: Commit** `feat(directory): domains, accounts, aliases, lists, config, auth`.

### Task 6: `Mailbox` — folder, append, flag, move, expunge, change-log

**Files:**
- Create: `crates/rusty-store/src/mailbox.rs`, `src/traits.rs` (khai báo `MetadataStore`/`Tx` theo spec §5)
- Test: `crates/rusty-store/tests/mailbox.rs`, `tests/mailbox_props.rs`

**Interfaces:**
- Consumes: `SqliteStore`, `BlobStore`, `rusty_mime::parse_envelope`, ids.
- Produces: `Mailbox::open(store: SqliteStore, blobs: Arc<dyn BlobStore>, account: AccountId) -> Result<Mailbox>` với
  - `create_folder(&self, parent: Option<FolderId>, name: &str) -> Result<FolderId>`; `rename_folder`, `move_folder(&self, id, new_parent: Option<FolderId>)`, `delete_folder(&self, id)` (từ chối folder hệ thống và folder còn thư).
  - `append(&self, folder: FolderId, raw: Bytes, flags: Flags) -> Result<MessageRef>`; `MessageRef { id: MessageId, uid: u32 }`. Thứ tự: `blobs.put` → transaction (upsert `blobs`, lấy `next_msg_id` và `next_uid`, chèn message, `thread_id` từ `In-Reply-To`/`References` hoặc id mới, ghi `change_log`).
  - `set_flags(&self, id, flags: Flags)`, `add_keyword`/`remove_keyword`, `move_message(&self, id, to: FolderId) -> Result<MessageRef>` (UID mới trong folder đích), `expunge(&self, id)` (đặt `deleted_at`, không xóa blob).
  - `list_folder(&self, folder: FolderId) -> Result<Vec<MessageSummary>>` (bỏ thư đã xóa mềm, sắp theo uid), `changes_since(&self, seq: i64) -> Result<Vec<Change>>`.

- [ ] **Step 1: Test:** `append_assigns_increasing_ids_and_uids`, `append_without_message_id_ok`, `append_empty_is_invalid`, `append_to_foreign_folder_is_not_found`, `move_changes_uid_and_logs_change`, `expunge_hides_message_but_keeps_blob`, `uid_is_never_reused_after_expunge`, `set_flags_logs_update`, `delete_folder_with_messages_conflicts`, `delete_system_folder_is_invalid`, `create_folder_with_slash_is_invalid`, `move_folder_into_descendant_conflicts`, `thread_id_shared_by_reply`, `failed_transaction_leaves_orphan_blob_not_metadata` (chèn lỗi giữa chừng bằng folder không tồn tại; kiểm tra không có hàng `messages`/`change_log`, blob mồ côi vẫn có file).
- [ ] **Step 2: Test thuộc tính (proptest):** `ids_uids_seq_are_gapless_and_monotonic` với chuỗi thao tác ngẫu nhiên; `concurrent_appends_no_duplicates` (50 task tokio song song vào cùng folder: uid là hoán vị của 1..=50, seq liên tục).
- [ ] **Step 3: Chạy, FAIL** — `cargo test -p rusty-store --test mailbox --test mailbox_props`.
- [ ] **Step 4: Implement** `Mailbox`. Transaction SQLite dùng `BEGIN IMMEDIATE` để tuần tự hóa ghi, tránh trùng bộ đếm.
- [ ] **Step 5: Chạy, PASS.**
- [ ] **Step 6: Commit** `feat(store): mailbox facade`.

### Task 7: Dọn dẹp mồ côi và xóa account

**Files:**
- Create: `crates/rusty-store/src/gc.rs`
- Test: `crates/rusty-store/tests/gc.rs`

**Interfaces:**
- Consumes: `SqliteStore`, `BlobStore`.
- Produces: `purge_deleted(store: &SqliteStore, older_than: i64) -> Result<u64>` (xóa thật thư có `deleted_at < older_than`, trả số thư đã xóa); `collect_blobs(store: &SqliteStore, blobs: &dyn BlobStore, grace: Duration) -> Result<u64>` (xóa blob `refcount = 0` quá hạn ân hạn, và file trong thư mục blob không có hàng `blobs` quá hạn ân hạn).

- [ ] **Step 1: Test:** `purge_removes_only_old_deleted_messages`, `collect_keeps_shared_blob_after_one_account_deleted`, `collect_removes_unreferenced_blob_after_grace`, `collect_removes_orphan_file_without_row`, `delete_account_cascades_mailbox_and_decrements_refcount`.
- [ ] **Step 2: Chạy, FAIL.**
- [ ] **Step 3: Implement.** Chỉ xóa file blob sau khi xóa hàng `blobs` thành công.
- [ ] **Step 4: Chạy, PASS.**
- [ ] **Step 5: Commit** `feat(store): garbage collection`.

### Task 8: Kiểm thu tiêu chí hoàn thành

**Files:**
- Create: `crates/rusty-store/tests/acceptance.rs`, `rusty.example.toml`, `README.md` (cách build/test trong WSL)

**Interfaces:**
- Consumes: mọi thứ ở trên. Produces: không có API mới.

- [ ] **Step 1: Viết test** `acceptance_10k_messages`: tạo domain + account, cây folder 3 cấp, append 10.000 thư (mẫu từ fixtures), đổi flag một nửa, move 1.000 thư, expunge 1.000, `changes_since(0)` trả đủ và `seq` liên tục, `list_folder` đúng số lượng; hoàn thành dưới 60 giây trên SQLite file tạm.
- [ ] **Step 2: Chạy** `cargo test --workspace` — mong đợi tất cả PASS.
- [ ] **Step 3: Chạy** `cargo clippy --workspace -- -D warnings` — mong đợi không có cảnh báo; sửa nếu có.
- [ ] **Step 4: Commit** `test: foundation acceptance`.

### Task 9: Nền tảng bảo mật (thêm theo định hướng security-first)

**Files:**
- Modify: `crates/*/src/lib.rs` (thêm `#![forbid(unsafe_code)]`), `crates/rusty-store/src/mailbox.rs` (giới hạn kích thước thư, nếu Task 6 chưa làm)
- Create: `deny.toml`, `fuzz/Cargo.toml`, `fuzz/fuzz_targets/{parse_envelope.rs,normalize_address.rs}`, `scripts/ci.sh`, `SECURITY.md`
- Test: `crates/rusty-store/tests/limits.rs`

**Interfaces:**
- Consumes: `parse_envelope`, `normalize_address`/`parse_address`, `Mailbox::append`.
- Produces: không có API mới ngoài `MailboxLimits { max_message_bytes: usize }` (mặc định 50 MiB) nếu chưa có.

- [ ] **Step 1: Test:** `append_over_limit_is_quota`, `append_exactly_at_limit_ok`, `workspace_forbids_unsafe` (test đọc mọi `lib.rs` và kiểm tra có `#![forbid(unsafe_code)]`).
- [ ] **Step 2: Chạy, FAIL.**
- [ ] **Step 3: Implement** giới hạn kích thước ở `Mailbox::append` (kiểm tra trước khi ghi blob) và thêm `forbid(unsafe_code)` cho mọi crate. Thêm vào `normalize_local` (rusty-directory): từ chối ký tự Unicode nhóm Cf (format, ví dụ U+200B zero-width space, U+202E bidi override) và nhóm Co/Cn; thêm test `rejects_zero_width_and_bidi_override_in_local_part` (có positive control).
- [ ] **Step 4: Thêm** `deny.toml` (cấm nguồn ngoài crates.io, bật advisory, chặn giấy phép không cho phép), `scripts/ci.sh` chạy `cargo test`, `clippy -D warnings`, `cargo deny check`, `cargo audit` và bộ test/build musl (Step 4b); hai fuzz target (`cargo fuzz`) cho `parse_envelope` và `parse_address`, chạy mỗi target tối thiểu 60 giây không crash; `SECURITY.md` mô tả mô hình đe dọa và quy trình báo lỗi.
- [ ] **Step 4b: Thêm kiểm tra musl vào `scripts/ci.sh`:** `cargo test --workspace --target x86_64-unknown-linux-musl` và `cargo build --release --workspace --target x86_64-unknown-linux-musl`; với mỗi binary release (hiện chưa có thì bỏ qua) kiểm tra `file` báo `statically linked`. SQLite bundled biên dịch bằng C nên cần `musl-tools` (`sudo apt install musl-tools`) và `CC_x86_64_unknown_linux_musl=musl-gcc`; ghi cách cài vào `SECURITY.md`/README, không bỏ qua im lặng nếu thiếu. Nếu test nào chỉ fail trên musl, ghi lại nguyên nhân (không tắt test). Giữ glibc làm mặc định cho dev.
- [ ] **Step 5: Chạy** `bash scripts/ci.sh` và fuzz 60 giây mỗi target — mong đợi sạch. Nếu công cụ chưa cài, ghi rõ trong báo cáo cách cài, không bỏ qua im lặng.
- [ ] **Step 6: Commit** `feat(security): unsafe ban, limits, supply-chain checks, fuzz targets`.
