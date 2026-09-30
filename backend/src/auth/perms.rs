use std::collections::HashMap;

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use serde::{Deserialize, Serialize};

use crate::error::AppError;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Permissions {
    pub system: bool,
    pub projects: HashMap<String, ProjectPerms>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProjectPerms {
    pub view: bool,
    pub control: bool,
    pub logs: bool,
    pub files: bool,
    pub git: bool,
}

impl ProjectPerms {
    pub const ALL: ProjectPerms = ProjectPerms {
        view: true,
        control: true,
        logs: true,
        files: true,
        git: true,
    };
}

impl Permissions {
    /// A malformed permissions column must lock the user out, never open up.
    pub fn parse_or_deny(raw: &str, username: &str) -> Permissions {
        match serde_json::from_str(raw) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(user = username, error = %e, "invalid permissions JSON — denying all");
                Permissions::default()
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Admin,
    User,
}

impl Role {
    pub fn from_db(s: &str) -> Role {
        if s == "admin" {
            Role::Admin
        } else {
            Role::User
        }
    }
}

/// The authenticated user for this request, loaded fresh from the database
/// by require_auth — permission edits, resets, and deletions therefore take
/// effect on the target's very next request.
#[derive(Clone)]
pub struct CurrentUser {
    pub username: String,
    pub role: Role,
    pub perms: Permissions,
}

impl CurrentUser {
    pub fn is_admin(&self) -> bool {
        self.role == Role::Admin
    }

    pub fn can_system(&self) -> bool {
        self.is_admin() || self.perms.system
    }

    pub fn project(&self, name: &str) -> ProjectPerms {
        if self.is_admin() {
            return ProjectPerms::ALL;
        }
        self.perms.projects.get(name).copied().unwrap_or_default()
    }

    pub fn require(&self, allowed: bool) -> Result<(), AppError> {
        if allowed {
            Ok(())
        } else {
            Err(AppError::Forbidden)
        }
    }
}

impl<S: Send + Sync> FromRequestParts<S> for CurrentUser {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<CurrentUser>()
            .cloned()
            .ok_or(AppError::Unauthorized)
    }
}
