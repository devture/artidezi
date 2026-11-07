// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
// SPDX-License-Identifier: Apache-2.0

//! Named browser users and the small, additive project permission model.

use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use argon2::{Algorithm, Argon2, Params, Version};
use artidezi_config::Config;
use artidezi_store::db::UiUser;

pub use artidezi_store::db::UiGrant as Grant;

const ARGON2_MEMORY_KIB: u32 = 19 * 1024;
const ARGON2_ITERATIONS: u32 = 2;
const ARGON2_PARALLELISM: u32 = 1;
/// A syntactically valid verifier used when a login names no enabled account.
pub(super) const DUMMY_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$AAAAAAAAAAAAAAAAAAAAAA$AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

#[derive(Clone)]
pub struct Access {
    pub id: String,
    pub display_name: String,
    pub timezone: String,
    pub timezone_initialized: bool,
    admin: bool,
    grants: std::collections::BTreeMap<String, Grant>,
}

impl Access {
    pub fn from_user(user: &UiUser) -> Self {
        Self {
            id: user.id.clone(),
            display_name: user.display_name.clone(),
            timezone: user.timezone.clone(),
            timezone_initialized: user.timezone_initialized,
            admin: user.instance_admin,
            grants: user.grants.clone(),
        }
    }

    pub fn is_admin(&self) -> bool {
        self.admin
    }

    pub fn can_read(&self, project: &str) -> bool {
        self.admin || self.grants.contains_key(project) || self.grants.contains_key("*")
    }

    pub fn can_write(&self, project: &str) -> bool {
        self.admin
            || self.grants.get(project) == Some(&Grant::Write)
            || self.grants.get("*") == Some(&Grant::Write)
    }

    pub fn actor(&self) -> String {
        format!("user:{} ({})", self.id, self.display_name)
    }

    pub fn grants(&self) -> &std::collections::BTreeMap<String, Grant> {
        &self.grants
    }
}

pub fn validate_user(user: &UiUser, config: &Config) -> Result<(), String> {
    let id = &user.id;
    if id.is_empty()
        || id.len() > 64
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        return Err(format!("invalid UI user ID: {id}"));
    }
    if user.display_name.trim().is_empty()
        || user.display_name.chars().count() > 128
        || user.display_name.chars().any(char::is_control)
    {
        return Err(format!("invalid display name for UI user {id}"));
    }
    if !artidezi_config::validate::valid_ui_initial_hash(&user.password_hash) {
        return Err(format!("UI user {id}: invalid password hash"));
    }
    if user.timezone.parse::<chrono_tz::Tz>().is_err() {
        return Err(format!("UI user {id}: invalid timezone"));
    }
    for project in user.grants.keys() {
        if project != "*" && !config.projects.contains_key(project) {
            return Err(format!("UI user {id} grants unknown project {project}"));
        }
    }
    Ok(())
}

pub fn hash_password(password: &str) -> Result<String, &'static str> {
    if password.chars().count() < 16 || password.len() > 1024 {
        return Err("UI password must contain at least 16 characters and at most 1024 bytes");
    }
    let mut salt = [0u8; 16];
    getrandom::fill(&mut salt).map_err(|_| "unable to generate password salt")?;
    let params = Params::new(
        ARGON2_MEMORY_KIB,
        ARGON2_ITERATIONS,
        ARGON2_PARALLELISM,
        Some(32),
    )
    .map_err(|_| "invalid Argon2id parameters")?;
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_with_salt(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|_| "unable to hash password")
}

pub fn verify_password(password: &str, hash: &str) -> bool {
    if password.len() > 1024 || !artidezi_config::validate::valid_ui_initial_hash(hash) {
        return false;
    }
    let Ok(parsed) = PasswordHash::new(hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_and_exact_grants_are_additive() {
        let user = UiUser {
            id: "a".into(),
            display_name: "A".into(),
            password_hash: String::new(),
            disabled: false,
            instance_admin: false,
            grants: std::collections::BTreeMap::from([
                ("*".into(), Grant::Read),
                ("app".into(), Grant::Write),
            ]),
            revision: 1,
            password_change_required: false,
            managed_by_config: false,
            timezone: "UTC".into(),
            timezone_initialized: true,
        };
        let access = Access::from_user(&user);
        assert!(access.can_read("other"));
        assert!(!access.can_write("other"));
        assert!(access.can_write("app"));
    }

    #[test]
    fn passwords_use_salted_slow_hashes_and_reject_wrong_credentials() {
        let hash = hash_password("a-long-example-password").unwrap();
        assert!(hash.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"));
        assert!(artidezi_config::validate::valid_ui_initial_hash(&hash));
        assert!(verify_password("a-long-example-password", &hash));
        assert!(!verify_password("another-example-password", &hash));
        assert!(!verify_password(
            "a-long-example-password",
            &hash.replace("m=19456", "m=1048576")
        ));
        assert!(artidezi_config::validate::valid_ui_initial_hash(DUMMY_HASH));
        assert!(!verify_password("a-long-example-password", DUMMY_HASH));
    }

    #[test]
    fn hashes_from_earlier_releases_still_verify() {
        let hash = "$argon2id$v=19$m=19456,t=2,p=1$BwcHBwcHBwcHBwcHBwcHBw$ZyqJ8FzTH2jpOxJjjcQ/uhvaI1QeluDzKaC0lPSiBMg";
        assert!(verify_password("a-long-example-password", hash));
    }

    #[test]
    fn account_validation_rejects_unknown_projects_and_bad_ids() {
        let config =
            artidezi_config::parse(include_str!("../../../config/tests/fixtures/valid.yaml"))
                .unwrap();
        let hash = DUMMY_HASH;
        let mut user = UiUser {
            id: "alice".into(),
            display_name: "Alice".into(),
            password_hash: hash.into(),
            disabled: false,
            instance_admin: true,
            grants: std::collections::BTreeMap::from([("missing".into(), Grant::Read)]),
            revision: 1,
            password_change_required: false,
            managed_by_config: false,
            timezone: "UTC".into(),
            timezone_initialized: true,
        };
        assert!(
            validate_user(&user, &config)
                .unwrap_err()
                .contains("unknown project")
        );
        user.id = "invalid name".into();
        assert!(
            validate_user(&user, &config)
                .unwrap_err()
                .contains("invalid UI user ID")
        );
    }
}
