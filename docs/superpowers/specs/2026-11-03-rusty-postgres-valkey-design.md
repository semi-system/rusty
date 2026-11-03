# Rusty - Postgres (dữ liệu người dùng) + Valkey (cache) + SQLite (cấu hình hệ thống)

Ngày: 2026-11-03. Trạng thái: chờ người dùng duyệt spec (sau đó mới lập kế hoạch triển khai chi tiết).

## 1. Bối cảnh và quyết định
Dự án nền tảng dùng SQLite làm kho metadata duy nhất. SQLite chỉ cho một người ghi trên mỗi file DB, nên một DB chung cho mọi người dùng sẽ nghẽn khi tải ghi tăng (xem benchmark Task 8: ~26 s cho ~17 nghìn giao dịch ghi, một người ghi). Quyết định của người dùng (2026-11-03):

- **Nguồn sự thật: Postgres 18** (phiên bản mục tiêu do người dùng chọn; CI và benchmark chạy trên 18; không phụ thuộc tính năng riêng của 18 trừ khi được ghi rõ ở mục dùng nó) (qua TCP/TLS), sau trait `MetadataStore` đã có.
- **Multi-tenant: domain = tenant** (mục 3c).
- **Cache: Valkey** (nhánh BSD của Redis, tương thích giao thức RESP; người dùng chọn Valkey thay Redis vì giấy phép), chỉ là cache, không bao giờ là nơi lưu chính.
- **Cấu hình hệ thống và mọi dữ liệu không liên quan người dùng nằm trong SQLite** ("system DB", mục 3b) cho dễ quản lý; dữ liệu liên quan người dùng (tài khoản, mailbox, thư, và các bảng có khóa ngoại tới chúng) nằm trong Postgres.
- **Bỏ ý tưởng cache SQLite theo từng user.**
- SQLite vẫn cần cho system DB, nên mã SQLite hiện có được giữ. Backend SQLite cho **dữ liệu người dùng** cũng được giữ cho dev, test và cài đặt nhỏ một máy (giả định, chờ xác nhận); không phát triển tính năng riêng cho nó ngoài việc giữ tương đương hành vi với Postgres.
- Người dùng đã chọn Valkey dù rủi ro bề mặt tấn công đã được nêu; do đó mục 6 (cứng hóa Valkey) là **bắt buộc**, không phải tùy chọn.

Ngoài phạm vi: nhiều node `store` đồng thời (cần thông báo giữa node, xem mục 5.5), replica đọc, sharding, mã hóa DB lúc nghỉ (làm ở tầng đĩa/Postgres), dịch vụ giao thức.

## 2. Kiến trúc
```
imap / smtp / jmap ... ──(Unix socket, role+capability)──► rusty store ──► Postgres  (nguồn sự thật)
                                                              │
                                                              ├──► Valkey (cache, có thể mất bất cứ lúc nào)
                                                              └──► system.db (SQLite: cấu hình hệ thống, lịch sử thay đổi, kiểm toán quản trị)
```
- Chỉ tiến trình `rusty store` giữ thông tin đăng nhập Postgres và Valkey (spec mô hình tiến trình §7b).
- Cache là **cache-aside** trong `store`: đọc thử cache, lỡ thì đọc Postgres rồi nạp cache; ghi xuống Postgres, sau commit thì vô hiệu hóa cache.
- Valkey lỗi, chậm hay trả dữ liệu hỏng: `store` **bỏ qua cache và đọc Postgres**, không bao giờ làm hỏng yêu cầu của người dùng vì cache.

Crate:
```
rusty-store       trait MetadataStore/Tx giữ nguyên; backend SQLite (feature `sqlite`, mặc định)
rusty-store-pg    backend Postgres (feature `postgres`), migrations/postgres/
rusty-cache       trait Cache + ValkeyCache + NoCache + MemoryCache (fake cho test), mã hóa giá trị
rusty-sysdb       system DB (SQLite): kho cài đặt có kiểu, lịch sử, kiểm toán; trait `SystemSettings`
```
`Mailbox` nhận `Arc<dyn Cache>` (mặc định `NoCache`) nên mã nghiệp vụ không đổi.

## 3. Backend Postgres

