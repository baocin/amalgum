//! Remote URLs and identities (§5.2, §5.10): normalise any git URL to `host/owner/repo`, derive
//! a stable repo id, build web/commit/permalink URLs for GitHub-, GitLab-, Bitbucket-, and
//! Gitea-style hosts (unknown hosts: `None`), and validate new ref names live (§5.9).

use crate::util;

/// `git@github.com:acme/conduit.git`, `ssh://git@github.com:22/acme/conduit`,
/// `https://user@github.com/acme/conduit.git/` → `github.com/acme/conduit`. Host lowercased,
/// port, user, scheme, `.git`, and trailing slashes dropped. Local paths and `file://` → `None`.
pub fn normalize(url: &str) -> Option<String> {
    let url = url.trim();
    if url.is_empty() || url.starts_with("file://") {
        return None;
    }

    let (host, path) = if let Some(rest) = url.strip_prefix("ssh://") {
        parse_scheme_authority(rest)?
    } else if let Some(rest) = url.strip_prefix("git://") {
        parse_scheme_authority(rest)?
    } else if let Some(rest) = url.strip_prefix("http://") {
        parse_scheme_authority(rest)?
    } else if let Some(rest) = url.strip_prefix("https://") {
        parse_scheme_authority(rest)?
    } else if url.contains("://") {
        // An unrecognised scheme (e.g. `ftp://`): not a git remote we know how to identify.
        return None;
    } else {
        parse_scp_like(url)?
    };

    let host = host.to_ascii_lowercase();
    let path = path.trim_start_matches('/');
    if path.is_empty() {
        return None;
    }

    let mut result = format!("{host}/{path}");
    trim_trailing_slashes(&mut result);
    if let Some(stripped) = result.strip_suffix(".git") {
        result.truncate(stripped.len());
        trim_trailing_slashes(&mut result);
    }
    if result.len() <= host.len() + 1 {
        return None; // nothing left after the host: not a repo path
    }
    Some(result)
}

fn trim_trailing_slashes(s: &mut String) {
    while s.ends_with('/') {
        s.pop();
    }
}

/// Splits `[user[:pass]@]host[:port]/path` (as found after a `scheme://`) into the host token
/// (brackets kept for an IPv6 literal) and the remaining path, which may be empty.
fn parse_scheme_authority(rest: &str) -> Option<(String, String)> {
    let authority_end = rest.find('/').unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    let path = &rest[authority_end..];

    let authority = match authority.rfind('@') {
        Some(i) => &authority[i + 1..],
        None => authority,
    };
    if authority.is_empty() {
        return None;
    }

    let host = if let Some(inner) = authority.strip_prefix('[') {
        let end = inner.find(']')?;
        &authority[..=end + 1]
    } else {
        let end = authority.find(':').unwrap_or(authority.len());
        &authority[..end]
    };
    if host.is_empty() {
        return None;
    }
    Some((host.to_string(), path.to_string()))
}

/// Splits scp-like `[user@]host:path` (only recognised when a `:` precedes the first `/`, per
/// `git`'s own disambiguation rule). A single-letter host is rejected so a Windows drive letter
/// like `C:\path` is never mistaken for a remote.
fn parse_scp_like(s: &str) -> Option<(String, String)> {
    if s.contains("://") {
        return None;
    }
    let rest = match s.find('@') {
        Some(i) => &s[i + 1..],
        None => s,
    };

    if let Some(inner) = rest.strip_prefix('[') {
        let end = inner.find(']')?;
        let host = &rest[..=end + 1];
        let path = rest[end + 2..].strip_prefix(':')?;
        if path.is_empty() {
            return None;
        }
        return Some((host.to_string(), path.to_string()));
    }

    let colon = rest.find(':')?;
    if let Some(slash) = rest.find('/')
        && slash < colon
    {
        return None; // a `/` before the `:` means this is a local path, not scp-like
    }
    let host = &rest[..colon];
    let path = &rest[colon + 1..];
    if host.is_empty() || path.is_empty() || host.chars().count() == 1 {
        return None;
    }
    Some((host.to_string(), path.to_string()))
}

