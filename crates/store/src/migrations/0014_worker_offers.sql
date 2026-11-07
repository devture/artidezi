-- SPDX-FileCopyrightText: 2026 Slavi Pantaleev
--
-- SPDX-License-Identifier: Apache-2.0

ALTER TABLE workers ADD COLUMN harnesses TEXT NOT NULL DEFAULT '[]';
ALTER TABLE workers ADD COLUMN tags TEXT NOT NULL DEFAULT '[]';
