use serde::{Deserialize, Serialize};

/// The three roles a workspace knows about.
///
/// Every capability check in the app is a method on this type, so the server
/// handlers and the UI ask the same question of the same source.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// First account in the workspace: owns the sender, the limits, the member
    /// list, the mail rules and the read-only links.
    Admin,
    /// Works the board and is mailed by the rules.
    Member,
    /// Reads and exports. Never assigned, never mailed, cannot comment.
    Viewer,
}

impl Role {
    /// Can this role change tasks — create, move, edit, delete?
    pub fn can_write_tasks(self) -> bool {
        matches!(self, Role::Admin | Role::Member)
    }

    /// Can a task be assigned to this role?
    pub fn can_be_assigned(self) -> bool {
        matches!(self, Role::Admin | Role::Member)
    }

    /// Can this role write comments?
    pub fn can_comment(self) -> bool {
        matches!(self, Role::Admin | Role::Member)
    }

    /// Does a mail rule ever address this role?
    pub fn is_mailable(self) -> bool {
        matches!(self, Role::Admin | Role::Member)
    }

    /// Sender, limits, member list, mail rules, read-only links.
    pub fn can_administer(self) -> bool {
        matches!(self, Role::Admin)
    }
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Admin => "admin",
            Role::Member => "member",
            Role::Viewer => "viewer",
        }
    }

    /// The inverse of [`Role::as_str`], for values read back out of the store.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "admin" => Some(Role::Admin),
            "member" => Some(Role::Member),
            "viewer" => Some(Role::Viewer),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Role;

    #[test]
    fn viewer_is_read_only_and_never_mailed() {
        assert!(!Role::Viewer.can_write_tasks());
        assert!(!Role::Viewer.can_be_assigned());
        assert!(!Role::Viewer.can_comment());
        assert!(!Role::Viewer.is_mailable());
        assert!(!Role::Viewer.can_administer());
    }

    #[test]
    fn only_admin_administers() {
        assert!(Role::Admin.can_administer());
        assert!(!Role::Member.can_administer());
    }

    #[test]
    fn member_works_the_board() {
        assert!(Role::Member.can_write_tasks());
        assert!(Role::Member.can_be_assigned());
        assert!(Role::Member.can_comment());
        assert!(Role::Member.is_mailable());
    }
}