### 3.1 Ánh xạ lược đồ SQLite -> Postgres
| SQLite (hiện có) | Postgres |
|---|---|
| `INTEGER PRIMARY KEY`, `INTEGER` | `bigint` (id dùng `bigint GENERATED ALWAYS AS IDENTITY`; bộ đếm theo mailbox giữ nguyên là cột) |
| `TEXT` + `CHECK (name = lower(name))` | `text COLLATE "C"` + cùng `CHECK`; so sánh chuỗi xác định, không phụ thuộc locale |
| `config TEXT CHECK (json_valid(config))` | `config jsonb NOT NULL` (kiểu đã đảm bảo hợp lệ) |
| `STRICT` | kiểu cột mặc định của Postgres đã chặt |
| Chỉ mục duy nhất từng phần `folders_role` | giữ nguyên (`WHERE role <> 'custom'`) |
| Chỉ mục trên biểu thức `COALESCE(parent_id,0)` | giữ nguyên |
| FK `ON DELETE NO ACTION` | giữ nguyên (kiểm tra cuối câu lệnh, cùng ngữ nghĩa với SQLite) |
| Trigger `RAISE(ABORT,'conflict:...')` | hàm plpgsql + `RAISE EXCEPTION ... USING ERRCODE`; mã tùy chỉnh `RS001` = Conflict, `RS002` = Invalid; `map_db_error` ánh xạ theo SQLSTATE, không còn dựa vào tiền tố chuỗi |
| Trigger refcount / zero_since (`unixepoch()`) | plpgsql, `extract(epoch from now())::bigint` |
| Trigger chống chu trình folder (CTE đệ quy, sâu <= 64) | plpgsql với `WITH RECURSIVE`, cùng giới hạn |
| `BEGIN IMMEDIATE` + `UPDATE ... RETURNING` | transaction `READ COMMITTED`; bộ đếm lấy bằng `UPDATE mailboxes SET next_msg_id = next_msg_id + 1 WHERE id = $1 RETURNING next_msg_id - 1`, **khóa đúng một hàng mailbox**, nên các mailbox khác nhau ghi song song |
| `PRAGMA foreign_keys=ON` | không cần |

Hai hệ quả:
- Id không còn tuần tự toàn cục như `INTEGER PRIMARY KEY` của SQLite; vẫn gapless theo từng mailbox (bộ đếm trong hàng `mailboxes`).
- Cột thời gian giữ nguyên là số nguyên (unix giây) để hai backend cho kết quả giống nhau.

### 3.2 Vai trò và kết nối
- Ba vai trò DB: `rusty_migrator` (DDL, chỉ dùng khi migrate), `rusty_store` (chỉ DML trên schema `rusty`, không superuser, không DDL, không `CREATE`), `rusty_ro` (đọc, cho báo cáo).
- Xác thực `scram-sha-256`; kết nối **TLS với `verify-full`**; `pg_hba.conf` chỉ cho địa chỉ của máy `store`; mật khẩu đọc từ tệp quyền `0400` thuộc user `rusty-store` (không để trong `rusty.toml`).
- Pool kết nối giới hạn, `statement_timeout` và `idle_in_transaction_session_timeout` đặt theo vai trò.
- Mọi truy vấn dùng tham số ràng buộc, không nối chuỗi.
- Tùy chọn (giai đoạn cứng hóa): **Row-Level Security** theo `mailbox_id` với biến phiên `rusty.mailbox_id`, như lớp phòng thủ thứ hai nếu một lỗi logic trong `store` bỏ sót điều kiện.

### 3.3 Migration và tương đương hành vi
- Hai bộ migration riêng: `migrations/sqlite/` (hiện có) và `migrations/postgres/`.
- **Bộ test dùng chung cho cả hai backend:** các test ràng buộc (hiện ở `schema_constraints.rs`), `mailbox.rs`, `gc.rs`, `acceptance` được viết lại thành hàm nhận một "harness" backend và chạy trên cả hai. Postgres test lấy từ biến môi trường `RUSTY_TEST_PG_URL` (CI khởi động một instance tạm); nếu thiếu biến thì test Postgres **bỏ qua với thông báo rõ**, nhưng `scripts/ci.sh` đặt biến này và coi việc thiếu là lỗi.
- **Test tương đương schema:** so sánh danh mục logic (bảng, cột, kiểu quy đổi, ràng buộc, chỉ mục) giữa hai backend.
- Mọi ràng buộc test âm hiện có phải có phiên bản cho Postgres.

## 3b. Cơ sở dữ liệu hệ thống (SQLite)

### Ranh giới: cái gì ở đâu
Khóa ngoại không bắc qua hai cơ sở dữ liệu khác nhau được. Vì người dùng đã yêu cầu siết quan hệ trong DB, quy tắc phân chia là:

- **Postgres:** mọi bảng có khóa ngoại tới hoặc từ dữ liệu người dùng: `domains`, `plans`, `accounts`, `aliases`, `lists`, `list_members`, `mailboxes`, `folders`, `messages`, `keywords`, `change_log`, `blobs` (đếm tham chiếu gắn chặt với `messages` bằng trigger). `domains` và `plans` ở lại đây vì `accounts` tham chiếu chúng.
- **SQLite (system DB):** dữ liệu **không có khóa ngoại nào tới nó từ Postgres** và không chứa bí mật: cài đặt toàn hệ thống, cài đặt theo dịch vụ, ngưỡng và giới hạn, cờ tính năng, chính sách hàng đợi/relay, ngưỡng bộ lọc thư rác, tham số GC (ví dụ `gc.grace_seconds`), metadata TLS và DKIM (đường dẫn tệp khóa, selector, ngày hết hạn, **không bao giờ chứa khóa riêng**), lịch sử thay đổi cài đặt, nhật ký kiểm toán thao tác quản trị.
- **Chuyển từ nơi cũ:** hàng plan đặc biệt `__global__` (cấu hình toàn cục hiện lưu trong bảng `plans`) chuyển sang system DB; việc dành riêng tên `__global__` được bỏ sau khi chuyển.
- **Cấu hình theo thứ tự ưu tiên** (account > plan > domain > global) vẫn giữ nguyên: `store` ghép giá trị lấy từ Postgres (account, plan, domain) và system DB (global) khi đọc; kết quả đã ghép được cache trong Valkey, khóa cache chứa `settings_version` nên đổi cài đặt toàn cục làm mất hiệu lực cache ngay.
- **Khởi động (bootstrap):** `rustyd` và các thông số cần trước khi `store` chạy (đường dẫn DB, cổng, user dịch vụ) vẫn nằm trong `rusty.toml` (Task 8). `rustyd` **không** đọc system DB, để không phụ thuộc vào `store`.

