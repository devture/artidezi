-- SPDX-FileCopyrightText: 2026 Slavi Pantaleev
-- SPDX-License-Identifier: Apache-2.0

CREATE TABLE ui_sessions (
    token_digest TEXT PRIMARY KEY NOT NULL,
    csrf TEXT NOT NULL,
    credential_scope TEXT NOT NULL,
    expires_at INTEGER NOT NULL
);
