// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiGrant {
    Read,
    Write,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UiUser {
    pub id: String,
    pub display_name: String,
    pub password_hash: String,
    pub disabled: bool,
    pub instance_admin: bool,
    pub grants: BTreeMap<String, UiGrant>,
    pub revision: i64,
    pub password_change_required: bool,
    pub managed_by_config: bool,
    pub timezone: String,
    pub timezone_initialized: bool,
}

#[derive(Clone, Debug)]
pub struct UiAccountDeclaration {
    pub id: String,
    pub display_name: String,
    pub initial_password_hash: String,
    pub disabled: bool,
    pub instance_admin: bool,
    pub grants: BTreeMap<String, UiGrant>,
}

fn user_row(row: &Row<'_>) -> rusqlite::Result<UiUser> {
    let grants: String = row.get(5)?;
    let grants = serde_json::from_str(&grants).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(5, rusqlite::types::Type::Text, Box::new(error))
    })?;
    Ok(UiUser {
        id: row.get(0)?,
        display_name: row.get(1)?,
        password_hash: row.get(2)?,
        disabled: row.get(3)?,
        instance_admin: row.get(4)?,
        grants,
        revision: row.get(6)?,
        password_change_required: row.get(7)?,
        managed_by_config: row.get(8)?,
        timezone: row.get(9)?,
        timezone_initialized: row.get(10)?,
    })
}

impl ConnectionDb {
    /// Apply declared identity and access atomically while preserving current passwords.
    /// Rows omitted from configuration are disabled rather than deleted for durable attribution.
    pub fn reconcile_ui_accounts(&mut self, desired: &[UiAccountDeclaration]) -> Result<()> {
        let tx = self.conn.transaction()?;
        let ids: BTreeSet<&str> = desired.iter().map(|account| account.id.as_str()).collect();
        for account in desired {
            let current: Option<UiUser> = tx.query_row(
                "SELECT id, display_name, password_hash, disabled, instance_admin, grants_json, revision, password_change_required, managed_by_config, timezone, timezone_initialized FROM ui_users WHERE id = ?1",
                [&account.id], user_row,
            ).optional()?;
            let grants_json = serde_json::to_string(&account.grants)?;
            if let Some(current) = current {
                if current.display_name != account.display_name
                    || current.disabled != account.disabled
                    || current.instance_admin != account.instance_admin
                    || current.grants != account.grants
                    || !current.managed_by_config
                {
                    tx.execute(
                        "UPDATE ui_users SET display_name = ?2, disabled = ?3, instance_admin = ?4, grants_json = ?5, managed_by_config = 1, revision = revision + 1 WHERE id = ?1",
                        params![account.id, account.display_name, account.disabled, account.instance_admin, grants_json],
                    )?;
                    if account.disabled && !current.disabled {
                        tx.execute(
                            "DELETE FROM ui_sessions WHERE principal_id = ?1",
                            [&account.id],
                        )?;
                    }
                }
            } else {
                tx.execute(
                    "INSERT INTO ui_users (id, display_name, password_hash, disabled, instance_admin, grants_json, revision, password_change_required, managed_by_config, timezone_initialized) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, 1, 1, 0)",
                    params![account.id, account.display_name, account.initial_password_hash, account.disabled, account.instance_admin, grants_json],
                )?;
            }
        }
        let mut stmt = tx.prepare("SELECT id, disabled, grants_json FROM ui_users")?;
        let existing = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, bool>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        for (id, disabled, grants_json) in existing {
            if !ids.contains(id.as_str()) && (!disabled || grants_json != "{}") {
                tx.execute(
                    "UPDATE ui_users SET disabled = 1, grants_json = '{}', revision = revision + 1 WHERE id = ?1",
                    [&id],
                )?;
                tx.execute("DELETE FROM ui_sessions WHERE principal_id = ?1", [&id])?;
            }
        }
        let enabled_admins: i64 = tx.query_row(
            "SELECT COUNT(*) FROM ui_users WHERE instance_admin = 1 AND disabled = 0",
            [],
            |row| row.get(0),
        )?;
        if enabled_admins == 0 {
            return Err(crate::StoreError::InvalidUiAccount(
                "at least one enabled instance administrator is required".into(),
            ));
        }
        tx.commit()?;
        Ok(())
    }

    pub fn get_ui_user(&self, id: &str) -> Result<Option<UiUser>> {
        Ok(self.conn.query_row(
            "SELECT id, display_name, password_hash, disabled, instance_admin, grants_json, revision, password_change_required, managed_by_config, timezone, timezone_initialized FROM ui_users WHERE id = ?1",
            [id], user_row,
        ).optional()?)
    }

