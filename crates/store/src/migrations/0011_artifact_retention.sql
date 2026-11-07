-- SPDX-FileCopyrightText: 2026 Slavi Pantaleev
--
-- SPDX-License-Identifier: Apache-2.0

CREATE TABLE artifact_uploads (
    attempt_id TEXT NOT NULL REFERENCES attempts (id),
    sha256 TEXT NOT NULL,
    size INTEGER NOT NULL,
    uploaded_at TEXT NOT NULL,
    PRIMARY KEY (attempt_id, sha256)
);

-- Written before unlinking; an interrupted sweep can safely finish on the next pass.
CREATE TABLE artifact_deletions (
    sha256 TEXT PRIMARY KEY,
    size INTEGER NOT NULL,
    deleted_at TEXT NOT NULL,
    reason TEXT NOT NULL
);

-- Expiry is irreversible even if an operator later edits a terminal task or lengthens retention.
CREATE TABLE task_input_expirations (
    task_id TEXT PRIMARY KEY REFERENCES tasks (id),
    expired_at TEXT NOT NULL
);