### Lược đồ (SQLite, STRICT, siết như các DB khác)
```sql
CREATE TABLE settings (
  scope       TEXT    NOT NULL CHECK (scope = 'global' OR scope GLOB 'service:[a-z][a-z0-9-]*'),
  key         TEXT    NOT NULL CHECK (key GLOB '[a-z][a-z0-9_.]*' AND length(key) <= 128),
  value       TEXT    NOT NULL CHECK (json_valid(value) AND length(value) <= 65536),
  version     INTEGER NOT NULL CHECK (version >= 1),
  updated_at  INTEGER NOT NULL,
  updated_by  TEXT    NOT NULL,            -- vai trò/uid + tên, không bao giờ để trống
  PRIMARY KEY (scope, key)
) STRICT;

CREATE TABLE settings_history (
  id          INTEGER PRIMARY KEY,
  scope       TEXT NOT NULL, key TEXT NOT NULL,
  old_value   TEXT, new_value TEXT,         -- NULL khi tạo/xóa
  version     INTEGER NOT NULL, at INTEGER NOT NULL, by TEXT NOT NULL
) STRICT;                                    -- chỉ thêm; trigger chặn UPDATE/DELETE

CREATE TABLE admin_audit (
  id INTEGER PRIMARY KEY, at INTEGER NOT NULL, actor TEXT NOT NULL,
  action TEXT NOT NULL, target TEXT NOT NULL, outcome TEXT NOT NULL CHECK (outcome IN ('ok','denied','error'))
) STRICT;                                    -- chỉ thêm

CREATE TABLE schema_info (id INTEGER PRIMARY KEY CHECK (id = 1), version INTEGER NOT NULL) STRICT;
```
- Trigger tự ghi `settings_history` khi chèn/sửa/xóa `settings`; trigger chặn `UPDATE`/`DELETE` trên `settings_history` và `admin_audit`.
- Biến mỗi cài đặt thành **kiểu có đăng ký trong mã** (`rusty-sysdb::registry`): tên khóa, phạm vi cho phép, kiểu/JSON schema, giới hạn min/max, giá trị mặc định, có thể đổi nóng hay cần khởi động lại. **Khóa lạ bị từ chối khi ghi**; giá trị ngoài giới hạn bị từ chối; đọc trả về kiểu đã kiểm tra. Cách này giữ việc quản lý dễ mà vẫn chặt.
- Giá trị nhạy cảm không được lưu ở đây (kiểm tra bằng danh sách cấm tên khóa kiểu `*password*`, `*secret*`, `*private_key*` khi đăng ký cài đặt, và test).

### Truy cập và an toàn
- Tệp `system.db` (mặc định `/var/lib/rusty/system.db`) do `rusty-store` sở hữu, quyền `0600`, thư mục `0700`; **chỉ `store` mở nó**; dịch vụ khác đọc/ghi cài đặt qua API vai trò của `store` (đọc: mọi dịch vụ đọc cài đặt của chính mình và `global`; ghi: chỉ `admin-api`/`rustyctl`).
- SQLite ở chế độ WAL, một người ghi (tần suất ghi thấp), `busy_timeout` hợp lý. Sao lưu bằng `VACUUM INTO`/sao lưu trực tuyến, nằm trong `rustyctl backup`.
- Lược đồ và ràng buộc kiểm thử bằng test âm như các DB khác (khóa lạ, JSON hỏng, vượt kích thước, sửa/xóa lịch sử đều bị từ chối).

### Quản lý (qua `rustyctl`, thêm vào spec mô hình tiến trình)
```
rustyctl config list [--scope S]        # liệt kê cài đặt (bí mật không có ở đây)
rustyctl config get <scope> <key>
rustyctl config set <scope> <key> <json>   # kiểm tra theo registry, ghi lịch sử, tăng version
rustyctl config history [<scope> <key>]
rustyctl config export > settings.toml     # để quản lý bằng git
rustyctl config import settings.toml       # kiểm tra toàn bộ, áp dụng nguyên tử hoặc không áp dụng gì
rustyctl config reload [dịch-vụ]           # báo dịch vụ nạp lại cài đặt có thể đổi nóng
```
Mọi thay đổi ghi `admin_audit` (ai, làm gì, kết quả).

### Giới hạn đã biết
- System DB nằm trên một máy. Khi có nhiều node `store`, cài đặt phải được phân phối (node chính ghi, node phụ nhận ảnh chụp), hoặc chuyển sang một schema trong Postgres; vì vậy truy cập qua trait `SystemSettings` để đổi backend sau mà không đổi nghiệp vụ. Ngoài phạm vi hiện tại.
- Không thể có khóa ngoại từ Postgres tới system DB: mọi tham chiếu từ dữ liệu người dùng tới cài đặt hệ thống (nếu cần sau này) phải là giá trị tự mô tả, kiểm tra ở tầng ứng dụng; đây là lý do dữ liệu nào cần ràng buộc chặt thì ở lại Postgres.

