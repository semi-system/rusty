# Rusty - ACL và chia sẻ folder (tương đương folder grants của Zimbra)

Ngày: 2026-11-03. Trạng thái: chờ người dùng duyệt spec (sau đó mới lập kế hoạch triển khai chi tiết).

## 1. Mục tiêu và ràng buộc
Cho phép chủ mailbox chia sẻ folder cho người khác (đọc, ghi, quản lý), ủy quyền, và chia sẻ cho cả nhóm, giống `folder grants` của Zimbra, nhưng dùng mô hình quyền chuẩn (**IMAP ACL, RFC 4314**) để IMAP, JMAP và CalDAV/CardDAV sau này cùng dùng một nguồn quyền.

Ràng buộc đã chốt từ các spec trước:
- **Domain = tenant là ranh giới bảo mật cứng.** Chia sẻ chỉ trong cùng tenant ở phiên bản đầu. Chia sẻ liên tenant bị cấm bằng ràng buộc DB; nếu cần sau này sẽ là chính sách tin cậy giữa tenant do `superadmin` đặt, ngoài phạm vi.
- Mọi kiểm tra quyền thực hiện trong `store` (không tin dịch vụ giao thức), theo capability của người gọi.
- Truy cập vào thứ mình không có quyền `lookup` trả `NotFound`, không phải `Forbidden`, để không lộ tồn tại.
- Admin (domain-admin) quản lý được grant trong tenant của mình để hỗ trợ, nhưng **vẫn không đọc được nội dung thư**.

Ngoài phạm vi: chia sẻ liên tenant, chia sẻ công khai không đăng nhập (liên kết công khai), "gửi thay mặt" (send-as, send-on-behalf; xem mục 9), cờ đã đọc theo từng người trên folder dùng chung (mục 5).

## 2. Người được cấp quyền (grantee)
| Loại | Ý nghĩa | Ghi chú |
|---|---|---|
| `account` | một tài khoản cụ thể trong cùng tenant | khóa ngoại tổng hợp ép cùng tenant |
| `list` | mọi thành viên (là tài khoản) của một danh sách phân phối trong cùng tenant | thay đổi thành viên có hiệu lực ngay; thành viên ngoài (`external`) bị bỏ qua |
| `domain` | mọi tài khoản đã đăng nhập của tenant này | dùng cho danh bạ/lịch chung toàn công ty |

Chủ mailbox luôn có đủ quyền trên mailbox của mình, không cần grant.

## 3. Quyền (rights, theo RFC 4314)
Quyền lưu thành bitmask. Mỗi quyền là một bit:

| Bit | Ký hiệu | Ý nghĩa |
|---|---|---|
| `lookup` | `l` | thấy folder trong danh sách |
| `read` | `r` | đọc thư/mục trong folder |
| `seen` | `s` | giữ trạng thái đã đọc (xem mục 5) |
| `write` | `w` | đổi cờ và keyword (trừ đã xóa) |
| `insert` | `i` | thêm thư (kể cả copy/move vào) |
| `post` | `p` | gửi thư tới địa chỉ nhận của folder (dùng cho thư mục công khai nhận thư) |
| `create` | `k` | tạo folder con |
| `delete_folder` | `x` | xóa hoặc đổi tên folder |
| `delete_msg` | `t` | đánh dấu xóa thư |
| `expunge` | `e` | xóa thật (expunge) |
| `admin` | `a` | đọc và đổi ACL của folder |

Mức đặt sẵn (preset) cho giao diện: `viewer` = `lr s`, `contributor` = `lr s i p`, `editor` = `lr s w i p k t e`, `manager` = tất cả trừ `admin`, `admin` = tất cả. Các preset chỉ là tên gọi cho bitmask, không phải trạng thái lưu riêng.

Quy tắc cố định:
- Mọi quyền khác (`r`, `w`, `i`...) chỉ có hiệu lực khi người đó cũng có `lookup`.
- Không có quyền âm (deny): chỉ cộng dồn quyền được cấp (đơn giản, dễ kiểm chứng). Việc chặn kế thừa dùng cờ ở mục 4.

