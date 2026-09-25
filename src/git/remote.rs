//! Remote URLs and identities (§5.2, §5.10): normalise any git URL to `host/owner/repo`, derive
//! a stable repo id, build web/commit/permalink URLs for GitHub-, GitLab-, Bitbucket-, and
//! Gitea-style hosts (unknown hosts: `None`), and validate new ref names live (§5.9).

/// `git@github.com:acme/conduit.git`, `ssh://git@github.com:22/acme/conduit`,
/// `https://user@github.com/acme/conduit.git/` → `github.com/acme/conduit`. Host lowercased,
/// port, user, scheme, `.git`, and trailing slashes dropped. Local paths and `file://` → `None`.
pub fn normalize(url: &str) -> Option<String> {
    todo!()
}

/// Stable repo id: 16 lowercase hex chars of `util::fnv1a64` over the normalised primary remote
/// URL, or over the canonical local path when there is no remote. Keys journal, cache, state.
pub fn repo_id(primary_remote_url: Option<&str>, local_path: &str) -> String {
    todo!()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Forge {
    GitHub,
    GitLab,
    Bitbucket,
    Gitea,
}

/// Recognise the forge from the host name (`github.`, `gitlab.`, `bitbucket.`,
/// `gitea.`/`codeberg.org`).
pub fn forge(normalized: &str) -> Option<Forge> {
    todo!()
}

/// `https://github.com/acme/conduit` for a recognised forge.
pub fn web_url(url: &str) -> Option<String> {
    todo!()
}

/// Commit page: GitHub/Gitea `/commit/<h>`, GitLab `/-/commit/<h>`, Bitbucket `/commits/<h>`.
pub fn commit_url(url: &str, hash: &str) -> Option<String> {
    todo!()
}

/// File line permalink at a commit: GitHub `/blob/<h>/<path>#L<n>`, Gitea
/// `/src/commit/<h>/<path>#L<n>`, GitLab `/-/blob/<h>/<path>#L<n>`, Bitbucket `/src/<h>/<path>#lines-<n>`.
pub fn permalink(url: &str, hash: &str, path: &str, line: Option<u32>) -> Option<String> {
    todo!()
}

/// Branch-name rules of `git check-ref-format --branch`: no `..`, no ASCII control chars,
/// space, `~ ^ : ? * [ \`, no leading `-` or `/`, no trailing `/` or `.`, no `//`, no
/// component starting with `.` or ending with `.lock`, not `@`, no `@{`. `Err` names the rule.
pub fn validate_branch_name(name: &str) -> Result<(), &'static str> {
    todo!()
}
