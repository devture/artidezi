-- SPDX-FileCopyrightText: 2026 Slavi Pantaleev
-- SPDX-License-Identifier: Apache-2.0

ALTER TABLE ui_users ADD COLUMN password_change_required INTEGER NOT NULL DEFAULT 0;
ALTER TABLE ui_users ADD COLUMN managed_by_config INTEGER NOT NULL DEFAULT 0;
