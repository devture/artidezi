// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

use std::path::Path;
use std::str::FromStr;

use artidezi_config::model::DeliveryMode;
use artidezi_contract::common::{ArtifactRef, Outcome};
use chrono::{DateTime, Duration, Utc};
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::model::*;
use crate::{Result, StoreError};

#[path = "sessions.rs"]
pub(super) mod sessions;
#[path = "ui_users.rs"]
pub(super) mod ui_users;
#[path = "workers.rs"]
mod workers;

const MIGRATIONS: &[&str] = &[
    include_str!("../migrations/0001_initial.sql"),
    include_str!("../migrations/0002_pr_discovery.sql"),
    include_str!("../migrations/0003_task_numbers.sql"),
    include_str!("../migrations/0004_ui_sessions.sql"),
    include_str!("../migrations/0005_forge_backoff.sql"),
    include_str!("../migrations/0006_source_ci_waits.sql"),
    include_str!("../migrations/0007_ui_users.sql"),
    include_str!("../migrations/0008_ui_accounts.sql"),
    include_str!("../migrations/0009_ui_initial_password.sql"),
    include_str!("../migrations/0010_ui_timezone.sql"),
    include_str!("../migrations/0011_artifact_retention.sql"),
    include_str!("../migrations/0012_worker_recovery.sql"),
    include_str!("../migrations/0013_transient_recovery.sql"),
    include_str!("../migrations/0014_worker_offers.sql"),
];

/// Brings the schema up to date in one transaction. `user_version` counts the applied migrations.
fn migrate(conn: &mut Connection) -> Result<()> {
    let known = MIGRATIONS.len() as i64;
    let found: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if found > known {
        return Err(StoreError::SchemaTooNew { found, known });
    }
    let tx = conn.transaction()?;
    for sql in &MIGRATIONS[found as usize..] {
        tx.execute_batch(sql)?;
    }
    tx.pragma_update(None, "user_version", known)?;
    tx.commit()?;
    Ok(())
}

/// Unsynchronized implementation. Only accessed through a Db handle holding its connection lock.
pub(super) struct ConnectionDb {
    conn: Connection,
}

fn ts(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}

fn parse_ts(s: &str) -> rusqlite::Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .map(|t| t.with_timezone(&Utc))
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })
}

fn enum_to_str<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .expect("unit enum serializes to a string")
}

fn enum_from_str<T: serde::de::DeserializeOwned>(s: &str) -> rusqlite::Result<T> {
    serde_json::from_value(serde_json::Value::String(s.to_owned())).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })
}

fn artifact_columns(sha: Option<String>, size: Option<i64>) -> Option<ArtifactRef> {
    match (sha, size) {
        (Some(sha256), Some(size)) => Some(ArtifactRef {
            sha256,
            size: size as u64,
        }),
        _ => None,
    }
}