## 3c. Multi-tenant: domain = tenant

Quyết định của người dùng: **một domain là một tenant**. Mô hình giống Zimbra (nhiều domain trên một hệ thống, mỗi domain có cấu hình, giới hạn và quản trị viên riêng) nhưng ranh giới tenant là **ranh giới bảo mật được thực thi và kiểm thử**, không chỉ là quy ước.

### 3c.1 Thay đổi lược đồ (cả Postgres và SQLite)
- `mailboxes.domain_id bigint NOT NULL REFERENCES domains(id)`: sao chép tenant xuống mailbox để ràng buộc, RLS và truy vấn quota không cần nối nhiều bảng. Trigger giữ `mailboxes.domain_id = accounts.domain_id`, bất biến sau khi tạo.
- `accounts.domain_id` **bất biến** (trigger chặn `UPDATE`). Chuyển tài khoản sang tenant khác là thao tác xuất/nhập có kiểm toán, không phải đổi cột.
- `UNIQUE (id, domain_id)` trên `accounts` để các bảng khác dùng khóa ngoại tổng hợp `(account_id, domain_id)`.
- `plans.domain_id bigint NULL REFERENCES domains(id)`: `NULL` = **plan hệ thống** (chỉ superadmin tạo/sửa, mọi tenant dùng được); khác `NULL` = **plan của tenant** (chỉ tenant đó dùng). Ràng buộc: `accounts.plan_id` phải trỏ plan hệ thống hoặc plan cùng `domain_id` (khóa ngoại tổng hợp hoặc trigger, cùng cách kiểm thử như các ràng buộc khác).
- `domain_admins (account_id, domain_id, PRIMARY KEY (account_id, domain_id), FOREIGN KEY (account_id, domain_id) REFERENCES accounts(id, domain_id))`: quản trị viên ủy quyền chỉ có thể là tài khoản **thuộc chính tenant đó**.
- **Alias và thành viên danh sách không được trỏ sang tenant khác** (phát hiện ở review cuối: hiện `aliases.account_id` và `list_members.account_id` có thể trỏ tới tài khoản của domain khác, nên domain-admin của B có thể tạo `x@B` chuyển thư vào mailbox của A và cho phép đăng nhập A bằng địa chỉ do B kiểm soát):
  - `aliases`: thêm `domain_id` của tài khoản đích vào khóa ngoại tổng hợp `FOREIGN KEY (account_id, target_domain_id) REFERENCES accounts(id, domain_id)` với `target_domain_id = domain_id` (CHECK), tức alias chỉ trỏ tới tài khoản **cùng domain**.
  - `list_members`: thêm `list_domain_id` và khóa ngoại tổng hợp `(account_id, list_domain_id) -> accounts(id, domain_id)`; thành viên ngoài (`external`) vẫn là địa chỉ tự do. Quyết định riêng nếu muốn cho phép danh sách chứa tài khoản của domain khác (mặc định: không).
  - Test âm cho cả hai, và test `LocalAuthenticator::verify` không chấp nhận mật khẩu của A qua địa chỉ thuộc B.
- Bộ đếm sử dụng: `mailbox_usage (mailbox_id PK, bytes, messages)` và `domain_usage (domain_id PK, bytes, messages, accounts)` do trigger cập nhật cùng transaction với `messages`/`accounts`; `CHECK (>= 0)`. Dung lượng tính cho tới khi thư bị xóa thật (thư đã expunge và Thùng rác vẫn tính, giống Zimbra).

### 3c.2 Quota và giới hạn theo tenant
- Khóa cài đặt trong cấu hình plan/domain/account: `quota.mailbox_bytes`, `quota.mailbox_messages`, `quota.domain_bytes`, `quota.domain_accounts`, `limits.max_message_bytes`, `limits.send_per_hour`, `limits.max_folders`, `limits.max_keywords_per_message`.
- **Trần hệ thống:** mỗi khóa có trần trong system DB (ví dụ `caps.limits.max_message_bytes`); giá trị của tenant/plan/account **không được vượt trần**, bị từ chối khi ghi và bị kẹp khi đọc. Tenant không thể tự nâng giới hạn quá mức hệ thống cho phép.
- Thực thi: `Mailbox::append` và các thao tác tạo tài khoản/folder kiểm tra quota trong cùng transaction với việc ghi; vượt thì `Error::Quota`. Không kiểm tra ngoài transaction.

### 3c.3 Phân quyền và quản trị ủy quyền
Vai trò mới trong ma trận quyền của `store` (spec mô hình tiến trình §7b):

| Vai trò | Phạm vi | Được làm | Không được làm |
|---|---|---|---|
| `superadmin` | toàn hệ thống | tạo/xóa domain, plan hệ thống, đặt trần, cài đặt hệ thống, gán `domain-admin` | đọc nội dung thư |
| `domain-admin(D)` | **chỉ domain D** (mang `domain_id` trong capability) | tạo/sửa/khóa tài khoản, alias, danh sách, plan của tenant, cấu hình domain D trong giới hạn trần; xem sử dụng/quota của D | thấy hoặc đoán sự tồn tại của domain khác, đọc nội dung thư, vượt trần |

