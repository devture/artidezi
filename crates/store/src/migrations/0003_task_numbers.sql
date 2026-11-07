-- SPDX-FileCopyrightText: 2026 Slavi Pantaleev
-- SPDX-License-Identifier: Apache-2.0

-- Human references are independent of immutable job/forge identifiers.
CREATE TABLE task_numbers (
    number INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id TEXT NOT NULL UNIQUE REFERENCES tasks(id) ON DELETE CASCADE
);
INSERT INTO task_numbers(task_id) SELECT id FROM tasks ORDER BY created_at, id;
CREATE TRIGGER assign_task_number AFTER INSERT ON tasks BEGIN
    INSERT INTO task_numbers(task_id) VALUES (NEW.id);
END;