## 4. Mô hình dữ liệu
```sql
CREATE TABLE folder_grants (
  mailbox_id        bigint  NOT NULL,
  folder_id         bigint  NOT NULL,
  domain_id         bigint  NOT NULL,                  -- tenant của mailbox, ép bằng khóa ngoại
  grantee_kind      text    NOT NULL CHECK (grantee_kind IN ('account','list','domain')),
  grantee_account   bigint  NULL,
  grantee_list      bigint  NULL,
  rights            integer NOT NULL CHECK (rights BETWEEN 1 AND 2047),
  granted_by        bigint  NOT NULL,                  -- tài khoản thực hiện
  created_at        bigint  NOT NULL,
  CHECK ((grantee_kind = 'account' AND grantee_account IS NOT NULL AND grantee_list IS NULL)
      OR (grantee_kind = 'list'    AND grantee_list    IS NOT NULL AND grantee_account IS NULL)
      OR (grantee_kind = 'domain'  AND grantee_account IS NULL AND grantee_list IS NULL)),
  FOREIGN KEY (mailbox_id, folder_id) REFERENCES folders(mailbox_id, id) ON DELETE CASCADE,
  FOREIGN KEY (mailbox_id, domain_id) REFERENCES mailboxes(id, domain_id),
  FOREIGN KEY (grantee_account, domain_id) REFERENCES accounts(id, domain_id) ON DELETE CASCADE,
  FOREIGN KEY (grantee_list, domain_id)    REFERENCES lists(id, domain_id)    ON DELETE CASCADE
);
-- một grantee chỉ có một dòng cho mỗi folder
CREATE UNIQUE INDEX folder_grants_uq ON folder_grants
  (mailbox_id, folder_id, grantee_kind, COALESCE(grantee_account, 0), COALESCE(grantee_list, 0));
```
- Cần `UNIQUE (id, domain_id)` trên `mailboxes` và `lists` (như đã đề ra cho `accounts` ở spec tenant) để các khóa ngoại tổng hợp ép **cùng tenant**: không thể cấp quyền cho người ở tenant khác ngay từ DB.
- Các khóa ngoại tổng hợp `(mailbox_id, domain_id)` hợp lệ trong một cụm đơn (single cluster) và trở thành các tham chiếu control plane nếu hệ thống được phân mảnh theo mailbox.
- `folders.inherit_grants boolean NOT NULL DEFAULT true`: folder con **thừa hưởng** grant của tổ tiên (cộng dồn). Đặt `false` thì chỉ dùng grant của chính folder đó (cắt kế thừa).
- Bảng `acl_version(mailbox_id PRIMARY KEY, version bigint)` tăng mỗi khi grant hoặc `inherit_grants` đổi; dùng làm khóa vô hiệu hóa cache.
- Nhật ký bất biến `grant_log(id, mailbox_id, folder_id, actor, action, grantee, old_rights, new_rights, at)` chỉ thêm, trigger chặn sửa/xóa; bổ sung cho `admin_audit` (admin) và dành cho cả thao tác của chủ mailbox.
- Giới hạn: tối đa 200 grant mỗi folder và 5.000 mỗi mailbox (`Quota` nếu vượt).

## 5. Tính quyền hiệu lực
Với người gọi `U` thao tác trên folder `F` của mailbox `M`:
1. Nếu `U` là chủ `M` thì đủ quyền.
2. Ngược lại, quyền = OR của: grant `account` cho `U`; grant `list` cho các danh sách có `U` là thành viên; grant `domain` (nếu `U` đăng nhập hợp lệ trong tenant), trên `F` và, nếu `F.inherit_grants`, trên các tổ tiên liên tiếp có `inherit_grants` (dừng khi gặp folder có `inherit_grants = false`).
3. Kết quả không có `lookup` thì mọi thao tác trả `NotFound`.

Cài đặt: **một truy vấn** (CTE đệ quy đi lên tối đa 64 cấp, nối `folder_grants` với tập danh tính của `U`), không vòng lặp truy vấn. Tập danh tính của `U` (các danh sách chứa `U`) là một truy vấn riêng được cache ngắn hạn theo `acl_version` của tenant/danh sách. Kết quả hiệu lực cache trong Valkey (khóa chứa `acl_version`, mã hóa như các giá trị cache khác); thiếu cache thì tính từ Postgres.

