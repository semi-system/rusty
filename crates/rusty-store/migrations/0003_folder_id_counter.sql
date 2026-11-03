-- Folder ids are never reused (JMAP/IMAP id stability).
--
-- mailboxes.next_folder_id: the id the next folder of this mailbox gets. Mailbox::create_folder
-- takes it with UPDATE ... RETURNING inside its write transaction, so ids only grow, also after
-- the folder with the highest id was deleted. rusty-directory seeds it to 6 after the five
-- system folders (ids 1..=5).
ALTER TABLE mailboxes ADD COLUMN next_folder_id INTEGER NOT NULL DEFAULT 1
  CHECK (next_folder_id >= 1);

-- One-time backfill: one past the highest folder id each mailbox has today.
UPDATE mailboxes SET next_folder_id =
  COALESCE((SELECT MAX(id) FROM folders f WHERE f.mailbox_id = mailboxes.id), 0) + 1;