impl ConnectionDb {
    pub fn task_inputs_expired(&self, task_id: &str) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM task_input_expirations WHERE task_id = ?1)",
            [task_id],
            |row| row.get(0),
        )?)
    }

    pub fn expire_task_inputs(&self, task_id: &str, now: DateTime<Utc>) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO task_input_expirations (task_id, expired_at) VALUES (?1, ?2)",
            params![task_id, ts(now)],
        )?;
        Ok(())
    }
    pub fn retention_metadata(&self) -> Result<Vec<serde_json::Value>> {
        let mut stmt = self
            .conn
            .prepare("SELECT payload FROM events UNION ALL SELECT data FROM pr_sources")?;
        stmt.query_map([], |row| row.get::<_, String>(0))?
            .map(|row| Ok(serde_json::from_str(&row?)?))
            .collect()
    }

    pub fn pin_artifact_upload(
        &self,
        attempt_id: &str,
        artifact: &ArtifactRef,
        now: DateTime<Utc>,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO artifact_uploads (attempt_id, sha256, size, uploaded_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (attempt_id, sha256) DO UPDATE SET uploaded_at = excluded.uploaded_at",
            params![attempt_id, artifact.sha256, artifact.size as i64, ts(now)],
        )?;
        Ok(())
    }

    /// Pins an upload unless it would take the attempt above `max_bytes`, as one statement so
    /// that concurrent uploads can't both pass. Returns whether it was pinned.
    pub fn pin_artifact_upload_within(
        &self,
        attempt_id: &str,
        artifact: &ArtifactRef,
        max_bytes: u64,
        now: DateTime<Utc>,
    ) -> Result<bool> {
        let changed = self.conn.execute(
            "INSERT INTO artifact_uploads (attempt_id, sha256, size, uploaded_at)
             SELECT ?1, ?2, ?3, ?4
             WHERE (SELECT COALESCE(SUM(size), 0) FROM artifact_uploads
                    WHERE attempt_id = ?1 AND sha256 != ?2) + ?3 <= ?5
             ON CONFLICT (attempt_id, sha256) DO UPDATE SET uploaded_at = excluded.uploaded_at",
            params![
                attempt_id,
                artifact.sha256,
                artifact.size as i64,
                ts(now),
                max_bytes.min(i64::MAX as u64) as i64
            ],
        )?;
        Ok(changed == 1)
    }

    pub fn artifact_uploads(&self) -> Result<Vec<(String, ArtifactRef, DateTime<Utc>)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT attempt_id, sha256, size, uploaded_at FROM artifact_uploads")?;
        Ok(stmt
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    ArtifactRef {
                        sha256: row.get(1)?,
                        size: row.get::<_, i64>(2)? as u64,
                    },
                    parse_ts(&row.get::<_, String>(3)?)?,
                ))
            })?
            .collect::<std::result::Result<_, _>>()?)
    }

    pub fn artifact_was_deleted(&self, sha256: &str) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM artifact_deletions WHERE sha256 = ?1)",
            [sha256],
            |row| row.get(0),
        )?)
    }

    pub fn record_artifact_deletion(
        &self,
        artifact: &ArtifactRef,
        now: DateTime<Utc>,
        reason: &str,
    ) -> Result<()> {
        // NORMAL WAL commits may roll back after power loss. Unlinking is irreversible, so
        // this receipt (and preceding task-expiry records) must reach stable storage first.
        // Db holds its connection lock throughout; other operations never observe this mode.
        let previous_sync: i64 = self
            .conn
            .pragma_query_value(None, "synchronous", |row| row.get(0))?;
        self.conn.pragma_update(None, "synchronous", "FULL")?;
        let write_result = self.conn.execute(
            "INSERT INTO artifact_deletions (sha256, size, deleted_at, reason) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (sha256) DO UPDATE SET size = excluded.size, deleted_at = excluded.deleted_at, reason = excluded.reason",
            params![artifact.sha256, artifact.size as i64, ts(now), reason],
        );
        let restore_result = self.conn.pragma_update(None, "synchronous", previous_sync);
        write_result?;
        restore_result?;
        Ok(())
    }

    /// Writes a consistent copy of the database to `dest`, replacing it atomically, so that a file
    /// backup taken while the orchestrator runs always contains one intact database.
    pub fn snapshot_to(&self, dest: &Path) -> Result<()> {
        let io = |path: &Path, source: std::io::Error| StoreError::Io {
            path: path.display().to_string(),
            source,
        };
        let dir = dest.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(dir).map_err(|e| io(dir, e))?;
        let mut partial = dest.as_os_str().to_owned();
        partial.push(".partial");
        let partial = std::path::PathBuf::from(partial);
        match std::fs::remove_file(&partial) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(io(&partial, e)),
            _ => {}
        }
        let target = partial
            .to_str()
            .ok_or_else(|| io(&partial, std::io::Error::other("path is not UTF-8")))?;
        self.conn.execute("VACUUM INTO ?1", [target])?;
        std::fs::File::open(&partial)
            .and_then(|file| file.sync_all())
            .map_err(|e| io(&partial, e))?;
        std::fs::rename(&partial, dest).map_err(|e| io(dest, e))?;
        std::fs::File::open(dir)
            .and_then(|file| file.sync_all())
            .map_err(|e| io(dir, e))
    }

    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(mut conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        migrate(&mut conn)?;
        Ok(Self { conn })
    }

    // ----- tasks -------------------------------------------------------------------------------

    pub fn list_pull_request_sources(
        &self,
        project: Option<&str>,
    ) -> Result<Vec<TrackedPullRequest>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT data FROM pr_sources WHERE (?1 IS NULL OR project = ?1) ORDER BY id",
        )?;
        let rows = stmt.query_map([project], |row| row.get::<_, String>(0))?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }

    pub fn get_pull_request_source(&self, id: &str) -> Result<TrackedPullRequest> {
        let data: String = self
            .conn
            .query_row("SELECT data FROM pr_sources WHERE id = ?1", [id], |row| {
                row.get(0)
            })
            .optional()?
            .ok_or_else(|| StoreError::NotFound {
                entity: "PR source",
                id: id.to_owned(),
            })?;
        Ok(serde_json::from_str(&data)?)
    }

    fn save_source_in(conn: &Connection, source: &TrackedPullRequest) -> Result<()> {
        conn.execute(
            "INSERT INTO pr_sources (id, project, repository_id, number, data) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (id) DO UPDATE SET data = excluded.data",
            params![source.id, source.project, source.repository_id, source.number.to_string(), serde_json::to_string(source)?],
        )?;
        Ok(())
    }

    pub fn save_pull_request_source(&self, source: &TrackedPullRequest) -> Result<()> {
        Self::save_source_in(&self.conn, source)
    }

    /// Records `assessment.superseded`, closes the task and saves its source's new schedule in one
    /// transaction, so a crash cannot leave a paused source behind a terminal task.
    pub fn supersede_source_task(
        &mut self,
        task_id: &str,
        attempt_id: &str,
        actor: &str,
        payload: serde_json::Value,
        source: &TrackedPullRequest,
    ) -> Result<()> {
        let task = self.get_task(task_id)?;
        if !task.state.can_transition_to(TaskState::Closed) {
            return Err(StoreError::InvalidTransition {
                entity: "task",
                id: task_id.into(),
                from: task.state.as_str().into(),
                to: TaskState::Closed.as_str().into(),
            });
        }
        let tx = self.conn.transaction()?;
        Self::insert_event(
            &tx,
            Some(task_id),
            Some(attempt_id),
            actor,
            "assessment.superseded",
            payload,
        )?;
        tx.execute(
            "UPDATE tasks SET state = ?2, updated_at = ?3 WHERE id = ?1",
            params![task_id, TaskState::Closed.as_str(), ts(Utc::now())],
        )?;
        tx.execute("DELETE FROM source_ci_waits WHERE task_id = ?1", [task_id])?;
        Self::insert_event(
            &tx,
            Some(task_id),
            None,
            actor,
            "task.state_changed",
            serde_json::json!({"from": task.state, "to": TaskState::Closed}),
        )?;
        Self::save_source_in(&tx, source)?;
        tx.commit()?;
        Ok(())
    }

    /// Queue and claim the source revision atomically. A crash cannot leave an untracked queued job.
    pub fn insert_discovery_task(
        &mut self,
        task: &Task,
        attempt: &Attempt,
        source: &TrackedPullRequest,
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        Self::insert_task_in(&tx, task)?;
        Self::insert_attempt_in(&tx, attempt)?;
        Self::save_source_in(&tx, source)?;
        tx.commit()?;
        Ok(())
    }

    pub fn defer_forge(&self, forge: &str, retry_after: DateTime<Utc>) -> Result<()> {
        self.conn.execute("INSERT INTO forge_backoff (forge, retry_after) VALUES (?1, ?2)
            ON CONFLICT (forge) DO UPDATE SET retry_after = MAX(forge_backoff.retry_after, excluded.retry_after)",
            params![forge, ts(retry_after)])?;
        Ok(())
    }

    pub fn forge_retry_after(&self, forge: &str) -> Result<Option<DateTime<Utc>>> {
        let value: Option<String> = self
            .conn
            .query_row(
                "SELECT retry_after FROM forge_backoff WHERE forge = ?1",
                [forge],
                |r| r.get(0),
            )
            .optional()?;
        Ok(value.as_deref().map(parse_ts).transpose()?)
    }

    /// Persist scheduling before contacting a forge, so failures do not cause a tight restart loop.
    pub fn discovery_scan_due(
        &mut self,
        project: &str,
        interval_seconds: u32,
        now: DateTime<Utc>,
        force: bool,
    ) -> Result<bool> {
        let next: Option<String> = self
            .conn
            .query_row(
                "SELECT next_scan_at FROM discovery_scans WHERE project = ?1",
                [project],
                |row| row.get(0),
            )
            .optional()?;
        if !force
            && next
                .as_deref()
                .map(parse_ts)
                .transpose()?
                .is_some_and(|next| next > now)
        {
            return Ok(false);
        }
        self.conn.execute(
            "INSERT INTO discovery_scans (project, next_scan_at) VALUES (?1, ?2)
             ON CONFLICT (project) DO UPDATE SET next_scan_at = excluded.next_scan_at",
            params![
                project,
                ts(now + Duration::seconds(i64::from(interval_seconds)))
            ],
        )?;
        Ok(true)
    }

    pub fn insert_task(&mut self, task: &Task) -> Result<()> {
        let tx = self.conn.transaction()?;
        Self::insert_task_in(&tx, task)?;
        tx.commit()?;
        Ok(())
    }

    fn insert_task_in(conn: &Connection, task: &Task) -> Result<()> {
        conn.execute(
            "INSERT INTO tasks (id, project, kind, created_at, updated_at, created_by, instructions,
                                delivery_mode, agent_profile, policy_snapshot, policy_digest, state)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                task.id,
                task.project,
                enum_to_str(&task.kind),
                ts(task.created_at),
                ts(task.updated_at),
                task.created_by,
                task.instructions,
                enum_to_str(&task.delivery_mode),
                task.agent_profile,
                task.policy_snapshot,
                task.policy_digest,
                task.state.as_str(),
            ],
        )?;
        for repo in &task.repositories {
            conn.execute(
                "INSERT INTO task_repositories (task_id, repository_id, branch, base_commit)
                 VALUES (?1, ?2, ?3, ?4)",
                params![task.id, repo.repository_id, repo.branch, repo.base_commit],
            )?;
        }
        Self::insert_event(
            conn,
            Some(&task.id),
            None,
            &task.created_by,
            "task.created",
            serde_json::json!({"project": task.project, "kind": task.kind, "delivery_mode": task.delivery_mode}),
        )?;
        Ok(())
    }

    fn task_from_row(row: &Row<'_>) -> rusqlite::Result<Task> {
        Ok(Task {
            id: row.get("id")?,
            number: row.get("number")?,
            project: row.get("project")?,
            kind: enum_from_str(&row.get::<_, String>("kind")?)?,
            created_at: parse_ts(&row.get::<_, String>("created_at")?)?,
            updated_at: parse_ts(&row.get::<_, String>("updated_at")?)?,
            created_by: row.get("created_by")?,
            instructions: row.get("instructions")?,
            delivery_mode: enum_from_str::<DeliveryMode>(&row.get::<_, String>("delivery_mode")?)?,
            agent_profile: row.get("agent_profile")?,
            policy_snapshot: row.get("policy_snapshot")?,
            policy_digest: row.get("policy_digest")?,
            state: enum_from_str(&row.get::<_, String>("state")?)?,
            repositories: vec![],
        })
    }

    fn load_task_repositories(&self, task: &mut Task) -> Result<()> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT repository_id, branch, base_commit FROM task_repositories
             WHERE task_id = ?1 ORDER BY repository_id",
        )?;
        task.repositories = stmt
            .query_map([&task.id], |row| {
                Ok(TaskRepository {
                    repository_id: row.get(0)?,
                    branch: row.get(1)?,
                    base_commit: row.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(())
    }

    pub fn get_task(&self, id: &str) -> Result<Task> {
        let task = self
            .conn
            .query_row(
                "SELECT tasks.*, task_numbers.number FROM tasks JOIN task_numbers ON task_numbers.task_id = tasks.id WHERE tasks.id = ?1",
                [id],
                Self::task_from_row,
            )
            .optional()?;
        let mut task = task.ok_or_else(|| StoreError::NotFound {
            entity: "task",
            id: id.to_owned(),
        })?;
        self.load_task_repositories(&mut task)?;
        Ok(task)
    }

    /// Resolves a human task number without changing the internal task identity.
    pub fn get_task_by_number(&self, number: i64) -> Result<Task> {
        let id: Option<String> = self
            .conn
            .query_row(
                "SELECT task_id FROM task_numbers WHERE number = ?1",
                [number],
                |row| row.get(0),
            )
            .optional()?;
        self.get_task(&id.ok_or_else(|| StoreError::NotFound {
            entity: "task",
            id: number.to_string(),
        })?)
    }

    pub fn list_tasks(&self, state: Option<TaskState>) -> Result<Vec<Task>> {
        let mut tasks: Vec<Task> = match state {
            Some(state) => {
                let mut stmt = self.conn.prepare_cached(
                    "SELECT tasks.*, task_numbers.number FROM tasks JOIN task_numbers ON task_numbers.task_id = tasks.id WHERE state = ?1 ORDER BY number DESC",
                )?;
                let rows = stmt.query_map([state.as_str()], Self::task_from_row)?;
                rows.collect::<rusqlite::Result<_>>()?
            }
            None => {
                let mut stmt = self
                    .conn
                    .prepare_cached("SELECT tasks.*, task_numbers.number FROM tasks JOIN task_numbers ON task_numbers.task_id = tasks.id ORDER BY number DESC")?;
                let rows = stmt.query_map([], Self::task_from_row)?;
                rows.collect::<rusqlite::Result<_>>()?
            }
        };
        for task in &mut tasks {
            self.load_task_repositories(task)?;
        }
        Ok(tasks)
    }

    pub fn set_task_state(&mut self, id: &str, next: TaskState, actor: &str) -> Result<()> {
        let task = self.get_task(id)?;
        if !task.state.can_transition_to(next) {
            return Err(StoreError::InvalidTransition {
                entity: "task",
                id: id.to_owned(),
                from: task.state.as_str().to_owned(),
                to: next.as_str().to_owned(),
            });
        }
        let tx = self.conn.transaction()?;
        tx.execute(
            "UPDATE tasks SET state = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, next.as_str(), ts(Utc::now())],
        )?;
        if next != TaskState::WaitingForCi {
            tx.execute("DELETE FROM source_ci_waits WHERE task_id = ?1", [id])?;
        }
        Self::insert_event(
            &tx,
            Some(id),
            None,
            actor,
            "task.state_changed",
            serde_json::json!({"from": task.state, "to": next}),
        )?;
        tx.commit()?;
        Ok(())
    }

    // ----- attempts ----------------------------------------------------------------------------

    /// Persist the state and its polling deadline together, including across process restarts.
    pub fn wait_for_source_ci(
        &mut self,
        task_id: &str,
        next_check_at: DateTime<Utc>,
    ) -> Result<()> {
        let task = self.get_task(task_id)?;
        if !task.state.can_transition_to(TaskState::WaitingForCi) {
            return Err(StoreError::InvalidTransition {
                entity: "task",
                id: task_id.into(),
                from: task.state.as_str().into(),
                to: TaskState::WaitingForCi.as_str().into(),
            });
        }
        let tx = self.conn.transaction()?;
        tx.execute(
            "UPDATE tasks SET state = 'waiting_for_ci', updated_at = ?2 WHERE id = ?1",
            params![task_id, ts(Utc::now())],
        )?;
        tx.execute(
            "INSERT INTO source_ci_waits (task_id, next_check_at) VALUES (?1, ?2)
             ON CONFLICT(task_id) DO UPDATE SET next_check_at = excluded.next_check_at",
            params![task_id, ts(next_check_at)],
        )?;
        Self::insert_event(
            &tx,
            Some(task_id),
            None,
            "orchestrator",
            "task.state_changed",
            serde_json::json!({"from": task.state, "to": TaskState::WaitingForCi}),
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Reserve the next check before contacting the forge; repeated ticks/restarts cannot hot-loop.
    pub fn claim_source_ci_check(
        &mut self,
        task_id: &str,
        now: DateTime<Utc>,
        next_check_at: DateTime<Utc>,
    ) -> Result<bool> {
        self.claim_source_check_in(task_id, TaskState::WaitingForCi, now, next_check_at)
    }

    /// Records a recheckable hold, its `needs_human` state and its recheck deadline in one
    /// transaction, so a crash can never leave a marked hold without a scheduled check.
    pub fn hold_for_source_check(
        &mut self,
        task_id: &str,
        attempt_id: &str,
        actor: &str,
        hold: serde_json::Value,
        next_check_at: DateTime<Utc>,
    ) -> Result<()> {
        let task = self.get_task(task_id)?;
        if task.state != TaskState::NeedsHuman
            && !task.state.can_transition_to(TaskState::NeedsHuman)
        {
            return Err(StoreError::InvalidTransition {
                entity: "task",
                id: task_id.into(),
                from: task.state.as_str().into(),
                to: TaskState::NeedsHuman.as_str().into(),
            });
        }
        let tx = self.conn.transaction()?;
        Self::insert_event(
            &tx,
            Some(task_id),
            Some(attempt_id),
            actor,
            "task.needs_human",
            hold,
        )?;
        if task.state != TaskState::NeedsHuman {
            tx.execute(
                "UPDATE tasks SET state = 'needs_human', updated_at = ?2 WHERE id = ?1",
                params![task_id, ts(Utc::now())],
            )?;
            Self::insert_event(
                &tx,
                Some(task_id),
                None,
                actor,
                "task.state_changed",
                serde_json::json!({"from": task.state, "to": TaskState::NeedsHuman}),
            )?;
        }
        tx.execute(
            "INSERT INTO source_ci_waits (task_id, next_check_at) VALUES (?1, ?2)
             ON CONFLICT(task_id) DO UPDATE SET next_check_at = excluded.next_check_at",
            params![task_id, ts(next_check_at)],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn claim_held_source_check(
        &mut self,
        task_id: &str,
        now: DateTime<Utc>,
        next_check_at: DateTime<Utc>,
    ) -> Result<bool> {
        self.claim_source_check_in(task_id, TaskState::NeedsHuman, now, next_check_at)
    }

    fn claim_source_check_in(
        &mut self,
        task_id: &str,
        state: TaskState,
        now: DateTime<Utc>,
        next_check_at: DateTime<Utc>,
    ) -> Result<bool> {
        Ok(self.conn.execute(
            "UPDATE source_ci_waits SET next_check_at = ?3 WHERE task_id = ?1
             AND next_check_at <= ?2
             AND EXISTS (SELECT 1 FROM tasks WHERE id = ?1 AND state = ?4)",
            params![task_id, ts(now), ts(next_check_at), state.as_str()],
        )? == 1)
    }

    pub fn insert_attempt(&mut self, attempt: &Attempt) -> Result<()> {
        let tx = self.conn.transaction()?;
        Self::insert_attempt_in(&tx, attempt)?;
        tx.commit()?;
        Ok(())
    }

    fn insert_attempt_in(conn: &Connection, attempt: &Attempt) -> Result<()> {
        conn.execute(
            "INSERT INTO attempts (id, task_id, number, state, created_at, updated_at, worker_pool,
                                   job_sha256, job_size, human_feedback, not_before)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                attempt.id,
                attempt.task_id,
                attempt.number,
                attempt.state.as_str(),
                ts(attempt.created_at),
                ts(attempt.updated_at),
                attempt.worker_pool,
                attempt.job.sha256,
                attempt.job.size as i64,
                attempt.human_feedback,
                attempt.not_before.map(ts),
            ],
        )?;
        Self::insert_event(
            conn,
            Some(&attempt.task_id),
            Some(&attempt.id),
            "orchestrator",
            "attempt.created",
            serde_json::json!({"number": attempt.number, "worker_pool": attempt.worker_pool}),
        )?;
        Ok(())
    }

    fn attempt_from_row(row: &Row<'_>) -> rusqlite::Result<Attempt> {
        let problems: String = row.get("validation_problems")?;
        Ok(Attempt {
            id: row.get("id")?,
            task_id: row.get("task_id")?,
            number: row.get("number")?,
            state: enum_from_str(&row.get::<_, String>("state")?)?,
            created_at: parse_ts(&row.get::<_, String>("created_at")?)?,
            updated_at: parse_ts(&row.get::<_, String>("updated_at")?)?,
            worker_pool: row.get("worker_pool")?,
            job: ArtifactRef {
                sha256: row.get("job_sha256")?,
                size: row.get::<_, i64>("job_size")? as u64,
            },
            human_feedback: row.get("human_feedback")?,
            not_before: row
                .get::<_, Option<String>>("not_before")?
                .map(|v| parse_ts(&v))
                .transpose()?,
            attempt_record: artifact_columns(row.get("record_sha256")?, row.get("record_size")?),
            result: artifact_columns(row.get("result_sha256")?, row.get("result_size")?),
            outcome: row
                .get::<_, Option<String>>("outcome")?
                .map(|s| enum_from_str::<Outcome>(&s))
                .transpose()?,
            validation_problems: serde_json::from_str(&problems).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?,
        })
    }

    pub fn get_attempt(&self, id: &str) -> Result<Attempt> {
        self.conn
            .query_row(
                "SELECT * FROM attempts WHERE id = ?1",
                [id],
                Self::attempt_from_row,
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound {
                entity: "attempt",
                id: id.to_owned(),
            })
    }

    pub fn list_attempts(&self, task_id: &str) -> Result<Vec<Attempt>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT * FROM attempts WHERE task_id = ?1 ORDER BY number")?;
        let rows = stmt.query_map([task_id], Self::attempt_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn attempts_in_state(&self, state: AttemptState) -> Result<Vec<Attempt>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT * FROM attempts WHERE state = ?1")?;
        let rows = stmt.query_map([state.as_str()], Self::attempt_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn latest_attempt(&self, task_id: &str) -> Result<Option<Attempt>> {
        Ok(self.list_attempts(task_id)?.pop())
    }

    fn transition_attempt(
        tx: &rusqlite::Transaction<'_>,
        attempt: &Attempt,
        next: AttemptState,
        actor: &str,
        payload: serde_json::Value,
    ) -> Result<()> {
        if !attempt.state.can_transition_to(next) {
            return Err(StoreError::InvalidTransition {
                entity: "attempt",
                id: attempt.id.clone(),
                from: attempt.state.as_str().to_owned(),
                to: next.as_str().to_owned(),
            });
        }
        // `attempt` may be stale: lease expiry runs outside the operation lock.
        let changed = tx.execute(
            "UPDATE attempts SET state = ?2, updated_at = ?3 WHERE id = ?1 AND state = ?4",
            params![
                attempt.id,
                next.as_str(),
                ts(Utc::now()),
                attempt.state.as_str()
            ],
        )?;
        if changed != 1 {
            return Err(StoreError::InvalidTransition {
                entity: "attempt",
                id: attempt.id.clone(),
                from: attempt.state.as_str().to_owned(),
                to: next.as_str().to_owned(),
            });
        }
        Self::insert_event(
            tx,
            Some(&attempt.task_id),
            Some(&attempt.id),
            actor,
            "attempt.state_changed",
            serde_json::json!({"from": attempt.state, "to": next, "details": payload}),
        )?;
        Ok(())
    }

    /// Hands the oldest queued attempt of a pool to a worker. Atomic: two workers cannot lease the
    /// same attempt.
    pub fn lease_next_attempt(
        &mut self,
        worker_pool: &str,
        worker_id: &str,
        ttl: Duration,
        now: DateTime<Utc>,
    ) -> Result<Option<(Attempt, Lease)>> {
        self.lease_next_attempt_matching(worker_pool, worker_id, ttl, now, |_| true)
    }

    pub fn lease_next_attempt_matching(
        &mut self,
        worker_pool: &str,
        worker_id: &str,
        ttl: Duration,
        now: DateTime<Utc>,
        accepts: impl Fn(&Attempt) -> bool,
    ) -> Result<Option<(Attempt, Lease)>> {
        self.observe_worker(worker_pool, worker_id, None, now)?;
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let queued: Vec<Attempt> = {
            let mut stmt = tx.prepare(
                "SELECT * FROM attempts WHERE state = 'queued' AND worker_pool = ?1
                 AND (not_before IS NULL OR not_before <= ?3)
                 AND EXISTS (SELECT 1 FROM workers w WHERE w.worker_pool = ?1 AND w.worker_id = ?2
                    AND w.drained = 0 AND w.quarantined = 0
                    AND (w.cooldown_until IS NULL OR w.cooldown_until <= ?3))
                 AND NOT EXISTS (SELECT 1 FROM leases WHERE worker_pool = ?1 AND worker_id = ?2)
                 ORDER BY created_at",
            )?;
            stmt.query_map(
                params![worker_pool, worker_id, ts(now)],
                Self::attempt_from_row,
            )?
            .collect::<rusqlite::Result<_>>()?
        };
        let attempt = queued.into_iter().find(|attempt| accepts(attempt));
        let Some(attempt) = attempt else {
            return Ok(None);
        };
        let lease = Lease {
            attempt_id: attempt.id.clone(),
            worker_pool: worker_pool.to_owned(),
            worker_id: worker_id.to_owned(),
            leased_at: now,
            expires_at: now + ttl,
        };
        tx.execute(
            "INSERT INTO leases (attempt_id, worker_pool, worker_id, leased_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                lease.attempt_id,
                lease.worker_pool,
                lease.worker_id,
                ts(lease.leased_at),
                ts(lease.expires_at)
            ],
        )?;
        Self::transition_attempt(
            &tx,
            &attempt,
            AttemptState::Leased,
            worker_id,
            serde_json::json!({"expires_at": lease.expires_at}),
        )?;
        tx.commit()?;
        let mut attempt = attempt;
        attempt.state = AttemptState::Leased;
        Ok(Some((attempt, lease)))
    }

    pub fn get_lease(&self, attempt_id: &str) -> Result<Option<Lease>> {
        Ok(self
            .conn
            .query_row(
                "SELECT attempt_id, worker_pool, worker_id, leased_at, expires_at FROM leases WHERE attempt_id = ?1",
                [attempt_id],
                |row| {
                    Ok(Lease {
                        attempt_id: row.get(0)?,
                        worker_pool: row.get(1)?,
                        worker_id: row.get(2)?,
                        leased_at: parse_ts(&row.get::<_, String>(3)?)?,
                        expires_at: parse_ts(&row.get::<_, String>(4)?)?,
                    })
                },
            )
            .optional()?)
    }

    /// Extends a lease. Fails when the lease belongs to another worker or no longer exists.
    pub fn heartbeat(
        &mut self,
        attempt_id: &str,
        worker_id: &str,
        ttl: Duration,
        now: DateTime<Utc>,
    ) -> Result<Lease> {
        self.heartbeat_until(attempt_id, worker_id, ttl, now, DateTime::<Utc>::MAX_UTC)
    }

    pub fn heartbeat_until(
        &mut self,
        attempt_id: &str,
        worker_id: &str,
        ttl: Duration,
        now: DateTime<Utc>,
        deadline: DateTime<Utc>,
    ) -> Result<Lease> {
        let lease = self
            .get_lease(attempt_id)?
            .filter(|l| l.worker_id == worker_id);
        let mut lease = lease.ok_or_else(|| StoreError::NotFound {
            entity: "lease",
            id: attempt_id.to_owned(),
        })?;
        // Clamp old leases too, including those renewed by an older controller before upgrade.
        if deadline < lease.expires_at {
            lease.expires_at = deadline;
            self.conn.execute(
                "UPDATE leases SET expires_at = ?2 WHERE attempt_id = ?1",
                params![attempt_id, ts(lease.expires_at)],
            )?;
        }
        if lease.expires_at <= now {
            return Err(StoreError::NotFound {
                entity: "lease",
                id: attempt_id.to_owned(),
            });
        }
        lease.expires_at = (now + ttl).min(deadline);
        self.observe_worker(&lease.worker_pool, worker_id, None, now)?;
        self.conn.execute(
            "UPDATE leases SET expires_at = ?2 WHERE attempt_id = ?1",
            params![attempt_id, ts(lease.expires_at)],
        )?;
        Ok(lease)
    }

    /// Marks every attempt whose lease expired as lost. Returns their ids.
    pub fn expire_leases(&mut self, now: DateTime<Utc>) -> Result<Vec<String>> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let expired: Vec<String> = {
            let mut stmt = tx.prepare("SELECT attempt_id FROM leases WHERE expires_at < ?1")?;
            let rows = stmt.query_map([ts(now)], |row| row.get(0))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        for id in &expired {
            let lease: (String, String) = tx.query_row(
                "SELECT worker_pool, worker_id FROM leases WHERE attempt_id = ?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            let attempt = tx.query_row(
                "SELECT * FROM attempts WHERE id = ?1",
                [id],
                Self::attempt_from_row,
            )?;
            Self::transition_attempt(
                &tx,
                &attempt,
                AttemptState::Lost,
                "orchestrator",
                serde_json::json!({}),
            )?;
            tx.execute(
                "INSERT OR IGNORE INTO lost_attempts (attempt_id, worker_pool, worker_id, lost_at)
                VALUES (?1, ?2, ?3, ?4)",
                params![id, lease.0, lease.1, ts(now)],
            )?;
            tx.execute(
                "UPDATE workers SET cooldown_until = ?3 WHERE worker_pool = ?1 AND worker_id = ?2",
                params![lease.0, lease.1, ts(now + Duration::minutes(5))],
            )?;
            Self::insert_event(
                &tx,
                Some(&attempt.task_id),
                Some(id),
                "orchestrator",
                "attempt.lost",
                serde_json::json!({"worker_pool": lease.0, "worker_id": lease.1}),
            )?;
            tx.execute("DELETE FROM leases WHERE attempt_id = ?1", [id])?;
        }
        tx.commit()?;
        Ok(expired)
    }

    /// Records the worker's upload. The attempt moves to `uploaded`; judging it is a separate step.
    pub fn record_upload(
        &mut self,
        attempt_id: &str,
        worker_id: &str,
        record: &ArtifactRef,
        result: Option<&ArtifactRef>,
        outcome: Option<Outcome>,
    ) -> Result<()> {
        let attempt = self.get_attempt(attempt_id)?;
        let lease = self
            .get_lease(attempt_id)?
            .filter(|l| l.worker_id == worker_id);
        if lease.is_none() {
            return Err(StoreError::NotFound {
                entity: "lease",
                id: attempt_id.to_owned(),
            });
        }
        let tx = self.conn.transaction()?;
        tx.execute(
            "UPDATE attempts SET record_sha256 = ?2, record_size = ?3, result_sha256 = ?4,
                                 result_size = ?5, outcome = ?6 WHERE id = ?1",
            params![
                attempt_id,
                record.sha256,
                record.size as i64,
                result.map(|r| r.sha256.clone()),
                result.map(|r| r.size as i64),
                outcome.map(|o| enum_to_str(&o)),
            ],
        )?;
        tx.execute("DELETE FROM leases WHERE attempt_id = ?1", [attempt_id])?;
        Self::transition_attempt(
            &tx,
            &attempt,
            AttemptState::Uploaded,
            worker_id,
            serde_json::json!({"record": record.sha256, "outcome": outcome}),
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn judge_attempt(
        &mut self,
        attempt_id: &str,
        problems: Vec<String>,
    ) -> Result<AttemptState> {
        let attempt = self.get_attempt(attempt_id)?;
        let next = if problems.is_empty() {
            AttemptState::Valid
        } else {
            AttemptState::Invalid
        };
        let tx = self.conn.transaction()?;
        tx.execute(
            "UPDATE attempts SET validation_problems = ?2 WHERE id = ?1",
            params![attempt_id, serde_json::to_string(&problems)?],
        )?;
        Self::transition_attempt(
            &tx,
            &attempt,
            next,
            "orchestrator",
            serde_json::json!({"problems": problems}),
        )?;
        tx.commit()?;
        Ok(next)
    }

    /// Atomically cancel only queued work. A worker which leased it first may finish.
    pub fn cancel_queued_attempt(&mut self, attempt_id: &str, actor: &str) -> Result<bool> {
        if self.get_attempt(attempt_id)?.state != AttemptState::Queued {
            return Ok(false);
        }
        self.cancel_attempt(attempt_id, actor)?;
        Ok(true)
    }

    pub fn cancel_attempt(&mut self, attempt_id: &str, actor: &str) -> Result<()> {
        let attempt = self.get_attempt(attempt_id)?;
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM leases WHERE attempt_id = ?1", [attempt_id])?;
        Self::transition_attempt(
            &tx,
            &attempt,
            AttemptState::Cancelled,
            actor,
            serde_json::json!({}),
        )?;
        tx.commit()?;
        Ok(())
    }

    // ----- merge requests ----------------------------------------------------------------------

    pub fn upsert_merge_request(&mut self, mr: &PublishedMergeRequest) -> Result<()> {
        self.conn.execute(
            "INSERT INTO merge_requests (task_id, repository_id, forge, number, url, source_branch,
                                         head_commit, state, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(task_id, repository_id) DO UPDATE SET
               forge = excluded.forge, number = excluded.number, url = excluded.url,
               source_branch = excluded.source_branch, head_commit = excluded.head_commit,
               state = excluded.state, updated_at = excluded.updated_at",
            params![
                mr.task_id,
                mr.repository_id,
                mr.forge,
                mr.number as i64,
                mr.url,
                mr.source_branch,
                mr.head_commit,
                mr.state.as_str(),
                ts(mr.updated_at),
            ],
        )?;
        Ok(())
    }

    fn merge_request_from_row(row: &Row<'_>) -> rusqlite::Result<PublishedMergeRequest> {
        Ok(PublishedMergeRequest {
            task_id: row.get("task_id")?,
            repository_id: row.get("repository_id")?,
            forge: row.get("forge")?,
            number: row.get::<_, i64>("number")? as u64,
            url: row.get("url")?,
            source_branch: row.get("source_branch")?,
            head_commit: row.get("head_commit")?,
            state: enum_from_str(&row.get::<_, String>("state")?)?,
            updated_at: parse_ts(&row.get::<_, String>("updated_at")?)?,
        })
    }

    pub fn get_merge_request(
        &self,
        task_id: &str,
        repository_id: &str,
    ) -> Result<Option<PublishedMergeRequest>> {
        Ok(self
            .conn
            .query_row(
                "SELECT * FROM merge_requests WHERE task_id = ?1 AND repository_id = ?2",
                [task_id, repository_id],
                Self::merge_request_from_row,
            )
            .optional()?)
    }

    pub fn list_merge_requests(&self, task_id: &str) -> Result<Vec<PublishedMergeRequest>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT * FROM merge_requests WHERE task_id = ?1 ORDER BY repository_id",
        )?;
        let rows = stmt.query_map([task_id], Self::merge_request_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    // ----- events ------------------------------------------------------------------------------

    fn insert_event(
        conn: &Connection,
        task_id: Option<&str>,
        attempt_id: Option<&str>,
        actor: &str,
        kind: &str,
        payload: serde_json::Value,
    ) -> Result<i64> {
        conn.execute(
            "INSERT INTO events (at, task_id, attempt_id, actor, kind, payload)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                ts(Utc::now()),
                task_id,
                attempt_id,
                actor,
                kind,
                payload.to_string()
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn record_event(
        &self,
        task_id: Option<&str>,
        attempt_id: Option<&str>,
        actor: &str,
        kind: &str,
        payload: serde_json::Value,
    ) -> Result<i64> {
        Self::insert_event(&self.conn, task_id, attempt_id, actor, kind, payload)
    }

    pub fn list_events(&self, task_id: &str) -> Result<Vec<Event>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT id, at, task_id, attempt_id, actor, kind, payload FROM events WHERE task_id = ?1 ORDER BY id")?;
        let rows = stmt.query_map([task_id], |row| {
            let payload: String = row.get(6)?;
            Ok(Event {
                id: row.get(0)?,
                at: parse_ts(&row.get::<_, String>(1)?)?,
                task_id: row.get(2)?,
                attempt_id: row.get(3)?,
                actor: row.get(4)?,
                kind: row.get(5)?,
                payload: serde_json::Value::from_str(&payload).unwrap_or(serde_json::Value::Null),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

#[cfg(test)]
mod retention_tests {
    use super::*;

    #[test]
    fn a_snapshot_is_a_complete_database_and_replaces_the_previous_one() {
        let dir = tempfile::tempdir().unwrap();
        let db = ConnectionDb::open(&dir.path().join("state.sqlite")).unwrap();
        let dest = dir.path().join("backup").join("state.sqlite");
        let artifact = ArtifactRef {
            sha256: "a".repeat(64),
            size: 10,
        };
        db.snapshot_to(&dest).unwrap();
        assert!(
            !ConnectionDb::open(&dest)
                .unwrap()
                .artifact_was_deleted(&artifact.sha256)
                .unwrap()
        );

        db.record_artifact_deletion(&artifact, Utc::now(), "test")
            .unwrap();
        std::fs::write(
            dir.path().join("backup").join("state.sqlite.partial"),
            "stale",
        )
        .unwrap();
        db.snapshot_to(&dest).unwrap();
        assert!(
            ConnectionDb::open(&dest)
                .unwrap()
                .artifact_was_deleted(&artifact.sha256)
                .unwrap()
        );
        assert!(
            !dir.path()
                .join("backup")
                .join("state.sqlite.partial")
                .exists()
        );
    }

    #[test]
    fn deletion_receipts_require_full_sync_and_restore_the_connection_after_failure() {
        let dir = tempfile::tempdir().unwrap();
        let db = ConnectionDb::open(&dir.path().join("state.sqlite")).unwrap();
        db.conn
            .execute_batch(
                "CREATE TRIGGER require_durable_deletion BEFORE INSERT ON artifact_deletions
             WHEN (SELECT synchronous FROM pragma_synchronous) != 2
             BEGIN SELECT RAISE(ABORT, 'deletion receipt needs FULL synchronization'); END;
             CREATE TRIGGER reject_deletion BEFORE INSERT ON artifact_deletions
             WHEN NEW.reason = 'reject'
             BEGIN SELECT RAISE(ABORT, 'simulated receipt failure'); END;",
            )
            .unwrap();
        let artifact = ArtifactRef {
            sha256: "a".repeat(64),
            size: 10,
        };
        db.record_artifact_deletion(&artifact, Utc::now(), "completed_task_repository_input")
            .unwrap();
        assert!(db.artifact_was_deleted(&artifact.sha256).unwrap());
        let sync = || {
            db.conn
                .pragma_query_value(None, "synchronous", |row| row.get::<_, i64>(0))
                .unwrap()
        };
        assert_eq!(sync(), 1);
        let rejected = ArtifactRef {
            sha256: "b".repeat(64),
            size: 20,
        };
        assert!(
            db.record_artifact_deletion(&rejected, Utc::now(), "reject")
                .is_err()
        );
        assert_eq!(sync(), 1);
        assert!(!db.artifact_was_deleted(&rejected.sha256).unwrap());
    }
}