/// Stable repo id: 16 lowercase hex chars of `util::fnv1a64` over the normalised primary remote
/// URL, or over the canonical local path when there is no remote. Keys journal, cache, state.
pub fn repo_id(primary_remote_url: Option<&str>, local_path: &str) -> String {
    let input = primary_remote_url.and_then(normalize).unwrap_or_else(|| local_path.to_string());
    format!("{:016x}", util::fnv1a64(input.as_bytes()))
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
    let host = normalized.split('/').next().unwrap_or("");
    if host.starts_with("github.") {
        Some(Forge::GitHub)
    } else if host.starts_with("gitlab.") {
        Some(Forge::GitLab)
    } else if host.starts_with("bitbucket.") {
        Some(Forge::Bitbucket)
    } else if host.starts_with("gitea.") || host == "codeberg.org" {
        Some(Forge::Gitea)
    } else {
        None
    }
}

/// `https://github.com/acme/conduit` for a recognised forge.
pub fn web_url(url: &str) -> Option<String> {
    let normalized = normalize(url)?;
    forge(&normalized)?;
    Some(format!("https://{normalized}"))
}

/// Commit page: GitHub/Gitea `/commit/<h>`, GitLab `/-/commit/<h>`, Bitbucket `/commits/<h>`.
pub fn commit_url(url: &str, hash: &str) -> Option<String> {
    let normalized = normalize(url)?;
    let suffix = match forge(&normalized)? {
        Forge::GitHub | Forge::Gitea => format!("/commit/{hash}"),
        Forge::GitLab => format!("/-/commit/{hash}"),
        Forge::Bitbucket => format!("/commits/{hash}"),
    };
    Some(format!("https://{normalized}{suffix}"))
}

/// File line permalink at a commit: GitHub `/blob/<h>/<path>#L<n>`, Gitea
/// `/src/commit/<h>/<path>#L<n>`, GitLab `/-/blob/<h>/<path>#L<n>`, Bitbucket `/src/<h>/<path>#lines-<n>`.
pub fn permalink(url: &str, hash: &str, path: &str, line: Option<u32>) -> Option<String> {
    let normalized = normalize(url)?;
    let f = forge(&normalized)?;
    let encoded_path = percent_encode_path(path);
    let (segment, anchor_prefix) = match f {
        Forge::GitHub => (format!("/blob/{hash}/{encoded_path}"), "L"),
        Forge::Gitea => (format!("/src/commit/{hash}/{encoded_path}"), "L"),
        Forge::GitLab => (format!("/-/blob/{hash}/{encoded_path}"), "L"),
        Forge::Bitbucket => (format!("/src/{hash}/{encoded_path}"), "lines-"),
    };
    let mut result = format!("https://{normalized}{segment}");
    if let Some(n) = line {
        result.push('#');
        result.push_str(anchor_prefix);
        result.push_str(&n.to_string());
    }
    Some(result)
}

/// Percent-encodes the unsafe characters that would otherwise break a permalink URL (space,
/// `#`, `?`), leaving `/` and everything else, including non-ASCII path characters, untouched.
fn percent_encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for ch in path.chars() {
        match ch {
            ' ' => out.push_str("%20"),
            '#' => out.push_str("%23"),
            '?' => out.push_str("%3F"),
            _ => out.push(ch),
        }
    }
    out
}