- Truy cập ngoài phạm vi trả **`NotFound`**, không phải `Forbidden`, để không lộ tenant khác có tồn tại hay không (chống oracle liệt kê).
- Không vai trò quản trị nào đọc được nội dung thư; xem thư của người dùng khi cần hỗ trợ là quy trình riêng có phê duyệt và kiểm toán (ngoài phạm vi).
- Mọi thao tác quản trị ghi `admin_audit` kèm tenant liên quan.

### 3c.4 Cô lập dữ liệu và mã hóa
- **RLS bắt buộc ở giai đoạn cứng hóa:** chính sách theo `domain_id` (bảng có cột này) và theo `mailbox_id` thuộc tenant (bảng khác, qua `mailboxes.domain_id`); phiên đặt `rusty.domain_id` và `rusty.mailbox_id` đầu mỗi transaction. Đây là lớp thứ hai sau điều kiện `WHERE` trong mã.
- **Khóa mã hóa cache theo tenant:** khóa mailbox = HKDF(master, `tenant:{domain_id}`, `mailbox:{id}`); khóa cache chứa `domain_id` trong AAD. Hủy khóa tenant (crypto-shredding) làm mọi giá trị cache của tenant không đọc được.
- **Hàng xóm ồn ào:** giới hạn đồng thời và tốc độ theo tenant tại `store` (số kết nối, số yêu cầu mỗi giây, hàng đợi công bằng giữa tenant); `statement_timeout` theo vai trò; một tenant không thể làm nghẽn tenant khác ở mức tài nguyên cấu hình.
- **Xóa tenant (offboarding):** xóa domain yêu cầu đã xóa hết tài khoản (`RESTRICT`), thao tác ghi kiểm toán, GC dọn blob; xuất dữ liệu tenant là lệnh riêng.

### 3c.4b Đặt tên blob theo tenant (cần quyết định TRƯỚC khi có dữ liệu)
Hiện blob đặt tên theo SHA-256 của nội dung, dùng chung toàn hệ thống. Review cuối chỉ ra hai hệ quả khi domain = tenant: (1) **kênh phụ giữa tenant**: hai tenant gửi cùng nội dung thì dùng chung một blob, nên thời gian `put` (có sẵn hay chưa) cho biết "nội dung này đã tồn tại ở đâu đó"; (2) không thể hủy dữ liệu của một tenant bằng cách hủy khóa (crypto-shredding) nếu blob dùng chung.
Đề xuất (cần người dùng duyệt): **tên blob = HMAC-SHA256 với khóa theo tenant** (`blob_key = HMAC(tenant_key, nội dung)`), lưu dưới tiền tố `t/{domain_id}/...`. Dedupe vẫn hoạt động *trong* một tenant; mất dedupe giữa các tenant (tốn thêm dung lượng khi nhiều tenant nhận cùng tệp đính kèm lớn). `BlobStore::put` nhận thêm tham số tenant/khóa. Đổi cách đặt tên sau khi đã có dữ liệu là di chuyển toàn bộ blob, nên quyết định trước.

### 3c.5 Kiểm thử cô lập chéo tenant (bắt buộc)
- Tạo hai tenant A và B với dữ liệu đầy đủ. Với **mỗi** thao tác của `store`, chạy với quyền của A nhắm vào đối tượng của B: kết quả phải là `NotFound`/rỗng, không thay đổi dữ liệu, không khác biệt quan sát được giữa "không tồn tại" và "thuộc tenant khác" (kể cả thông báo lỗi và mã lỗi).
- Test thuộc tính (proptest): chuỗi thao tác ngẫu nhiên của A không bao giờ làm thay đổi hàm băm trạng thái dữ liệu của B.
- Test âm DB cho từng ràng buộc mới (plan của tenant khác, `domain_admins` sai tenant, `accounts.domain_id` bị đổi, `mailboxes.domain_id` lệch).
- Test quota: vượt quota mailbox/domain trong cùng transaction (hai `append` đồng thời gần ngưỡng không cùng vượt), trần hệ thống không bị vượt, xóa thật thư giảm bộ đếm đúng.
- Test RLS: dùng vai trò `rusty_store` với biến phiên sai không đọc được hàng nào.

### 3c.6 Ghi chú
- Không có "tổ chức" chứa nhiều domain trong phạm vi này; nếu cần sau này sẽ thêm lớp `organizations` phía trên domain (không đổi ranh giới bảo mật hiện tại).
- Cài đặt riêng của tenant nằm trong `domains.config` (Postgres), không trong system DB, vì có khóa ngoại và ranh giới tenant.

