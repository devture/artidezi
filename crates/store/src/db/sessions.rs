// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
// SPDX-License-Identifier: Apache-2.0

use super::*;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiSession {
    pub csrf: String,
    pub principal_id: Option<String>,
    /// Present only while saving a verified login; not persisted in the session row.
    pub verified_revision: Option<i64>,
}

impl ConnectionDb {
    /// Call even without a cookie so older login-mode sessions are discarded on the first visit.
    pub fn ui_session_csrf(
        &self,
        token_digest: Option<&str>,
        scope: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<UiSession>> {
        self.conn.execute(
            "DELETE FROM ui_sessions WHERE expires_at <= ?1 OR credential_scope != ?2",
            params![now.timestamp(), scope],
        )?;
        Ok(self
            .conn
            .query_row(
                "SELECT csrf, principal_id FROM ui_sessions WHERE token_digest = ?1",
                [token_digest],
                |row| {
                    Ok(UiSession {
                        csrf: row.get(0)?,
                        principal_id: row.get(1)?,
                        verified_revision: None,
                    })
                },
            )
            .optional()?)
    }

    /// Replace a browser's old login atomically, bounded independently of expired sessions.
    /// Only a digest of the opaque bearer is persisted; database reads cannot recover cookies.
    pub fn save_ui_session(
        &mut self,
        token_digest: &str,
        session: &UiSession,
        scope: &str,
        expires_at: DateTime<Utc>,
        replaced_digest: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<bool> {
        let tx = self.conn.transaction()?;
        if let Some(expected) = session.verified_revision {
            let actual: Option<i64> = tx
                .query_row(
                    "SELECT revision FROM ui_users WHERE id = ?1 AND disabled = 0",
                    [session.principal_id.as_deref()],
                    |row| row.get(0),
                )
                .optional()?;
            if actual != Some(expected) {
                return Ok(false);
            }
        }
        tx.execute(
            "DELETE FROM ui_sessions WHERE expires_at <= ?1 OR credential_scope != ?2 OR token_digest = ?3",
            params![now.timestamp(), scope, replaced_digest],
        )?;
        let count: u32 = tx.query_row("SELECT COUNT(*) FROM ui_sessions", [], |r| r.get(0))?;
        let available = count < 128;
        if available {
            tx.execute(
                "INSERT INTO ui_sessions (token_digest, csrf, principal_id, credential_scope, expires_at) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![token_digest, session.csrf, session.principal_id, scope, expires_at.timestamp()],
            )?;
        }
        tx.commit()?;
        Ok(available)
    }

    pub fn delete_ui_session(&self, token_digest: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM ui_sessions WHERE token_digest = ?1",
            [token_digest],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Db;

    fn session(csrf: &str, principal_id: Option<&str>) -> UiSession {
        UiSession {
            csrf: csrf.into(),
            principal_id: principal_id.map(str::to_owned),
            verified_revision: None,
        }
    }

    #[test]
    fn sessions_survive_reopening_but_not_expiry_rotation_or_logout() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.sqlite");
        let now = Utc::now();
        let expires = now + Duration::days(30);
        let mut db = Db::open(&path).unwrap();
        assert!(
            db.save_ui_session(
                "digest",
                &session("csrf", Some("alice")),
                "scope",
                expires,
                None,
                now,
            )
            .unwrap()
        );
        drop(db);
        let mut db = Db::open(&path).unwrap();
        assert_eq!(
            db.ui_session_csrf(Some("digest"), "scope", now + Duration::days(20))
                .unwrap()
                .map(|session| (session.csrf, session.principal_id)),
            Some(("csrf".into(), Some("alice".into())))
        );
        assert_eq!(
            db.ui_session_csrf(Some("digest"), "scope", expires)
                .unwrap(),
            None
        );
        assert!(
            db.save_ui_session(
                "digest",
                &session("csrf", Some("alice")),
                "scope",
                expires,
                None,
                now,
            )
            .unwrap()
        );
        // A visit without a cookie also invalidates old sessions; restoring a previous password
        // must not resurrect those sessions after the new configuration has served a request.
        assert_eq!(db.ui_session_csrf(None, "rotated", now).unwrap(), None);
        assert_eq!(
            db.ui_session_csrf(Some("digest"), "scope", now).unwrap(),
            None
        );
        assert!(
            db.save_ui_session(
                "new",
                &session("csrf", Some("alice")),
                "scope",
                expires,
                None,
                now,
            )
            .unwrap()
        );
        db.delete_ui_session("new").unwrap();
        assert_eq!(db.ui_session_csrf(Some("new"), "scope", now).unwrap(), None);
    }

    #[test]
    fn session_limit_allows_replacement_and_reclaims_expired_entries() {
        let mut db = Db::open_in_memory().unwrap();
        let now = Utc::now();
        let expires = now + Duration::days(30);
        for i in 0..128 {
            assert!(
                db.save_ui_session(
                    &i.to_string(),
                    &session("csrf", None),
                    "scope",
                    expires,
                    None,
                    now,
                )
                .unwrap()
            );
        }
        assert!(
            !db.save_ui_session(
                "overflow",
                &session("csrf", None),
                "scope",
                expires,
                None,
                now,
            )
            .unwrap()
        );
        assert!(
            db.save_ui_session(
                "replacement",
                &session("new-csrf", None),
                "scope",
                expires,
                Some("0"),
                now,
            )
            .unwrap()
        );
        assert_eq!(db.ui_session_csrf(Some("0"), "scope", now).unwrap(), None);
        assert!(
            db.save_ui_session(
                "fresh",
                &session("csrf", None),
                "scope",
                expires + Duration::days(30),
                None,
                expires,
            )
            .unwrap()
        );
    }
}
