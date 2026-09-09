-- Evidence artifacts become durable canonical content.
--
-- Small redacted artifacts stay inline in `artifact_text`. Anything larger than
-- `storage.blob_threshold_bytes` moves to the content-addressed filesystem blob
-- store: the row then keeps `artifact_blob_digest` plus a bounded head excerpt,
-- and reads resolve the full text through the blob store.
--
-- `artifact_digest` is untouched: it remains the SHA-256 of the artifact bytes
-- exactly as observed, and stays the value carried by EVIDENCE_OBSERVED events.
-- The blob digest addresses the *redacted* bytes actually written to disk, so
-- the two are deliberately independent values.

ALTER TABLE evidence ADD COLUMN artifact_text TEXT;
ALTER TABLE evidence ADD COLUMN artifact_excerpt TEXT;
ALTER TABLE evidence ADD COLUMN artifact_blob_digest TEXT REFERENCES blobs(digest);
ALTER TABLE evidence ADD COLUMN artifact_size_bytes INTEGER;

CREATE INDEX evidence_artifact_blob ON evidence(artifact_blob_digest)
    WHERE artifact_blob_digest IS NOT NULL;
