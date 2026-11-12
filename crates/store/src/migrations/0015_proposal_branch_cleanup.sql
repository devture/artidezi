-- SPDX-FileCopyrightText: 2026 Slavi Pantaleev
-- SPDX-License-Identifier: Apache-2.0

-- Immutable merge receipts; completed rows prevent later observations from re-enqueuing them.
-- No historical backfill: only merges observed after this migration request cleanup.
CREATE TABLE proposal_branch_cleanups (
    task_id TEXT NOT NULL REFERENCES tasks (id),
    repository_id TEXT NOT NULL,
    number INTEGER NOT NULL,
    receipt TEXT NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0,
    next_attempt_at TEXT,
    PRIMARY KEY (task_id, repository_id, number)
);
CREATE INDEX proposal_branch_cleanups_due ON proposal_branch_cleanups (next_attempt_at)
    WHERE next_attempt_at IS NOT NULL;