Trạng thái đã đọc (`s`): **phiên bản đầu dùng chung** cờ `\\Seen` của chủ mailbox (người có `s` đổi cờ chung). Cờ đã đọc theo từng người trên folder dùng chung là một bảng riêng (`message_user_state`) làm sau nếu cần.

## 6. Thực thi trong `store`
- Mọi thao tác nhận một **`Actor`** (chủ hoặc người được ủy quyền) do `store` suy ra từ capability của yêu cầu, và `mailbox_id` đích. Mã gọi qua `ScopedMailbox` (bọc `Mailbox`) kiểm tra quyền cần thiết trước khi thực hiện, **trong cùng giao dịch** với thao tác (tránh TOCTOU khi quyền vừa bị thu hồi).
- Bảng quyền cần cho từng thao tác: danh sách folder `l`; liệt kê/đọc thư `r`; đổi cờ `w` (đã đọc cần `s`); thêm/copy vào `i`; tạo folder con `k`; xóa/đổi tên folder `x`; đánh dấu xóa `t`; expunge `e`; xem/đổi ACL `a`; `move` cần `t` ở nguồn và `i` ở đích; `copy` cần `r` ở nguồn và `i` ở đích.
- **Chủ mailbox khóa hoặc tài khoản người được cấp bị khóa:** grant vẫn còn nhưng không dùng được (người gọi phải là tài khoản `active`); mailbox của tài khoản bị khóa không truy cập được qua ủy quyền.
- Thu hồi: xóa grant có hiệu lực ngay (kiểm tra trong giao dịch); phiên đang mở bị từ chối ở lần gọi kế tiếp; cache vô hiệu theo `acl_version`.
- Hạn mức dung lượng tính cho **chủ** mailbox. Thư người được cấp quyền thêm vào folder dùng chung cũng tính vào hạn mức của chủ.
- Chính sách chống leo thang: người có `admin` trên folder chỉ cấp được tối đa quyền mà bản thân mình có (không cấp quyền mình không có), và không đổi được grant của chủ.

## 7. Ảnh hưởng tới các giao thức và dịch vụ
- **IMAP:** hiển thị folder được chia sẻ trong không gian tên "Other Users" (RFC 2342); lệnh `SETACL/GETACL/LISTRIGHTS/MYRIGHTS/DELETEACL` ánh xạ trực tiếp sang bảng quyền (RFC 4314); `LIST` chỉ trả folder có `lookup`.
- **JMAP:** truy cập hộp thư của tài khoản khác qua `accountId` và `Mailbox/get` kèm quyền (`myRights`), theo RFC 8621 và khối chia sẻ của JMAP.
- **Đồng bộ:** người được cấp quyền đọc `change_log` của mailbox chủ **đã lọc** theo các folder họ thấy; tạo/xóa grant sinh bản ghi thay đổi cho người nhận (để client biết folder xuất hiện hay biến mất).
- **Tìm kiếm:** kết quả từ chỉ mục của mailbox chủ phải lọc theo quyền `r` từng folder trước khi trả về; không được rò rỉ số lượng hay trích đoạn của thư không được phép đọc.
- **Lịch, danh bạ (sau này):** dùng cùng bảng `folder_grants` cho folder loại lịch/danh bạ (thêm `folders.kind`), nên CalDAV/CardDAV không cần mô hình quyền riêng.
- **Cache Valkey:** khóa chứa `acl_version`; thay đổi grant luôn tăng version trong cùng giao dịch.

## 8. Quản trị và kiểm toán
- Chủ mailbox quản lý grant của folder mình; người có `admin` quản lý grant của folder đó trong giới hạn mục 6.
- `domain-admin` xem và thu hồi mọi grant trong tenant (để xử lý sự cố, rò rỉ quyền) qua `rustyctl acl ...` và admin console; mọi thao tác ghi `admin_audit` và `grant_log`. Admin **không** tự cấp cho mình quyền đọc thư người khác (việc đó là quy trình riêng, có phê duyệt).
- Báo cáo "ai đang truy cập được folder này" và "folder nào đang được chia sẻ ra ngoài" cho chủ và admin.

