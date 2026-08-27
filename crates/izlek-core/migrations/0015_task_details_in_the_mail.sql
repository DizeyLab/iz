-- A rule can opt into folding the task's own facts into the mail body,
-- instead of the rule's subject line being the only thing the recipient
-- reads. Off by default, so every existing rule keeps sending exactly what
-- it always sent.
ALTER TABLE mail_rule ADD COLUMN include_task_details INTEGER NOT NULL DEFAULT 0;