## 3d. Chuẩn bị trước dự án tenant/Postgres (từ review cuối nhánh nền tảng)
Các việc cần làm ở "phase 00" hoặc ngay đầu dự án, vì đổi sau khi có dữ liệu hoặc sau khi có client sẽ đắt:
- **Ranh giới backend mức phương thức:** trait cho `Mailbox` và `Directory` (hiện `Directory` gắn cứng SQL SQLite và `Tx` quá mỏng); ánh xạ lỗi theo SQLSTATE; bỏ `pool()` khỏi API công khai; chuyển hàng `__global__` khỏi `plans`.
- **Mẫu khóa đọc-rồi-ghi:** mọi thao tác ghi của `Mailbox` khóa hàng mailbox trước (`SELECT ... FOR UPDATE` hoặc `UPDATE ... RETURNING` đầu tiên). Trên Postgres `READ COMMITTED`, các chỗ đọc-rồi-ghi hiện chỉ an toàn nhờ `BEGIN IMMEDIATE` của SQLite (ví dụ đếm keyword trước khi chèn) sẽ đọc dữ liệu cũ nếu không khóa.
- **Nguồn đồng hồ cho `zero_since`:** trigger SQLite dùng đồng hồ DB, GC dùng đồng hồ máy `store`. Với Postgres ở máy khác, dùng thời điểm do ứng dụng truyền vào (trigger `BEFORE` đặt `NEW.zero_since`) hoặc chấp nhận lệch đồng hồ và ghi rõ trong hạn ân hạn.
- **Nén `change_log`:** thêm `min_retained_seq` mỗi mailbox để trả "cannotCalculateChanges" thay vì tải toàn bộ lịch sử; job nén theo hạn lưu.
- **Truyền luồng cho append/get:** hiện cả thư nằm trong một `Bytes` trong bộ nhớ cho mỗi yêu cầu đang chạy; API `store` cần truyền luồng theo khối (spec mô hình tiến trình §7b).
- **Chế độ journal SQLite và đồng bộ:** hiện dùng mặc định (rollback journal, synchronous FULL, busy timeout 5 s); quyết định WAL + `synchronous` sau khi có benchmark; ghi rõ vì spec mô hình tiến trình từng nhắc WAL.
- **Chuyển `GC` sang bảo vệ bằng DB** khi thêm backend đối tượng (spec triển khai container §7.3).
- **Trigger độ sâu folder** hiện chỉ kiểm tra folder được di chuyển, không phải cả cây con; ứng dụng đã chặn, Postgres phải sao chép quy tắc ở cả hai nơi.

## 4. Cache Valkey: những gì được cache
Chỉ metadata, không bao giờ cache nội dung thư (đã ở file blob) hay hash mật khẩu.

| Loại | Khóa | Giá trị | TTL |
|---|---|---|---|
| Danh sách thư trong folder (trang) | `rs:v1:{mbx}:fl:{folder}:{cursor}` | tóm tắt thư (id, uid, size, ngày, subject, from, flags, thread) | 60 s |
| Tóm tắt một thư | `rs:v1:{mbx}:msg:{id}` | như trên | 300 s |
| Cây folder | `rs:v1:{mbx}:tree` | danh sách folder | 300 s |
| `resolve(address)` | `rs:v1:dir:addr:{hash(addr)}` | kết quả phân giải | 60 s |
| Cấu hình hiệu lực | `rs:v1:dir:cfg:{account}:{key}:{settings_version}` | giá trị JSON đã ghép account/plan/domain/global | 60 s |

TTL là lưới an toàn; tính đúng đắn do cơ chế dưới đây đảm bảo, không phải TTL.

## 5. Nhất quán và vô hiệu hóa

### 5.1 Hợp đồng
1. **Không bao giờ phục vụ dữ liệu cũ hơn lần ghi gần nhất của chính client** (đọc được cái vừa ghi).
2. Dữ liệu trong cache có thể cũ tối đa bằng TTL với client khác, trừ khi vô hiệu hóa sớm hơn.
3. Cache không bao giờ làm sai kết quả ghi hay quyết định phân quyền: kiểm tra quyền luôn dựa vào Postgres/capability, không dựa cache.

### 5.2 Cơ chế `min_seq`
Mỗi giá trị cache mang `seq` của `change_log` tại thời điểm nạp. Mỗi phản hồi ghi trả `seq` mới nhất; client (dịch vụ giao thức, từ trạng thái JMAP/IMAP MODSEQ) gửi lại `min_seq` ở các yêu cầu đọc sau. Giá trị cache có `seq < min_seq` bị bỏ qua và nạp lại từ Postgres. Cơ chế này không cần Lua, không cần giao dịch phía Valkey, và khớp với `change_log` đã có.

### 5.3 Vô hiệu hóa sau commit
Sau khi transaction Postgres commit thành công, `store` xóa các khóa bị ảnh hưởng (`DEL`/pipeline): folder liên quan, tóm tắt thư, cây folder khi đổi folder. Lỗi xóa cache được ghi log và đếm, **không** làm hỏng yêu cầu; TTL và `min_seq` bù lại.

### 5.4 Chống ghi cũ đè mới (stale set)
Việc nạp cache bằng `SET` sau khi đọc Postgres có thể ghi đè một vô hiệu hóa xen giữa. Biện pháp: giá trị luôn mang `seq`; trước khi tin giá trị, đọc `min_seq` của yêu cầu; nạp cache chỉ làm khi `seq` của giá trị >= `seq` hiện biết của mailbox trong bộ nhớ tiến trình `store`. Phần còn lại do TTL giới hạn. (Chọn thiết kế này thay vì dùng Lua/`WATCH` để giữ Valkey ở cấu hình không cần scripting.)

