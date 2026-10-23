-- An IMAP UID identifies a message only within a UIDVALIDITY generation.
-- Preserve archived emails, event/job identities, and existing raw file paths.
ALTER TABLE emails ADD COLUMN uid_validity BIGINT NOT NULL DEFAULT 0;
UPDATE emails e SET uid_validity = s.uid_validity
FROM mailbox_states s
WHERE e.account_id = s.account_id AND e.mailbox_name = s.mailbox_name;
-- 0 is the legacy unknown-generation sentinel when no mailbox state exists.
ALTER TABLE emails DROP CONSTRAINT emails_account_id_mailbox_name_uid_key;
ALTER TABLE emails ADD CONSTRAINT emails_generation_uid_key
    UNIQUE (account_id, mailbox_name, uid_validity, uid);