## 9. Chưa làm trong phạm vi này
- **Gửi thay mặt:** quyền `send_as` / `send_on_behalf` theo tài khoản (Zimbra có); cần bảng `delegations(account, delegate, kind)` ràng buộc cùng tenant và tích hợp vào submission SMTP. Thiết kế riêng.
- **Liên kết công khai** và **khách bên ngoài** (guest share có mật khẩu của Zimbra).
- Cờ đã đọc theo từng người; chia sẻ liên tenant; quyền âm.

## 10. Kiểm thử
- **Ma trận quyền × thao tác:** mỗi thao tác của `ScopedMailbox` với từng tổ hợp quyền (đủ, thiếu đúng một bit cần thiết) -> thành công hoặc `NotFound`/`Forbidden` đúng quy tắc; test thuộc tính so khớp một mô hình tham chiếu đơn giản (OR các grant dọc đường lên).
- **Kế thừa:** grant ở tổ tiên, cắt bằng `inherit_grants = false`, di chuyển folder sang cây khác thì quyền hiệu lực đổi đúng.
- **Danh sách:** thêm/bớt thành viên có hiệu lực ngay; thành viên ngoài không có quyền.
- **Cùng tenant bằng ràng buộc DB:** test âm cho khóa ngoại tổng hợp (cấp cho tài khoản/danh sách tenant khác bị từ chối).
- **Không lộ tồn tại:** không có `lookup` thì `NotFound` giống hệt folder không tồn tại (cả thông báo và thời gian trong biên hợp lý).
- **Thu hồi và TOCTOU:** thu hồi quyền giữa chừng một thao tác nhiều bước; thao tác kế tiếp bị từ chối; chạy đồng thời (4 kết nối) cấp, thu hồi và thao tác.
- **Chống leo thang:** không cấp quyền mình không có; không sửa grant của chủ; tài khoản bị khóa không dùng được grant.
- **Tìm kiếm và đồng bộ:** kết quả và `change_log` luôn đã lọc theo quyền.
- **Giới hạn:** vượt số grant tối đa -> `Quota`; độ sâu cây > 64 không làm CTE chạy vô hạn.
- **Hiệu năng:** tính quyền hiệu lực là một truy vấn; test khẳng định bằng bộ đếm câu lệnh.

## 11. Phân kỳ triển khai (sau khi duyệt)
1. Migration: `UNIQUE (id, domain_id)` còn thiếu, `folder_grants`, `acl_version`, `grant_log`, `folders.inherit_grants`; test âm.
2. Thư viện tính quyền hiệu lực (một truy vấn) + mô hình tham chiếu + test thuộc tính.
3. `ScopedMailbox` và bảng quyền theo thao tác, kiểm tra trong cùng giao dịch; chuyển `Mailbox` hiện tại thành đường dành riêng cho chủ.
4. Quản lý grant (đặt, đổi, thu hồi, liệt kê) với chống leo thang, nhật ký, cache `acl_version`.
5. Tích hợp vào API `store` (vai trò + actor), `rustyctl acl ...`, lọc `change_log` và tìm kiếm.
6. Ánh xạ IMAP ACL và JMAP sharing khi hai giao thức đó được làm.

## 12. Rủi ro và điểm mở
- ACL là nơi dễ sinh lỗi leo thang đặc quyền nhất: bắt buộc có mô hình tham chiếu, test thuộc tính và review bảo mật riêng.
- Kiểm tra quyền phải ở **mọi** đường đọc (tìm kiếm, đồng bộ, xuất, job chuyển mailbox, cache): dễ sót; cần một danh sách kiểm và test chéo.
- Chia sẻ làm phức tạp hóa việc di chuyển mailbox giữa node và xóa tài khoản (grant của và tới tài khoản đó); đã dùng `ON DELETE CASCADE` và `grant_log` để lại dấu vết.
- Cờ đã đọc dùng chung có thể gây bất ngờ khi nhiều người cùng xem một folder; ghi rõ trong tài liệu và làm sau nếu cần.
