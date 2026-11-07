-- SPDX-FileCopyrightText: 2026 Slavi Pantaleev
-- SPDX-License-Identifier: Apache-2.0

CREATE TABLE ui_users (
    id TEXT PRIMARY KEY NOT NULL,
    display_name TEXT NOT NULL,
    password_hash TEXT NOT NULL,
    disabled INTEGER NOT NULL DEFAULT 0,
    instance_admin INTEGER NOT NULL DEFAULT 0,
    grants_json TEXT NOT NULL DEFAULT '{}',
    revision INTEGER NOT NULL DEFAULT 1
);
