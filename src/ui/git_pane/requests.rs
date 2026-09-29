//! Requests the git pane's menus hand to features that live outside it.
//!
//! [`SyncRequest`] is every network item of the commit context menu (§5.4 item 3), the Refs
//! remote rows (§5.10 **Fetch** / **Prune**), and the follow-ups of local operations that need
//! the network (create tag with **Push to <remote>**, delete branch / tag with **Also delete on
//! <remote>**). The pane never runs fetch / pull / push itself: it emits
//! `GitEvent::Sync(request)` and the sync feature (§5.10, `git::net`) runs it with its progress
//! pill, protected-branch rule, and journal entry. `remote: None` means the workspace's bound
//! remote, which only the app knows.

/// One network operation asked for from the git pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncRequest {
    /// `git fetch <remote> --prune --progress`; `None` = the bound remote.
    Fetch { remote: Option<String> },
    /// `git remote prune <remote>` (Refs → remote row → **Prune**).
    Prune { remote: String },
    /// **Pull** on the current branch row, with the split button's default mode.
    Pull,
    /// **Push `feat` to origin** / **Push to…** ▸ remote.
    PushBranch { branch: String, remote: Option<String> },
    /// **Force push with lease** (never plain `--force` from a menu, §5.10).
    ForcePushWithLease { branch: String, remote: Option<String> },
    /// §5.11 **Push to <bound remote>** after create, or **Push to…** on a tag.
    PushTag { tag: String, remote: Option<String> },
    /// §5.11 **Push all tags** (Refs → remote row): `git push <remote> --tags`.
    PushAllTags { remote: String },
    /// W13 **Also delete `origin/feat`** after a branch delete (`refs/heads/<branch>`).
    DeleteRemoteBranch { remote: String, branch: String },
    /// §5.11 **Also delete on <remote>** after a tag delete (`refs/tags/<tag>`).
    DeleteRemoteTag { remote: String, tag: String },
}

/// Every menu request is one of the sync feature's requests (§5.10); the menus keep their own
/// vocabulary so each item says exactly what it does, and this is the one translation.
impl From<SyncRequest> for super::SyncRequest {
    fn from(request: SyncRequest) -> Self {
        use super::SyncRequest as S;
        use super::sync::Force;
        match request {
            SyncRequest::Fetch { remote } => S::Fetch { remote, all: false, prune: true },
            SyncRequest::Prune { remote } => S::Fetch { remote: Some(remote), all: false, prune: true },
            SyncRequest::Pull => S::Pull { mode: None },
            SyncRequest::PushBranch { branch, remote } => {
                S::Push { remote, branch: Some(branch), force: Force::No }
            }
            SyncRequest::ForcePushWithLease { branch, remote } => {
                S::Push { remote, branch: Some(branch), force: Force::WithLease }
            }
            SyncRequest::PushTag { tag, remote } => S::PushTag { remote: remote.unwrap_or_default(), tag },
            SyncRequest::PushAllTags { remote } => S::PushAllTags { remote },
            SyncRequest::DeleteRemoteBranch { remote, branch } => {
                S::DeleteRemote { remote, refname: format!("refs/heads/{branch}") }
            }
            SyncRequest::DeleteRemoteTag { remote, tag } => {
                S::DeleteRemote { remote, refname: format!("refs/tags/{tag}") }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_requests_map_onto_sync_requests() {
        use crate::ui::git_pane::SyncRequest as S;
        use crate::ui::git_pane::sync::Force;
        assert_eq!(
            S::from(SyncRequest::ForcePushWithLease { branch: "feat".into(), remote: None }),
            S::Push { remote: None, branch: Some("feat".into()), force: Force::WithLease }
        );
        assert_eq!(
            S::from(SyncRequest::DeleteRemoteTag { remote: "origin".into(), tag: "v1".into() }),
            S::DeleteRemote { remote: "origin".into(), refname: "refs/tags/v1".into() }
        );
        assert_eq!(S::from(SyncRequest::Pull), S::Pull { mode: None });
    }
}