### 5.5 Nhiều node `store` (ngoài phạm vi, chuẩn bị chỗ)
Thông báo vô hiệu hóa giữa các node đi qua Postgres `LISTEN/NOTIFY` hoặc đọc `changes_since`; cache vẫn dùng `seq` nên đúng đắn không phụ thuộc thông báo.

## 6. Cứng hóa Valkey (bắt buộc)
- **Mạng:** chỉ nghe Unix socket (cùng máy) hoặc TLS mutual nếu khác máy; `protected-mode yes`; không mở ra Internet; tường lửa chỉ cho máy `store`.
- **Xác thực và ACL:** user `rusty-cache` với mật khẩu mạnh từ tệp `0400`; ACL chỉ cho các lệnh `GET SET MGET DEL EXPIRE UNLINK PING` trên mẫu khóa `rs:v1:*`; user `default` bị tắt.
- **Tắt tính năng nguy hiểm:** vô hiệu `EVAL`, `EVALSHA`, `FUNCTION`, `SCRIPT`, `MODULE`, `CONFIG`, `DEBUG`, `KEYS`, `FLUSHALL`, `FLUSHDB`, `SLAVEOF/REPLICAOF`, `SHUTDOWN`; không nạp module.
- **Không lưu xuống đĩa:** `save ""`, `appendonly no` (cache có thể mất); nếu cần bền thì để trên volume mã hóa.
- **Giới hạn bộ nhớ:** `maxmemory` + `allkeys-lru`; giới hạn kích thước giá trị (<= 64 KiB) và số khóa mỗi mailbox.
- **Mã hóa giá trị (bắt buộc):** mọi giá trị cache được mã hóa AEAD (XChaCha20-Poly1305 hoặc AES-256-GCM, crate RustCrypto) với khóa suy ra bằng HKDF từ khóa chính theo từng mailbox; tiêu đề giá trị gồm `version`, `key_id`, `seq`, nonce; **khóa mailbox và tên khóa Valkey là dữ liệu liên kết (AAD)** để không thể hoán đổi giá trị giữa các khóa. Khóa chính ở tệp `0400` thuộc `rusty-store`, xoay vòng bằng `key_id`. Hệ quả: kẻ chiếm được Valkey không đọc được metadata và không giả mạo được giá trị.
- **Giải mã thất bại, sai AAD, sai `version`, quá cỡ:** coi là cache miss, đếm sự kiện bảo mật, **xóa khóa**.
- **Phiên bản phần mềm:** theo dõi advisory (CI chạy kiểm tra phiên bản Valkey/Valkey khi có); cập nhật nhanh là một phần quy trình vá zero-day.
- Sandbox của tiến trình `store`: được kết nối mạng chỉ tới cổng Postgres và Valkey (Landlock network khi kernel hỗ trợ), không được mở cổng nghe.

## 7. Giao diện cache (`rusty-cache`)
```rust
#[async_trait] pub trait Cache: Send + Sync {
    async fn get(&self, key: &CacheKey) -> CacheResult<Option<CachedValue>>;   // lỗi -> coi như miss
    async fn put(&self, key: &CacheKey, value: &CachedValue, ttl: Duration);
    async fn invalidate(&self, keys: &[CacheKey]);
}
```
- `CacheKey` dựng bằng hàm kiểu (không ghép chuỗi tùy ý): chứa `mailbox_id`/loại/id, bảo đảm tiền tố `rs:v1:`.
- `ValkeyCache`: crate `redis` (rustls, tokio), pool, timeout ngắn (mặc định 50 ms đọc/ghi), circuit breaker (mở khi lỗi liên tiếp, thử lại nửa mở), không retry vô hạn.
- `NoCache` (mặc định), `MemoryCache` (fake cho test, có chế độ tiêm lỗi/trễ/hỏng giá trị).
- Số liệu: tỉ lệ trúng, miss, lỗi, thời gian; **không** ghi khóa hay giá trị vào log.

## 8. Kiểm thử
- **Tương đương backend:** mọi test hiện có chạy trên cả SQLite và Postgres; test âm ràng buộc trên Postgres; so sánh danh mục schema.
- **Đồng thời trên Postgres:** nhiều mailbox ghi song song không chặn nhau; cùng một mailbox: uid/seq không trùng không lỗ hổng (như test 50 tác vụ hiện có); không deadlock khi hai thao tác khóa hàng theo thứ tự khác nhau (kiểm tra thứ tự khóa cố định).
- **Cache:**
  - cache-aside cơ bản, `min_seq` loại bỏ giá trị cũ, vô hiệu hóa sau commit;
  - stale-set race (mô phỏng bằng `MemoryCache` có trễ);
  - **chaos:** Valkey chết, treo, trả giá trị hỏng/bị đổi AAD/quá cỡ/phiên bản lạ -> yêu cầu vẫn thành công qua Postgres, không panic, ghi nhận sự kiện bảo mật;
  - kẻ giả mạo Valkey hoán đổi giá trị giữa hai mailbox bị phát hiện (AAD);
  - mã hóa: nonce không lặp, xoay khóa, giải mã bằng khóa cũ trong thời gian chuyển tiếp.
