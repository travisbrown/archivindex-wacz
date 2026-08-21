CREATE TABLE IF NOT EXISTS payloads (
    digest_algorithm TEXT NOT NULL,
    digest           BLOB NOT NULL,
    payload_length   INTEGER CHECK (payload_length IS NULL OR payload_length >= 0),
    record_id        TEXT NOT NULL,
    target_uri       TEXT NOT NULL,
    warc_date        TEXT NOT NULL,
    PRIMARY KEY (digest_algorithm, digest)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS resource_state (
    target_uri       TEXT NOT NULL,
    etag             TEXT,
    last_modified    TEXT,
    digest_algorithm TEXT,
    digest           BLOB,
    record_id        TEXT,
    warc_date        TEXT,
    CHECK ((digest_algorithm IS NULL) = (digest IS NULL)),
    PRIMARY KEY (target_uri)
) WITHOUT ROWID;
