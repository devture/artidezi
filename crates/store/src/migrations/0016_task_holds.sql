-- SPDX-FileCopyrightText: 2026 Slavi Pantaleev
-- SPDX-License-Identifier: Apache-2.0

-- Holds of tasks without an incoming change. An incoming change carries its hold in pr_sources.
CREATE TABLE task_holds (
    task_id TEXT PRIMARY KEY REFERENCES tasks (id),
    data TEXT NOT NULL
);