- **Multi-tenant:** toàn bộ mục 3c.5.
- **System DB:** test âm cho mọi ràng buộc (khóa lạ theo registry, vượt giới hạn, JSON hỏng, kích thước, scope sai, sửa/xóa lịch sử và kiểm toán); trigger lịch sử ghi đúng; `config import` nguyên tử (một giá trị sai thì không áp dụng gì); danh sách cấm tên khóa nhạy cảm; quyền tệp `0600`; ghép cấu hình giữa Postgres và system DB đúng thứ tự ưu tiên và đổi `settings_version` làm mất hiệu lực cache.
- **Bảo mật cấu hình:** test `scripts/check-valkey-config.sh` (hoặc lệnh `rustyctl check`) xác minh ACL, lệnh bị tắt, không persistence, TLS; fuzz bộ giải mã giá trị cache.
- **Benchmark:** cùng kịch bản trên SQLite và Postgres (1, 4, 16 người ghi; thư nhỏ và 1 MiB), có và không có Valkey cho tải đọc; báo cáo thông lượng và p50/p99. Dùng số liệu này quyết định bước tiếp theo (sharding, replica, hay đủ dùng).

## 9. Rủi ro và điểm mở
- **Giấy phép:** đã chọn **Valkey** (nhánh BSD của Redis, tương thích giao thức RESP, do Linux Foundation bảo trợ) thay vì Redis, vì Redis đã đổi giấy phép (RSALv2/SSPL từ 7.4, rồi thêm tùy chọn AGPLv3 ở bản 8). Mã dùng giao thức RESP chung nên đổi qua lại được. Cần kiểm tra khi triển khai: phiên bản Valkey đang được bảo trì và có advisory bảo mật được theo dõi; một số lệnh/tính năng mới của Redis có thể không có ở Valkey, nhưng spec chỉ dùng các lệnh cơ bản.
- **Bề mặt tấn công:** Valkey là dịch vụ C chứa dữ liệu của mọi người dùng trong bộ nhớ; giảm thiểu bằng mã hóa giá trị, ACL, cô lập mạng; rủi ro còn lại là từ chối dịch vụ cache (đã có fallback).
- **Tenant = domain là quyết định khó đảo ngược:** `domain_id` được nhúng vào mailbox, plan, khóa mã hóa và RLS. Nếu sau này cần "tổ chức" nhiều domain, phải thêm lớp phía trên chứ không đổi nghĩa của tenant.
- **Ba nơi lưu dữ liệu** (Postgres, Valkey, SQLite system DB): mỗi nơi có vai trò riêng và quy tắc ranh giới rõ (mục 3b); sao lưu và khôi phục cần thống nhất giữa Postgres và system DB (cùng thời điểm, `rustyctl backup` làm cả hai).
- **Hai backend:** tăng công bảo trì; giảm bằng bộ test dùng chung và test tương đương schema. Nếu gánh nặng quá lớn, cân nhắc bỏ SQLite sau khi Postgres ổn định (quyết định riêng).
- **Quản lý khóa chính của cache:** hiện là tệp; sau này nên dùng KMS/HSM hoặc `systemd-creds`.
- **Độ trễ:** thêm một vòng TCP tới Postgres; giảm bằng pool, pipeline và cache.
- Chưa quyết: chọn crate Postgres (`sqlx` đã dùng, tiếp tục) và có bật RLS ở giai đoạn đầu hay sau.

## 10. Phân kỳ triển khai (sẽ thành kế hoạch chi tiết sau khi duyệt)
00. **Mô hình tenant** (làm trước hết, cả hai backend): migration cho `mailboxes.domain_id`, bất biến `accounts.domain_id`, `plans.domain_id`, `domain_admins`, bộ đếm sử dụng; quota trong `Mailbox::append`; vai trò `domain-admin`; bộ test cô lập chéo tenant.
0. `rusty-sysdb`: migration SQLite, registry cài đặt có kiểu, trigger lịch sử, trait `SystemSettings`, API `store` cho cài đặt, chuyển `__global__` ra khỏi `plans`, `rustyctl config ...` (phần lệnh làm cùng dự án `rustyctl`).
1. Harness test dùng chung cho hai backend; tách test hiện có thành test dùng chung; ánh xạ lỗi theo SQLSTATE/mã backend.
2. `rusty-store-pg`: migration Postgres, trigger plpgsql, `PgStore` cho `MetadataStore`; chạy toàn bộ test dùng chung trên Postgres.
3. Test tương đương schema và test đồng thời nhiều mailbox; benchmark SQLite với Postgres.
4. `rusty-cache`: trait, `NoCache`, `MemoryCache` (có tiêm lỗi), kiểu khóa, mã hóa giá trị AEAD, fuzz bộ giải mã.
5. Tích hợp cache-aside vào `Mailbox` (đọc danh sách/tóm tắt/cây folder), `min_seq`, vô hiệu hóa sau commit, race stale-set.
6. `ValkeyCache` (rustls, timeout, circuit breaker), cấu hình ACL mẫu, script kiểm tra cấu hình, test chaos.
7. Cứng hóa và vận hành: vai trò DB, TLS verify-full, tệp bí mật, tài liệu triển khai; RLS (tùy chọn); benchmark có cache.
