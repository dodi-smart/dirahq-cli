-- Stable change-id for captured commits. `git patch-id --stable` survives
-- rebase/amend/cherry-pick (the sha does not), so the cloud can re-anchor a
-- commit whose original sha was rewritten out of the remote. Nullable: older
-- rows and commits with no diff (merges) carry NULL.
ALTER TABLE artifacts ADD COLUMN patch_id TEXT;
