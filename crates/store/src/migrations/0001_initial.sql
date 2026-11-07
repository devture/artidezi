-- SPDX-FileCopyrightText: 2026 Slavi Pantaleev
--
-- SPDX-License-Identifier: Apache-2.0

CREATE TABLE tasks (
    id              TEXT PRIMARY KEY,
    project         TEXT NOT NULL,
    kind            TEXT NOT NULL,
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL,
    created_by      TEXT NOT NULL,
    instructions    TEXT NOT NULL,
    delivery_mode   TEXT NOT NULL,
    agent_profile   TEXT NOT NULL,
    policy_snapshot TEXT NOT NULL,
    policy_digest   TEXT NOT NULL,
    state           TEXT NOT NULL
);
CREATE INDEX tasks_state ON tasks (state, created_at);

CREATE TABLE task_repositories (
    task_id       TEXT NOT NULL REFERENCES tasks (id),
    repository_id TEXT NOT NULL,
    branch        TEXT NOT NULL,
    base_commit   TEXT NOT NULL,
    PRIMARY KEY (task_id, repository_id)
);

CREATE TABLE attempts (
    id                  TEXT PRIMARY KEY,
    task_id             TEXT NOT NULL REFERENCES tasks (id),
    number              INTEGER NOT NULL,
    state               TEXT NOT NULL,
    created_at          TEXT NOT NULL,
    updated_at          TEXT NOT NULL,
    worker_pool         TEXT NOT NULL,
    job_sha256          TEXT NOT NULL,
    job_size            INTEGER NOT NULL,
    human_feedback      TEXT,
    record_sha256       TEXT,
    record_size         INTEGER,
    result_sha256       TEXT,
    result_size         INTEGER,
    outcome             TEXT,
    validation_problems TEXT NOT NULL DEFAULT '[]',
    UNIQUE (task_id, number)
);
CREATE INDEX attempts_queue ON attempts (state, worker_pool, created_at);

CREATE TABLE leases (
    attempt_id  TEXT PRIMARY KEY REFERENCES attempts (id),
    worker_pool TEXT NOT NULL,
    worker_id   TEXT NOT NULL,
    leased_at   TEXT NOT NULL,
    expires_at  TEXT NOT NULL
);

CREATE TABLE merge_requests (
    task_id       TEXT NOT NULL REFERENCES tasks (id),
    repository_id TEXT NOT NULL,
    forge         TEXT NOT NULL,
    number        INTEGER NOT NULL,
    url           TEXT NOT NULL,
    source_branch TEXT NOT NULL,
    head_commit   TEXT NOT NULL,
    state         TEXT NOT NULL,
    updated_at    TEXT NOT NULL,
    PRIMARY KEY (task_id, repository_id)
);

CREATE TABLE events (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    at         TEXT NOT NULL,
    task_id    TEXT,
    attempt_id TEXT,
    actor      TEXT NOT NULL,
    kind       TEXT NOT NULL,
    payload    TEXT NOT NULL
);
CREATE INDEX events_task ON events (task_id, id);
