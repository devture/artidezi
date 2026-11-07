-- SPDX-FileCopyrightText: 2026 Slavi Pantaleev
--
-- SPDX-License-Identifier: Apache-2.0

ALTER TABLE attempts ADD COLUMN not_before TEXT;
CREATE TABLE transient_failures (
    attempt_id TEXT PRIMARY KEY REFERENCES attempts (id),
    reason TEXT NOT NULL,
    replacement_id TEXT UNIQUE REFERENCES attempts (id),
    retry_number INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE instance_runtime (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    discovery_paused INTEGER NOT NULL DEFAULT 0
);
INSERT INTO instance_runtime (singleton) VALUES (1);
