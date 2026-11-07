-- SPDX-FileCopyrightText: 2026 Slavi Pantaleev
--
-- SPDX-License-Identifier: Apache-2.0

CREATE TABLE pr_sources (
    id TEXT PRIMARY KEY,
    project TEXT NOT NULL,
    repository_id TEXT NOT NULL,
    number TEXT NOT NULL,
    data TEXT NOT NULL,
    UNIQUE (project, repository_id, number)
);

CREATE TABLE discovery_scans (
    project TEXT PRIMARY KEY,
    next_scan_at TEXT NOT NULL
);