    pub fn list_ui_users(&self) -> Result<Vec<UiUser>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, display_name, password_hash, disabled, instance_admin, grants_json, revision, password_change_required, managed_by_config, timezone, timezone_initialized FROM ui_users ORDER BY id",
        )?;
        Ok(stmt
            .query_map([], user_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn create_ui_user(&self, user: &UiUser) -> Result<()> {
        self.conn.execute(
            "INSERT INTO ui_users (id, display_name, password_hash, disabled, instance_admin, grants_json, revision, password_change_required, managed_by_config, timezone, timezone_initialized) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7, ?8, ?9, ?10)",
            params![user.id, user.display_name, user.password_hash, user.disabled, user.instance_admin, serde_json::to_string(&user.grants)?, user.password_change_required, user.managed_by_config, user.timezone, user.timezone_initialized],
        )?;
        Ok(())
    }

    pub fn update_ui_user(&mut self, user: &UiUser) -> Result<bool> {
        let tx = self.conn.transaction()?;
        let changed = update_ui_user_in(&tx, user)?;
        tx.commit()?;
        Ok(changed)
    }

    /// Compare-and-swap the old verifier so two password changes cannot overwrite one another.
    pub fn change_ui_password(
        &mut self,
        id: &str,
        expected_hash: &str,
        new_hash: &str,
    ) -> Result<bool> {
        let tx = self.conn.transaction()?;
        let changed = tx.execute(
            "UPDATE ui_users SET password_hash = ?3, revision = revision + 1, password_change_required = 0 WHERE id = ?1 AND password_hash = ?2 AND disabled = 0",
            params![id, expected_hash, new_hash],
        )?;
        if changed != 0 {
            tx.execute("DELETE FROM ui_sessions WHERE principal_id = ?1", [id])?;
        }
        tx.commit()?;
        Ok(changed != 0)
    }

    pub fn change_ui_timezone(&mut self, id: &str, timezone: &str) -> Result<bool> {
        Ok(self.conn.execute(
            "UPDATE ui_users SET timezone = ?2, timezone_initialized = 1, revision = revision + 1 WHERE id = ?1 AND disabled = 0",
            params![id, timezone],
        )? != 0)
    }
}

fn update_ui_user_in(tx: &rusqlite::Transaction<'_>, user: &UiUser) -> Result<bool> {
    let old: Option<(String, bool)> = tx
        .query_row(
            "SELECT password_hash, disabled FROM ui_users WHERE id = ?1 AND revision = ?2",
            params![user.id, user.revision],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((old_hash, old_disabled)) = old else {
        return Ok(false);
    };
    tx.execute(
            "UPDATE ui_users SET display_name = ?2, password_hash = ?3, disabled = ?4, instance_admin = ?5, grants_json = ?6, revision = revision + 1, password_change_required = ?8, managed_by_config = ?9, timezone = ?10, timezone_initialized = ?11 WHERE id = ?1 AND revision = ?7",
            params![user.id, user.display_name, user.password_hash, user.disabled, user.instance_admin, serde_json::to_string(&user.grants)?, user.revision, user.password_change_required, user.managed_by_config, user.timezone, user.timezone_initialized],
        )?;
    let enabled_admins: i64 = tx.query_row(
        "SELECT COUNT(*) FROM ui_users WHERE instance_admin = 1 AND disabled = 0",
        [],
        |row| row.get(0),
    )?;
    if enabled_admins == 0 {
        return Err(crate::StoreError::InvalidUiAccount(
            "at least one enabled instance administrator is required".into(),
        ));
    }
    if old_hash != user.password_hash || (!old_disabled && user.disabled) {
        tx.execute(
            "DELETE FROM ui_sessions WHERE principal_id = ?1",
            [&user.id],
        )?;
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Db;

    fn user(id: &str, admin: bool) -> UiUser {
        UiUser {
            id: id.into(),
            display_name: id.into(),
            password_hash: "old-hash".into(),
            disabled: false,
            instance_admin: admin,
            grants: BTreeMap::new(),
            revision: 1,
            password_change_required: false,
            managed_by_config: false,
            timezone: "UTC".into(),
            timezone_initialized: true,
        }
    }

    #[test]
    fn account_updates_preserve_or_revoke_sessions_as_required() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("accounts.sqlite");
        let mut db = Db::open(&path).unwrap();
        db.create_ui_user(&user("owner", true)).unwrap();
        db.create_ui_user(&user("reader", false)).unwrap();
        let now = Utc::now();
        let session = crate::db::UiSession {
            csrf: "csrf".into(),
            principal_id: Some("reader".into()),
            verified_revision: Some(1),
        };
        assert!(
            db.save_ui_session(
                "token",
                &session,
                "named",
                now + Duration::days(30),
                None,
                now,
            )
            .unwrap()
        );
        let mut reader = db.get_ui_user("reader").unwrap().unwrap();
        reader.grants.insert("app".into(), UiGrant::Read);
        assert!(db.update_ui_user(&reader).unwrap());
        assert!(
            db.ui_session_csrf(Some("token"), "named", now)
                .unwrap()
                .is_some()
        );
        assert!(
            !db.save_ui_session(
                "stale",
                &session,
                "named",
                now + Duration::days(30),
                None,
                now,
            )
            .unwrap()
        );
        assert!(
            db.get_ui_user("reader")
                .unwrap()
                .unwrap()
                .grants
                .contains_key("app")
        );
        assert!(
            !db.change_ui_password("reader", "wrong", "new-hash")
                .unwrap()
        );
        assert!(
            db.change_ui_password("reader", "old-hash", "new-hash")
                .unwrap()
        );
        assert!(
            db.ui_session_csrf(Some("token"), "named", now)
                .unwrap()
                .is_none()
        );
        let mut owner = db.get_ui_user("owner").unwrap().unwrap();
        owner.disabled = true;
        assert!(matches!(
            db.update_ui_user(&owner),
            Err(crate::StoreError::InvalidUiAccount(_))
        ));
        assert!(!db.get_ui_user("owner").unwrap().unwrap().disabled);
        let mut reader = db.get_ui_user("reader").unwrap().unwrap();
        reader.disabled = true;
        assert!(db.update_ui_user(&reader).unwrap());
        drop(db);
        let db = Db::open(&path).unwrap();
        assert!(db.get_ui_user("reader").unwrap().unwrap().disabled);
    }

    #[test]
    fn configured_access_reconciles_while_current_password_drifts() {
        let mut db = Db::open_in_memory().unwrap();
        db.create_ui_user(&user("legacy", true)).unwrap();
        let owner = UiAccountDeclaration {
            id: "owner".into(),
            display_name: "Owner".into(),
            initial_password_hash: "owner-initial".into(),
            disabled: false,
            instance_admin: true,
            grants: BTreeMap::new(),
        };
        let mut declared = UiAccountDeclaration {
            id: "alice".into(),
            display_name: "Alice".into(),
            initial_password_hash: "initial-hash".into(),
            disabled: false,
            instance_admin: false,
            grants: BTreeMap::from([("app".into(), UiGrant::Read)]),
        };
        db.reconcile_ui_accounts(&[owner.clone(), declared.clone()])
            .unwrap();
        assert!(db.get_ui_user("legacy").unwrap().unwrap().disabled);
        let alice = db.get_ui_user("alice").unwrap().unwrap();
        assert_eq!(alice.password_hash, "initial-hash");
        assert!(alice.password_change_required && alice.managed_by_config);
        assert_eq!(alice.timezone, "UTC");
        assert!(!alice.timezone_initialized);
        assert!(db.change_ui_timezone("alice", "Europe/Lisbon").unwrap());
        let alice = db.get_ui_user("alice").unwrap().unwrap();
        assert_eq!(alice.timezone, "Europe/Lisbon");
        assert!(alice.timezone_initialized);
        assert!(
            db.change_ui_password("alice", "initial-hash", "personal-hash")
                .unwrap()
        );
        let now = Utc::now();
        let revision = db.get_ui_user("alice").unwrap().unwrap().revision;
        assert!(
            db.save_ui_session(
                "alice-token",
                &crate::db::UiSession {
                    csrf: "csrf".into(),
                    principal_id: Some("alice".into()),
                    verified_revision: Some(revision),
                },
                "named",
                now + Duration::days(30),
                None,
                now,
            )
            .unwrap()
        );
        declared.display_name = "Alice Changed".into();
        declared.initial_password_hash = "different-initial-hash".into();
        declared.grants.insert("app".into(), UiGrant::Write);
        db.reconcile_ui_accounts(&[owner.clone(), declared.clone()])
            .unwrap();
        let alice = db.get_ui_user("alice").unwrap().unwrap();
        assert_eq!(alice.password_hash, "personal-hash");
        assert!(!alice.password_change_required);
        assert_eq!(alice.display_name, "Alice Changed");
        assert_eq!(alice.grants["app"], UiGrant::Write);
        assert_eq!(alice.timezone, "Europe/Lisbon");
        assert!(
            db.ui_session_csrf(Some("alice-token"), "named", now)
                .unwrap()
                .is_some()
        );
        let revision = alice.revision;
        db.reconcile_ui_accounts(&[owner.clone(), declared.clone()])
            .unwrap();
        assert_eq!(db.get_ui_user("alice").unwrap().unwrap().revision, revision);
        let mut alice = db.get_ui_user("alice").unwrap().unwrap();
        alice.display_name = "Temporary CLI Edit".into();
        alice.grants.insert("app".into(), UiGrant::Read);
        alice.disabled = true;
        db.update_ui_user(&alice).unwrap();
        db.reconcile_ui_accounts(&[owner.clone(), declared.clone()])
            .unwrap();
        let alice = db.get_ui_user("alice").unwrap().unwrap();
        assert!(!alice.disabled);
        assert_eq!(alice.display_name, "Alice Changed");
        assert_eq!(alice.grants["app"], UiGrant::Write);
        db.reconcile_ui_accounts(std::slice::from_ref(&owner))
            .unwrap();
        let removed = db.get_ui_user("alice").unwrap().unwrap();
        assert!(removed.disabled);
        assert!(removed.grants.is_empty());
        assert!(
            db.ui_session_csrf(Some("alice-token"), "named", now)
                .unwrap()
                .is_none()
        );
        db.reconcile_ui_accounts(&[owner, declared]).unwrap();
        let alice = db.get_ui_user("alice").unwrap().unwrap();
        assert!(!alice.disabled);
        assert_eq!(alice.password_hash, "personal-hash");
        assert!(!alice.password_change_required);
        assert_eq!(alice.timezone, "Europe/Lisbon");
    }

    #[test]
    fn migrated_accounts_keep_utc_without_browser_initialization() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("accounts.sqlite");
        let legacy = rusqlite::Connection::open(&path).unwrap();
        for sql in &super::super::MIGRATIONS[..9] {
            legacy.execute_batch(sql).unwrap();
        }
        legacy
            .execute("INSERT INTO ui_users (id, display_name, password_hash) VALUES ('existing', 'Existing', 'hash')", [])
            .unwrap();
        legacy.pragma_update(None, "user_version", 9).unwrap();
        drop(legacy);

        let db = Db::open(&path).unwrap();
        let existing = db.get_ui_user("existing").unwrap().unwrap();
        assert_eq!(existing.timezone, "UTC");
        assert!(existing.timezone_initialized);
    }

    #[test]
    fn reconciliation_requires_a_declared_enabled_admin_and_rolls_back() {
        let mut db = Db::open_in_memory().unwrap();
        let declared = UiAccountDeclaration {
            id: "owner".into(),
            display_name: "Owner".into(),
            initial_password_hash: "initial-hash".into(),
            disabled: false,
            instance_admin: true,
            grants: BTreeMap::new(),
        };
        let mut no_admin = declared.clone();
        no_admin.instance_admin = false;
        assert!(matches!(
            db.reconcile_ui_accounts(&[no_admin]),
            Err(crate::StoreError::InvalidUiAccount(_))
        ));
        assert!(db.get_ui_user("owner").unwrap().is_none());
        db.reconcile_ui_accounts(&[declared]).unwrap();
        assert!(matches!(
            db.reconcile_ui_accounts(&[]),
            Err(crate::StoreError::InvalidUiAccount(_))
        ));
        assert!(!db.get_ui_user("owner").unwrap().unwrap().disabled);
    }

    #[test]
    fn existing_account_is_adopted_without_resetting_password() {
        let mut db = Db::open_in_memory().unwrap();
        db.create_ui_user(&user("owner", true)).unwrap();
        db.reconcile_ui_accounts(&[UiAccountDeclaration {
            id: "owner".into(),
            display_name: "Configured owner".into(),
            initial_password_hash: "new-initial".into(),
            disabled: false,
            instance_admin: true,
            grants: BTreeMap::new(),
        }])
        .unwrap();
        let owner = db.get_ui_user("owner").unwrap().unwrap();
        assert_eq!(owner.password_hash, "old-hash");
        assert_eq!(owner.display_name, "Configured owner");
        assert!(owner.managed_by_config);
    }
}