/// Branch-name rules of `git check-ref-format --branch`: no `..`, no ASCII control chars,
/// space, `~ ^ : ? * [ \`, no leading `-` or `/`, no trailing `/` or `.`, no `//`, no
/// component starting with `.` or ending with `.lock`, not `@`, no `@{`. `Err` names the rule.
pub fn validate_branch_name(name: &str) -> Result<(), &'static str> {
    if name.is_empty() {
        return Err("ref name may not be empty");
    }
    if name == "@" {
        return Err("ref name may not be the single character '@'");
    }
    if name.contains("@{") {
        return Err("ref name may not contain '@{'");
    }
    if name.contains("..") {
        return Err("ref name may not contain '..'");
    }
    if name.starts_with('/') || name.ends_with('/') {
        return Err("ref name may not begin or end with '/'");
    }
    if name.contains("//") {
        return Err("ref name may not contain consecutive slashes");
    }
    if name.starts_with('-') {
        return Err("ref name may not begin with '-'");
    }
    if name.ends_with('.') {
        return Err("ref name may not end with '.'");
    }
    if name.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return Err("ref name may not contain ASCII control characters");
    }
    if name.chars().any(|c| matches!(c, ' ' | '~' | '^' | ':' | '?' | '*' | '[' | '\\')) {
        return Err("ref name may not contain space, ~, ^, :, ?, *, [, or \\");
    }
    for component in name.split('/') {
        if component.starts_with('.') {
            return Err("no slash-separated component may begin with '.'");
        }
        if component.ends_with(".lock") {
            return Err("no slash-separated component may end with '.lock'");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn normalize_table() {
        let cases: &[(&str, Option<&str>)] = &[
            // scp-like
            ("git@github.com:acme/conduit.git", Some("github.com/acme/conduit")),
            ("github.com:acme/conduit.git", Some("github.com/acme/conduit")),
            ("GIT@GITHUB.COM:acme/conduit.git", Some("github.com/acme/conduit")),
            // ssh://
            ("ssh://git@github.com:22/acme/conduit", Some("github.com/acme/conduit")),
            ("ssh://github.com/acme/conduit.git", Some("github.com/acme/conduit")),
            // https:// / http:// / git://
            ("https://user@github.com/acme/conduit.git/", Some("github.com/acme/conduit")),
            ("https://user:pass@github.com/acme/conduit.git", Some("github.com/acme/conduit")),
            ("http://github.com/acme/conduit", Some("github.com/acme/conduit")),
            ("git://github.com/acme/conduit.git", Some("github.com/acme/conduit")),
            // host lowercased, path case preserved
            ("https://GitHub.COM/Acme/Conduit.git", Some("github.com/Acme/Conduit")),
            // GitLab subgroups
            ("git@gitlab.com:group/subgroup/project.git", Some("gitlab.com/group/subgroup/project")),
            ("https://gitlab.com/a/b/c.git", Some("gitlab.com/a/b/c")),
            // Bitbucket
            ("git@bitbucket.org:team/repo.git", Some("bitbucket.org/team/repo")),
            ("https://bitbucket.org/team/repo.git", Some("bitbucket.org/team/repo")),
            // Gitea / Codeberg
            ("git@gitea.example.com:org/repo.git", Some("gitea.example.com/org/repo")),
            ("https://codeberg.org/org/repo.git", Some("codeberg.org/org/repo")),
            // ports
            ("https://git.example.com:8443/acme/conduit.git", Some("git.example.com/acme/conduit")),
            // IPv4 / IPv6, with port
            ("ssh://deploy@192.168.1.10:2200/srv/repos/app.git", Some("192.168.1.10/srv/repos/app")),
            ("ssh://git@[::1]:2222/acme/conduit.git", Some("[::1]/acme/conduit")),
            ("git@[::1]:acme/conduit.git", Some("[::1]/acme/conduit")),
            // `~user` paths kept
            ("ssh://git@example.com/~alice/project.git", Some("example.com/~alice/project")),
            ("git@example.com:~alice/project.git", Some("example.com/~alice/project")),
            // trailing slash without .git
            ("https://github.com/acme/conduit/", Some("github.com/acme/conduit")),
            // no repo path at all
            ("https://github.com", None),
            ("https://github.com/", None),
            // local paths
            ("/home/user/project", None), // portability: allow
            ("./project", None),
            ("../project", None),
            // file://
            ("file:///home/user/project", None),
            // bare word
            ("origin", None),
            // Windows-y paths
            ("C:\\Users\\me\\project", None),
            ("C:/Users/me/project", None),
            // unsupported scheme
            ("ftp://example.com/project", None),
        ];
        for (input, expected) in cases {
            assert_eq!(normalize(input), expected.map(str::to_string), "input: {input}");
        }
    }

    #[test]
    fn repo_id_is_stable_and_transport_independent() {
        let ssh = repo_id(Some("git@github.com:acme/conduit.git"), "/x");
        let https = repo_id(Some("https://github.com/acme/conduit.git"), "/x");
        assert_eq!(ssh, https, "same repo via ssh and https must share an id");
        assert_eq!(ssh.len(), 16);
        assert!(ssh.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn repo_id_falls_back_to_local_path() {
        let a = repo_id(None, "/repos/a");
        let b = repo_id(None, "/repos/b");
        assert_ne!(a, b);
        // An unrecognisable remote URL falls back to the local path too, rather than colliding
        // every unparsable remote onto the same id.
        let unparsable = repo_id(Some("not a url"), "/repos/a");
        assert_eq!(unparsable, a);
    }

    #[test]
    fn forge_recognises_known_hosts() {
        assert_eq!(forge("github.com/acme/conduit"), Some(Forge::GitHub));
        assert_eq!(forge("gitlab.com/acme/conduit"), Some(Forge::GitLab));
        assert_eq!(forge("bitbucket.org/acme/conduit"), Some(Forge::Bitbucket));
        assert_eq!(forge("gitea.example.com/acme/conduit"), Some(Forge::Gitea));
        assert_eq!(forge("codeberg.org/acme/conduit"), Some(Forge::Gitea));
        assert_eq!(forge("example.com/acme/conduit"), None);
        assert_eq!(forge("raw.githubusercontent.com/acme/conduit"), None);
    }

    #[test]
    fn web_url_and_commit_url_and_permalink() {
        let url = "git@github.com:acme/conduit.git";
        assert_eq!(web_url(url).as_deref(), Some("https://github.com/acme/conduit"));
        assert_eq!(
            commit_url(url, "ab12cd3").as_deref(),
            Some("https://github.com/acme/conduit/commit/ab12cd3")
        );
        assert_eq!(
            permalink(url, "ab12cd3", "src/main.rs", Some(42)).as_deref(),
            Some("https://github.com/acme/conduit/blob/ab12cd3/src/main.rs#L42")
        );
        assert_eq!(
            permalink(url, "ab12cd3", "src/main.rs", None).as_deref(),
            Some("https://github.com/acme/conduit/blob/ab12cd3/src/main.rs")
        );

        let gitlab = "git@gitlab.com:acme/conduit.git";
        assert_eq!(
            commit_url(gitlab, "ab12cd3").as_deref(),
            Some("https://gitlab.com/acme/conduit/-/commit/ab12cd3")
        );
        assert_eq!(
            permalink(gitlab, "ab12cd3", "src/main.rs", Some(3)).as_deref(),
            Some("https://gitlab.com/acme/conduit/-/blob/ab12cd3/src/main.rs#L3")
        );

        let bitbucket = "git@bitbucket.org:acme/conduit.git";
        assert_eq!(
            commit_url(bitbucket, "ab12cd3").as_deref(),
            Some("https://bitbucket.org/acme/conduit/commits/ab12cd3")
        );
        assert_eq!(
            permalink(bitbucket, "ab12cd3", "src/main.rs", Some(3)).as_deref(),
            Some("https://bitbucket.org/acme/conduit/src/ab12cd3/src/main.rs#lines-3")
        );

        let gitea = "git@gitea.example.com:acme/conduit.git";
        assert_eq!(
            commit_url(gitea, "ab12cd3").as_deref(),
            Some("https://gitea.example.com/acme/conduit/commit/ab12cd3")
        );
        assert_eq!(
            permalink(gitea, "ab12cd3", "src/main.rs", Some(3)).as_deref(),
            Some("https://gitea.example.com/acme/conduit/src/commit/ab12cd3/src/main.rs#L3")
        );

        // Unknown host: nothing.
        let unknown = "git@example.com:acme/conduit.git";
        assert_eq!(web_url(unknown), None);
        assert_eq!(commit_url(unknown, "ab12cd3"), None);
        assert_eq!(permalink(unknown, "ab12cd3", "src/main.rs", None), None);
    }

    #[test]
    fn permalink_percent_encodes_path_but_keeps_slashes() {
        let url = "git@github.com:acme/conduit.git";
        assert_eq!(
            permalink(url, "ab12cd3", "a dir/file #1?.rs", None).as_deref(),
            Some("https://github.com/acme/conduit/blob/ab12cd3/a%20dir/file%20%231%3F.rs")
        );
    }

    #[test]
    fn validate_branch_name_table() {
        let cases: &[(&str, bool)] = &[
            ("main", true),
            ("feat/oauth", true),
            ("feat/oauth-2", true),
            ("under_score", true),
            ("CAPS", true),
            ("123", true),
            ("refs/heads/x", true),
            ("x.y.z", true),
            ("a./b", true),
            ("bad-", true),
            ("", false),
            ("-bad", false),
            ("bad.", false),
            ("bad..bad", false),
            ("bad//bad", false),
            ("/bad", false),
            ("bad/", false),
            ("ba d", false),
            ("ba~d", false),
            ("ba^d", false),
            ("ba:d", false),
            ("ba?d", false),
            ("ba*d", false),
            ("ba[d", false),
            ("ba\\d", false),
            ("@", false),
            ("ba@{d", false),
            (".bad", false),
            ("bad/.bad", false),
            ("bad.lock", false),
            ("bad/x.lock", false),
            ("feat/../x", false),
            ("a/b/.c", false),
            (".a/b", false),
            ("a/b.", false),
        ];
        for (name, expect_ok) in cases {
            assert_eq!(validate_branch_name(name).is_ok(), *expect_ok, "name: {name:?}");
        }
    }

    /// Cross-checks our rules against the real `git check-ref-format --branch` for a broad set
    /// of candidate names, so drift from actual `git` behaviour is caught.
    ///
    /// Two intentional disagreements, both because this function is pure (no repo access) and
    /// must judge a name for a *new* branch, not resolve an existing ref:
    /// - `@` alone: real `git --branch` accepts it as shorthand for "the current branch" (it
    ///   requires a repo to resolve); we reject it as too ambiguous a name to create.
    /// - the literal name `HEAD`: real `git --branch` special-cases and rejects exactly this
    ///   string (but not e.g. `feat/HEAD`); the contract's rule list does not call this out, so
    ///   we do not special-case it.
    #[test]
    fn validate_branch_name_agrees_with_real_git() {
        let candidates = [
            "main",
            "feat/oauth",
            "feat/oauth-2",
            "release/1.0",
            "under_score",
            "CAPS",
            "123",
            "a.b",
            "a./b",
            "bad-",
            "refs/heads/x",
            "x.y.z",
            "a-",
            "feat--x",
            "a/b/c",
            "",
            "-bad",
            "--",
            "-",
            "bad.",
            "bad..bad",
            "bad//bad",
            "/bad",
            "bad/",
            "/feat",
            "feat/",
            "ba d",
            "ba~d",
            "ba^d",
            "ba:d",
            "ba?d",
            "ba*d",
            "ba[d",
            "ba\\d",
            "ba@{d",
            "@{0}",
            "@{-1}",
            ".bad",
            "bad/.bad",
            "bad.lock",
            "bad/x.lock",
            "feat/../x",
            "a/b/.c",
            ".a/b",
            "a/b.",
            "end.with.dot.",
            "feat/日本語",
            "emoji-🎉",
        ];
        let known_disagreements = ["@", "HEAD"];

        for name in candidates {
            let real_ok = Command::new("git")
                .args(["check-ref-format", "--branch", name])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            let ours_ok = validate_branch_name(name).is_ok();
            assert_eq!(ours_ok, real_ok, "disagreement with real git for {name:?}");
        }

        for name in known_disagreements {
            let real_ok = Command::new("git")
                .args(["check-ref-format", "--branch", name])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            let ours_ok = validate_branch_name(name).is_ok();
            assert_ne!(ours_ok, real_ok, "expected a documented disagreement for {name:?}");
        }
    }
}
