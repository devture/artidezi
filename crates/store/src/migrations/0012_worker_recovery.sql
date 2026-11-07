-- SPDX-FileCopyrightText: 2026 Slavi Pantaleev
--
-- SPDX-License-Identifier: Apache-2.0

CREATE TABLE workers (
    worker_pool TEXT NOT NULL,
    worker_id TEXT NOT NULL,
    last_seen TEXT NOT NULL,
    quarantined INTEGER NOT NULL DEFAULT 0,
    drained INTEGER NOT NULL DEFAULT 0,
    cooldown_until TEXT,
    PRIMARY KEY (worker_pool, worker_id)
);

-- Existing leases provide a minimum known contact time until the next heartbeat/poll.
INSERT INTO workers (worker_pool, worker_id, last_seen)
SELECT worker_pool, worker_id, MAX(leased_at) FROM leases GROUP BY worker_pool, worker_id;

-- Loss and replacement are separate durable steps, so recovery resumes after a crash.
CREATE TABLE lost_attempts (
    attempt_id TEXT PRIMARY KEY REFERENCES attempts (id),
    worker_pool TEXT NOT NULL,
    worker_id TEXT NOT NULL,
    lost_at TEXT NOT NULL,
    replacement_id TEXT UNIQUE REFERENCES attempts (id),
    retry_number INTEGER NOT NULL DEFAULT 0
);
