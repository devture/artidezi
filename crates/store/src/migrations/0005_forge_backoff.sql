-- SPDX-FileCopyrightText: 2026 Slavi Pantaleev
-- SPDX-License-Identifier: Apache-2.0

CREATE TABLE forge_backoff (
    forge TEXT PRIMARY KEY NOT NULL,
    retry_after TEXT NOT NULL
);
