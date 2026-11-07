-- SPDX-FileCopyrightText: 2026 Slavi Pantaleev
-- SPDX-License-Identifier: Apache-2.0

-- Existing accounts keep UTC. Newly prepared accounts opt into browser detection.
ALTER TABLE ui_users ADD COLUMN timezone TEXT NOT NULL DEFAULT 'UTC';
ALTER TABLE ui_users ADD COLUMN timezone_initialized INTEGER NOT NULL DEFAULT 1;
