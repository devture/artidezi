// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

use super::*;
use artidezi_contract::wire::AdvertisedHarness;

impl ConnectionDb {
    pub fn adopt_transient_failure(&mut self, attempt_id: &str, reason: &str) -> Result<bool> {
        let tx = self.conn.transaction()?;
        let task_id: Option<String> = tx
            .query_row(
                "SELECT a.task_id FROM attempts a JOIN tasks t ON t.id = a.task_id
             WHERE a.id = ?1 AND a.state = 'invalid' AND a.result_sha256 IS NULL
             AND t.state = 'needs_human' AND NOT EXISTS
             (SELECT 1 FROM transient_failures WHERE attempt_id = a.id)
             AND a.number = (SELECT MAX(number) FROM attempts WHERE task_id = a.task_id)",
                [attempt_id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(task_id) = task_id else {
            return Ok(false);
        };
        tx.execute(
            "INSERT INTO transient_failures (attempt_id, reason) VALUES (?1, ?2)",
            params![attempt_id, reason],
        )?;
        tx.execute(
            "UPDATE tasks SET state = 'working', updated_at = ?2 WHERE id = ?1",
            params![task_id, ts(Utc::now())],
        )?;
        Self::insert_event(
            &tx,
            Some(&task_id),
            Some(attempt_id),
            "recovery",
            "task.state_changed",
            serde_json::json!({"from":"needs_human","to":"working","reason":reason}),
        )?;
        tx.commit()?;
        Ok(true)
    }
    pub fn transient_failure_recorded(&self, attempt_id: &str) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM transient_failures WHERE attempt_id = ?1)",
            [attempt_id],
            |row| row.get(0),
        )?)
    }
    pub fn discovery_paused(&self) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT discovery_paused FROM instance_runtime WHERE singleton = 1",
            [],
            |row| row.get(0),
        )?)
    }
    pub fn set_discovery_paused(&mut self, paused: bool, actor: &str) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "UPDATE instance_runtime SET discovery_paused = ?1 WHERE singleton = 1",
            [paused],
        )?;
        Self::insert_event(
            &tx,
            None,
            None,
            actor,
            "instance.discovery_pause_changed",
            serde_json::json!({"discovery_paused":paused}),
        )?;
        tx.commit()?;
        Ok(())
    }
    pub fn record_transient_failure(&self, attempt_id: &str, reason: &str) -> Result<()> {
        self.conn.execute("INSERT OR IGNORE INTO transient_failures (attempt_id, reason)
            SELECT id, ?2 FROM attempts WHERE id = ?1 AND state = 'invalid' AND result_sha256 IS NULL", params![attempt_id, reason])?;
        Ok(())
    }

    pub fn pending_transient_failures(&self) -> Result<Vec<Attempt>> {
        let mut stmt = self.conn.prepare("SELECT a.* FROM attempts a JOIN transient_failures f ON f.attempt_id = a.id
            JOIN tasks t ON t.id = a.task_id WHERE a.state = 'invalid' AND t.state = 'working' AND f.replacement_id IS NULL
            AND a.number = (SELECT MAX(number) FROM attempts WHERE task_id = a.task_id)")?;
        Ok(stmt
            .query_map([], Self::attempt_from_row)?
            .collect::<std::result::Result<_, _>>()?)
    }

    pub fn recover_transient_attempt(
        &mut self,
        failed: &str,
        replacement: &Attempt,
        max_retries: u32,
        now: DateTime<Utc>,
    ) -> Result<bool> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let attempt = tx.query_row(
            "SELECT * FROM attempts WHERE id = ?1",
            [failed],
            Self::attempt_from_row,
        )?;
        let current: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM tasks t JOIN transient_failures f ON f.attempt_id = ?2
            WHERE t.id = ?1 AND t.state = 'working' AND f.replacement_id IS NULL
            AND ?2 = (SELECT id FROM attempts WHERE task_id = t.id ORDER BY number DESC LIMIT 1))",
            params![attempt.task_id, failed],
            |row| row.get(0),
        )?;
        if !current || attempt.state != AttemptState::Invalid || attempt.result.is_some() {
            return Ok(false);
        }
        let count: u32 = tx.query_row(
            "SELECT COUNT(*) FROM transient_failures f JOIN attempts a ON a.id = f.attempt_id
            WHERE a.task_id = ?1 AND f.replacement_id IS NOT NULL",
            [&attempt.task_id],
            |row| row.get(0),
        )?;
        let safe: bool = tx.query_row("SELECT NOT EXISTS (SELECT 1 FROM attempts WHERE task_id = ?1 AND state IN ('uploaded','valid'))
            AND NOT EXISTS (SELECT 1 FROM merge_requests WHERE task_id = ?1)
            AND NOT EXISTS (SELECT 1 FROM events WHERE task_id = ?1 AND kind LIKE 'publication.%')", [&attempt.task_id], |row| row.get(0))?;
        if safe && count < max_retries {
            if replacement.task_id != attempt.task_id
                || Some(replacement.number) != attempt.number.checked_add(1)
                || replacement.state != AttemptState::Queued
                || replacement.worker_pool != attempt.worker_pool
            {
                return Err(StoreError::InvalidTransition {
                    entity: "recovery",
                    id: failed.into(),
                    from: "invalid".into(),
                    to: "invalid replacement".into(),
                });
            }
            let mut next = replacement.clone();
            let delays = [60, 120, 300, 600, 1800];
            let delay = delays[(count as usize).min(delays.len() - 1)];
            next.not_before = Some(now + Duration::seconds(delay));
            Self::insert_attempt_in(&tx, &next)?;
            tx.execute("UPDATE transient_failures SET replacement_id = ?2, retry_number = ?3 WHERE attempt_id = ?1", params![failed,next.id,count+1])?;
            let reason: String = tx.query_row(
                "SELECT reason FROM transient_failures WHERE attempt_id = ?1",
                [failed],
                |row| row.get(0),
            )?;
            Self::insert_event(
                &tx,
                Some(&attempt.task_id),
                Some(&next.id),
                "orchestrator",
                "attempt.transient_retry_queued",
                serde_json::json!({"failed_attempt_id":failed,"retry_number":count+1,"max_retries":max_retries,"reason":reason,"not_before":next.not_before}),
            )?;
            tx.commit()?;
            return Ok(true);
        }
        tx.execute(
            "UPDATE tasks SET state = 'needs_human', updated_at = ?2 WHERE id = ?1",
            params![attempt.task_id, ts(now)],
        )?;
        Self::insert_event(
            &tx,
            Some(&attempt.task_id),
            Some(failed),
            "orchestrator",
            "task.state_changed",
            serde_json::json!({"from":"working","to":"needs_human","reason":"transient execution retry limit or publication safety guard","retry_number":count,"max_retries":max_retries}),
        )?;
        tx.commit()?;
        Ok(false)
    }

    pub fn observe_worker(
        &self,
        pool: &str,
        id: &str,
        quarantined: Option<bool>,
        now: DateTime<Utc>,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO workers (worker_pool, worker_id, last_seen, quarantined)
            VALUES (?1, ?2, ?3, COALESCE(?4, 0)) ON CONFLICT(worker_pool, worker_id) DO UPDATE SET
            last_seen = excluded.last_seen, quarantined = COALESCE(?4, workers.quarantined)",
            params![pool, id, ts(now), quarantined],
        )?;
        Ok(())
    }

    /// Records what a worker offers, so that unschedulable work can say what is missing.
    pub fn record_worker_offer(
        &self,
        pool: &str,
        id: &str,
        harnesses: &[AdvertisedHarness],
        tags: &[String],
        now: DateTime<Utc>,
    ) -> Result<()> {
        self.observe_worker(pool, id, None, now)?;
        self.conn.execute(
            "UPDATE workers SET harnesses = ?3, tags = ?4 WHERE worker_pool = ?1 AND worker_id = ?2",
            params![
                pool,
                id,
                serde_json::to_string(harnesses)?,
                serde_json::to_string(tags)?
            ],
        )?;
        Ok(())
    }

    /// Drops workers the configuration no longer lists. Their leases expire and recover as usual.
    pub fn forget_unlisted_workers(&self, listed: &[(&str, &str)]) -> Result<()> {
        let known: Vec<(String, String)> = self
            .conn
            .prepare("SELECT worker_pool, worker_id FROM workers")?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<std::result::Result<_, _>>()?;
        for (pool, id) in known {
            if !listed.contains(&(pool.as_str(), id.as_str())) {
                self.conn.execute(
                    "DELETE FROM workers WHERE worker_pool = ?1 AND worker_id = ?2",
                    params![pool, id],
                )?;
            }
        }
        Ok(())
    }

    pub fn list_workers(&self) -> Result<Vec<Worker>> {
        let mut stmt = self.conn.prepare("SELECT w.*, l.attempt_id, l.expires_at, t.number AS task_number
            FROM workers w LEFT JOIN leases l ON l.worker_pool = w.worker_pool AND l.worker_id = w.worker_id
            LEFT JOIN attempts a ON a.id = l.attempt_id LEFT JOIN task_numbers t ON t.task_id = a.task_id
            ORDER BY w.worker_pool, w.worker_id")?;
        Ok(stmt
            .query_map([], |row| {
                Ok(Worker {
                    worker_pool: row.get("worker_pool")?,
                    worker_id: row.get("worker_id")?,
                    last_seen: parse_ts(&row.get::<_, String>("last_seen")?)?,
                    quarantined: row.get("quarantined")?,
                    drained: row.get("drained")?,
                    harnesses: serde_json::from_str(&row.get::<_, String>("harnesses")?)
                        .unwrap_or_default(),
                    tags: serde_json::from_str(&row.get::<_, String>("tags")?).unwrap_or_default(),
                    cooldown_until: row
                        .get::<_, Option<String>>("cooldown_until")?
                        .map(|v| parse_ts(&v))
                        .transpose()?,
                    attempt_id: row.get("attempt_id")?,
                    task_number: row.get("task_number")?,
                    lease_expires_at: row
                        .get::<_, Option<String>>("expires_at")?
                        .map(|v| parse_ts(&v))
                        .transpose()?,
                })
            })?
            .collect::<std::result::Result<_, _>>()?)
    }

    pub fn drain_worker(&mut self, pool: &str, id: &str, drained: bool, actor: &str) -> Result<()> {
        let tx = self.conn.transaction()?;
        if tx.execute(
            "UPDATE workers SET drained = ?3 WHERE worker_pool = ?1 AND worker_id = ?2",
            params![pool, id, drained],
        )? != 1
        {
            return Err(StoreError::NotFound {
                entity: "worker",
                id: format!("{pool}/{id}"),
            });
        }
        Self::insert_event(
            &tx,
            None,
            None,
            actor,
            "worker.drain_changed",
            serde_json::json!({"worker_pool":pool,"worker_id":id,"drained":drained}),
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn pending_lost_attempts(&self) -> Result<Vec<Attempt>> {
        let mut stmt = self.conn.prepare(
            "SELECT a.* FROM attempts a JOIN tasks t ON t.id = a.task_id
            WHERE a.state = 'lost' AND t.state = 'working'
            AND a.number = (SELECT MAX(number) FROM attempts WHERE task_id = a.task_id)",
        )?;
        Ok(stmt
            .query_map([], Self::attempt_from_row)?
            .collect::<std::result::Result<_, _>>()?)
    }

    /// Commit either a bounded replacement or a human-attention decision atomically.
    pub fn recover_lost_attempt(
        &mut self,
        lost: &str,
        replacement: Option<&Attempt>,
        max_retries: u32,
    ) -> Result<bool> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let attempt = tx.query_row(
            "SELECT * FROM attempts WHERE id = ?1",
            [lost],
            Self::attempt_from_row,
        )?;
        let current: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM tasks t WHERE t.id = ?1 AND t.state = 'working'
            AND ?2 = (SELECT id FROM attempts WHERE task_id = t.id ORDER BY number DESC LIMIT 1))",
            params![attempt.task_id, lost],
            |row| row.get(0),
        )?;
        if !current || attempt.state != AttemptState::Lost {
            return Ok(false);
        }
        let count: u32 = tx
            .query_row(
                "SELECT COUNT(*) FROM lost_attempts l JOIN attempts a ON a.id = l.attempt_id WHERE a.task_id = ?1 AND l.replacement_id IS NOT NULL",
                [&attempt.task_id],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(0);
        let safe: bool = tx.query_row("SELECT NOT EXISTS (SELECT 1 FROM attempts a WHERE task_id = ?1 AND ((record_sha256 IS NOT NULL AND NOT EXISTS (SELECT 1 FROM transient_failures f WHERE f.attempt_id = a.id)) OR result_sha256 IS NOT NULL OR state IN ('uploaded','valid')))
            AND NOT EXISTS (SELECT 1 FROM merge_requests WHERE task_id = ?1)
            AND NOT EXISTS (SELECT 1 FROM events WHERE task_id = ?1 AND kind LIKE 'publication.%')
            AND EXISTS (SELECT 1 FROM lost_attempts WHERE attempt_id = ?2 AND replacement_id IS NULL)",
            params![attempt.task_id, lost], |row| row.get(0))?;
        if safe
            && count < max_retries
            && let Some(next) = replacement
        {
            if next.task_id != attempt.task_id
                || Some(next.number) != attempt.number.checked_add(1)
                || next.state != AttemptState::Queued
                || next.worker_pool != attempt.worker_pool
            {
                return Err(StoreError::InvalidTransition {
                    entity: "recovery",
                    id: lost.into(),
                    from: "lost".into(),
                    to: "invalid replacement".into(),
                });
            }
            Self::insert_attempt_in(&tx, next)?;
            tx.execute("UPDATE lost_attempts SET replacement_id = ?2, retry_number = ?3 WHERE attempt_id = ?1", params![lost, next.id, count + 1])?;
            Self::insert_event(
                &tx,
                Some(&attempt.task_id),
                Some(&next.id),
                "orchestrator",
                "attempt.recovery_queued",
                serde_json::json!({"lost_attempt_id":lost,"retry_number":count+1,"max_retries":max_retries}),
            )?;
            tx.commit()?;
            return Ok(true);
        }
        tx.execute(
            "UPDATE tasks SET state = 'needs_human', updated_at = ?2 WHERE id = ?1",
            params![attempt.task_id, ts(Utc::now())],
        )?;
        Self::insert_event(
            &tx,
            Some(&attempt.task_id),
            Some(lost),
            "orchestrator",
            "task.state_changed",
            serde_json::json!({"from":"working","to":"needs_human","reason":"lost execution could not be retried automatically","retry_number":count,"max_retries":max_retries}),
        )?;
        tx.commit()?;
        Ok(false)
    }
}
