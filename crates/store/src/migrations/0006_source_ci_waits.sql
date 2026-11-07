-- SPDX-FileCopyrightText: 2026 Slavi Pantaleev
--
-- SPDX-License-Identifier: Apache-2.0

CREATE TABLE source_ci_waits (
    task_id       TEXT PRIMARY KEY REFERENCES tasks (id),
    next_check_at TEXT NOT NULL
);
