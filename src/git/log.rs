//! Commit records for the graph (§5.4) and the details area (§5.5).
//!
//! The graph streams `git log` output: records are NUL-separated (`-z`), fields `%x1f`-separated,
//! decorations are full ref names (`--decorate=full`) so a local branch `origin/x` is never
//! confused with a remote-tracking branch. Callers split the stream on NUL and feed each record
//! to [`parse_record`] as it arrives.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decoration {
    /// Detached `HEAD`.
    Head,
    /// `HEAD -> refs/heads/<name>`: the current branch.
    CurrentBranch(String),
    Branch(String),
    /// `origin/main`.
    Remote(String),
    Tag(String),
    /// Stash ref or anything else, by full name.
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    pub id: String,
    pub parents: Vec<String>,
    pub author: String,
    pub email: String,
    /// Author time, Unix seconds.
    pub time: u64,
    pub committer: String,
    pub committer_email: String,
    pub commit_time: u64,
    pub refs: Vec<Decoration>,
    pub subject: String,
}

/// `%H %P %an %ae %at %cn %ce %ct %D %s`, `%x1f`-separated.
pub const LOG_FORMAT: &str = "%H%x1f%P%x1f%an%x1f%ae%x1f%at%x1f%cn%x1f%ce%x1f%ct%x1f%D%x1f%s";

/// `git log` argv for the graph: topo order, `-z`, full decorations, [`LOG_FORMAT`], plus
/// `extra` (revision ranges, `--all`, `-n`, paths).
///
/// `--no-show-signature` overrides a user's `log.showSignature=true`: without it, a signed
/// commit makes git write the signature-verification program's output to stdout ahead of the
/// record itself, which corrupts [`parse_record`]'s id. `--no-color` is the same defensive
/// override for `color.ui=always`.
pub fn log_args(extra: &[&str]) -> Vec<String> {
    let mut args = vec![
        "log".to_string(),
        "--topo-order".to_string(),
        "-z".to_string(),
        "--decorate=full".to_string(),
        "--no-show-signature".to_string(),
        "--no-color".to_string(),
        format!("--format={LOG_FORMAT}"),
    ];
    args.extend(extra.iter().map(|s| s.to_string()));
    args
}

/// Field separator emitted by [`LOG_FORMAT`] (`%x1f`).
const FIELD_SEP: char = '\u{1f}';

/// Parse one record (no trailing NUL). `None` if it is malformed.
///
/// Some git versions emit a leading `\n` between `-z` records; it is stripped before parsing.
/// Exactly 10 `%x1f`-separated fields are required, in [`LOG_FORMAT`] order. Bytes that are not
/// valid UTF-8 (e.g. Latin-1 author names from a commit with no `encoding` header, which git
/// never re-encodes) are replaced with U+FFFD rather than rejecting the whole record.
pub fn parse_record(rec: &[u8]) -> Option<Commit> {
    let rec = rec.strip_prefix(b"\n").unwrap_or(rec);
    let text = String::from_utf8_lossy(rec);
    let mut fields = text.split(FIELD_SEP);

    let id = fields.next()?;
    let parents_field = fields.next()?;
    let author = fields.next()?;
    let email = fields.next()?;
    let time = fields.next()?;
    let committer = fields.next()?;
    let committer_email = fields.next()?;
    let commit_time = fields.next()?;
    let refs_field = fields.next()?;
    let subject = fields.next()?;
    // Exactly 10 fields: a further one means the record has an extra separator.
    if fields.next().is_some() {
        return None;
    }
    if id.is_empty() {
        return None;
    }

    let parents = if parents_field.is_empty() {
        Vec::new()
    } else {
        parents_field.split(' ').map(str::to_string).collect()
    };

    Some(Commit {
        id: id.to_string(),
        parents,
        author: author.to_string(),
        email: email.to_string(),
        time: time.parse().ok()?,
        committer: committer.to_string(),
        committer_email: committer_email.to_string(),
        commit_time: commit_time.parse().ok()?,
        refs: parse_decorations(refs_field),
        subject: subject.to_string(),
    })
}

/// Parse `%D`-style decorations (`--decorate=full`): comma-space-separated ref names, e.g.
/// `HEAD -> refs/heads/main, refs/remotes/origin/main, tag: refs/tags/v1`.
fn parse_decorations(field: &str) -> Vec<Decoration> {
    if field.is_empty() {
        return Vec::new();
    }
    field.split(", ").map(parse_decoration).collect()
}

fn parse_decoration(s: &str) -> Decoration {
    if let Some(name) = s.strip_prefix("HEAD -> ") {
        return Decoration::CurrentBranch(name.strip_prefix("refs/heads/").unwrap_or(name).to_string());
    }
    if s == "HEAD" {
        return Decoration::Head;
    }
    if let Some(name) = s.strip_prefix("tag: ") {
        return Decoration::Tag(name.strip_prefix("refs/tags/").unwrap_or(name).to_string());
    }
    if let Some(name) = s.strip_prefix("refs/heads/") {
        return Decoration::Branch(name.to_string());
    }
    if let Some(name) = s.strip_prefix("refs/remotes/") {
        return Decoration::Remote(name.to_string());
    }
    Decoration::Other(s.to_string())
}

/// Parse a whole `-z` stream: records are NUL-separated, the last one may be followed by a
/// trailing NUL (or not); empty chunks and malformed records are skipped.
pub fn parse_log(out: &[u8]) -> Vec<Commit> {
    out.split(|&b| b == 0).filter(|rec| !rec.is_empty()).filter_map(parse_record).collect()
}

/// `git show` argv printing commit `rev`'s full message for [`split_message`] (`HEAD` for the
/// amend editor's prefill). `--no-show-signature` as in [`log_args`]: a signed commit's
/// verification output would otherwise precede the message. The trailing `--` marks `rev` as a
/// revision: without it git refuses one that is also a worktree path (a file named `HEAD`, or
/// `head` on a case-insensitive filesystem) as ambiguous.
pub fn message_args(rev: &str) -> [&str; 6] {
    ["show", "-s", "--no-show-signature", "--format=%B", rev, "--"]
}

/// Full message for the details area: [`message_args`] output, split into the subject (first
/// line) and body (rest, leading blank lines trimmed, trailing whitespace trimmed). CRLF line
/// endings are tolerated.
pub fn split_message(full: &str) -> (String, String) {
    let full = full.replace("\r\n", "\n");
    let mut parts = full.splitn(2, '\n');
    let subject = parts.next().unwrap_or("").trim_end().to_string();
    let rest = parts.next().unwrap_or("");

    let lines: Vec<&str> = rest.lines().collect();
    let start = lines.iter().position(|l| !l.trim().is_empty()).unwrap_or(lines.len());
    let body = lines[start..].join("\n").trim_end().to_string();

    (subject, body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempRepo;

    /// Builds one `%x1f`-separated record with the given fields, in [`LOG_FORMAT`] order.
    #[allow(clippy::too_many_arguments)]
    fn record(
        id: &str,
        parents: &str,
        an: &str,
        ae: &str,
        at: &str,
        cn: &str,
        ce: &str,
        ct: &str,
        refs: &str,
        subject: &str,
    ) -> Vec<u8> {
        [id, parents, an, ae, at, cn, ce, ct, refs, subject].join("\u{1f}").into_bytes()
    }

    fn args_to_str(v: &[String]) -> Vec<&str> {
        v.iter().map(String::as_str).collect()
    }

    // ---- log_args ----

    #[test]
    fn log_args_base() {
        let args = log_args(&[]);
        assert_eq!(
            args_to_str(&args),
            vec![
                "log",
                "--topo-order",
                "-z",
                "--decorate=full",
                "--no-show-signature",
                "--no-color",
                "--format=%H%x1f%P%x1f%an%x1f%ae%x1f%at%x1f%cn%x1f%ce%x1f%ct%x1f%D%x1f%s"
            ]
        );
    }

    #[test]
    fn log_args_appends_extra_after_format() {
        let args = log_args(&["--all", "-n", "500"]);
        assert_eq!(args_to_str(&args)[7..], ["--all", "-n", "500"]);
        assert_eq!(args.len(), 10);
    }

    #[test]
    fn log_args_disables_show_signature() {
        // Regression: without `--no-show-signature`, a user's `log.showSignature=true` makes a
        // signed commit's gpg-verification chatter land in stdout ahead of the record.
        assert!(args_to_str(&log_args(&[])).contains(&"--no-show-signature"));
    }

    // ---- parse_record: well-formed ----

    #[test]
    fn parse_record_root_commit_no_parents() {
        let rec = record("aaa", "", "Ada", "a@x.com", "100", "Ada", "a@x.com", "100", "", "init");
        let c = parse_record(&rec).expect("valid record");
        assert_eq!(c.id, "aaa");
        assert!(c.parents.is_empty());
        assert_eq!(c.author, "Ada");
        assert_eq!(c.email, "a@x.com");
        assert_eq!(c.time, 100);
        assert_eq!(c.committer, "Ada");
        assert_eq!(c.committer_email, "a@x.com");
        assert_eq!(c.commit_time, 100);
        assert!(c.refs.is_empty());
        assert_eq!(c.subject, "init");
    }

    #[test]
    fn parse_record_merge_commit_two_parents() {
        let rec = record("m", "p1 p2", "A", "a@x", "1", "C", "c@x", "2", "", "merge");
        let c = parse_record(&rec).expect("valid record");
        assert_eq!(c.parents, vec!["p1".to_string(), "p2".to_string()]);
        assert_eq!(c.time, 1);
        assert_eq!(c.commit_time, 2);
    }

    #[test]
    fn parse_record_octopus_merge_many_parents() {
        let rec = record("m", "p1 p2 p3 p4", "A", "a@x", "1", "C", "c@x", "2", "", "octopus");
        let c = parse_record(&rec).expect("valid record");
        assert_eq!(c.parents, vec!["p1", "p2", "p3", "p4"]);
    }

    #[test]
    fn parse_record_subject_may_contain_any_char() {
        let rec = record("a", "", "A", "a@x", "1", "A", "a@x", "1", "", "fix: a/b (c) #1 — done!");
        let c = parse_record(&rec).expect("valid record");
        assert_eq!(c.subject, "fix: a/b (c) #1 — done!");
    }

    #[test]
    fn parse_record_tolerates_leading_newline_between_z_records() {
        let plain = record("a", "", "A", "a@x", "1", "A", "a@x", "1", "", "s");
        let mut with_nl = b"\n".to_vec();
        with_nl.extend_from_slice(&plain);
        assert_eq!(parse_record(&with_nl), parse_record(&plain));
        assert!(parse_record(&with_nl).is_some());
    }

    // ---- parse_record: decorations ----

    #[test]
    fn decoration_current_branch() {
        let rec = record("a", "", "A", "a@x", "1", "A", "a@x", "1", "HEAD -> refs/heads/main", "s");
        let c = parse_record(&rec).unwrap();
        assert_eq!(c.refs, vec![Decoration::CurrentBranch("main".to_string())]);
    }

    #[test]
    fn decoration_bare_head_detached() {
        let rec = record("a", "", "A", "a@x", "1", "A", "a@x", "1", "HEAD", "s");
        let c = parse_record(&rec).unwrap();
        assert_eq!(c.refs, vec![Decoration::Head]);
    }

    #[test]
    fn decoration_local_branch() {
        let rec = record("a", "", "A", "a@x", "1", "A", "a@x", "1", "refs/heads/feat/x", "s");
        let c = parse_record(&rec).unwrap();
        assert_eq!(c.refs, vec![Decoration::Branch("feat/x".to_string())]);
    }

    #[test]
    fn decoration_remote_branch() {
        let rec = record("a", "", "A", "a@x", "1", "A", "a@x", "1", "refs/remotes/origin/main", "s");
        let c = parse_record(&rec).unwrap();
        assert_eq!(c.refs, vec![Decoration::Remote("origin/main".to_string())]);
    }

    #[test]
    fn decoration_tag() {
        let rec = record("a", "", "A", "a@x", "1", "A", "a@x", "1", "tag: refs/tags/v1", "s");
        let c = parse_record(&rec).unwrap();
        assert_eq!(c.refs, vec![Decoration::Tag("v1".to_string())]);
    }

    #[test]
    fn decoration_other_by_full_name() {
        let rec = record("a", "", "A", "a@x", "1", "A", "a@x", "1", "refs/stash", "s");
        let c = parse_record(&rec).unwrap();
        assert_eq!(c.refs, vec![Decoration::Other("refs/stash".to_string())]);
    }

    #[test]
    fn decoration_local_branch_named_like_a_remote_is_not_confused() {
        // A local branch literally called "origin/x" must decode as Branch, not Remote,
        // because --decorate=full always spells out refs/heads/ vs refs/remotes/.
        let rec = record("a", "", "A", "a@x", "1", "A", "a@x", "1", "refs/heads/origin/x", "s");
        let c = parse_record(&rec).unwrap();
        assert_eq!(c.refs, vec![Decoration::Branch("origin/x".to_string())]);
    }

    #[test]
    fn decoration_multiple_comma_separated() {
        let rec = record(
            "a",
            "",
            "A",
            "a@x",
            "1",
            "A",
            "a@x",
            "1",
            "HEAD -> refs/heads/main, tag: refs/tags/v1, refs/remotes/origin/main",
            "s",
        );
        let c = parse_record(&rec).unwrap();
        assert_eq!(
            c.refs,
            vec![
                Decoration::CurrentBranch("main".to_string()),
                Decoration::Tag("v1".to_string()),
                Decoration::Remote("origin/main".to_string()),
            ]
        );
    }

    // ---- parse_record: malformed input never panics ----

    #[test]
    fn parse_record_malformed_inputs_return_none() {
        let cases: Vec<Vec<u8>> = vec![
            b"".to_vec(),
            b"only-a-hash".to_vec(),
            record("", "", "A", "a@x", "1", "A", "a@x", "1", "", "s"), // empty id
            record("a", "", "A", "a@x", "not-a-number", "A", "a@x", "1", "", "s"), // bad time
            record("a", "", "A", "a@x", "1", "A", "a@x", "not-a-number", "", "s"), // bad commit_time
            // one field short (9 instead of 10, missing subject)
            ["a", "", "A", "a@x", "1", "A", "a@x", "1", "s"].join("\u{1f}").into_bytes(),
            {
                // one field too many
                let mut v = record("a", "", "A", "a@x", "1", "A", "a@x", "1", "", "s");
                v.push(0x1f);
                v.extend_from_slice(b"extra");
                v
            },
            vec![0xff, 0xfe, 0x1f, 0x00, 0x1f], // invalid UTF-8 is lossy-decoded now, but still short on fields
            b"\x1f\x1f\x1f\x1f\x1f\x1f\x1f\x1f\x1f".to_vec(), // all-empty except id (empty id)
            b"a\x1f".to_vec(),                  // truncated after one separator
            b"a\x1fb\x1fc\x1fd\x1fe\x1ff\x1fg\x1fh\x1fi\x1fj\x1fk".to_vec(), // 11 fields
        ];
        for (i, rec) in cases.into_iter().enumerate() {
            assert_eq!(parse_record(&rec), None, "case {i} should be malformed: {rec:?}");
        }
    }

    #[test]
    fn parse_record_nine_fields_is_malformed() {
        // LOG_FORMAT has 10 fields; only 9 here (missing subject).
        let rec = ["a", "", "A", "a@x", "1", "A", "a@x", "1", ""].join("\u{1f}").into_bytes();
        assert_eq!(parse_record(&rec), None);
    }

    // ---- parse_log ----

    #[test]
    fn parse_log_splits_on_nul_and_skips_trailing_empty() {
        let r1 = record("a", "", "A", "a@x", "1", "A", "a@x", "1", "", "one");
        let r2 = record("b", "a", "A", "a@x", "2", "A", "a@x", "2", "", "two");
        let mut stream = r1.clone();
        stream.push(0);
        stream.extend_from_slice(&r2);
        stream.push(0); // trailing NUL like real `-z` output
        let commits = parse_log(&stream);
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].id, "a");
        assert_eq!(commits[1].id, "b");
    }

    #[test]
    fn parse_log_skips_malformed_records_without_panicking() {
        let good = record("a", "", "A", "a@x", "1", "A", "a@x", "1", "", "one");
        let mut stream = b"garbage-not-enough-fields".to_vec();
        stream.push(0);
        stream.extend_from_slice(&good);
        stream.push(0);
        let commits = parse_log(&stream);
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].id, "a");
    }

    #[test]
    fn parse_log_empty_stream() {
        assert_eq!(parse_log(b""), vec![]);
        assert_eq!(parse_log(b"\0"), vec![]);
    }

    // ---- parse_log / log_args against a real TempRepo ----

    #[test]
    fn parse_log_real_repo_decorations_and_shape() {
        let mut repo = TempRepo::new();
        let c1 = repo.commit_file("a.txt", "a", "first");
        let _c2 = repo.commit_file("b.txt", "b", "second");
        repo.git(&["branch", "feature", &c1]);
        repo.git(&["tag", "v1"]);
        repo.git(&["update-ref", "refs/remotes/origin/main", "HEAD"]);
        repo.git(&["checkout", "-q", "-b", "origin/x", &c1]);
        let c3 = repo.commit_file("c.txt", "c", "third on origin/x");
        repo.git(&["checkout", "-q", "main"]);

        let args = log_args(&["--all"]);
        let out = repo.git_raw(&args_to_str(&args));
        let commits = parse_log(&out);

        assert_eq!(commits.len(), 3);
        assert_eq!(commits[0].id, c3);
        assert_eq!(commits[0].refs, vec![Decoration::Branch("origin/x".to_string())]);
        assert_eq!(commits[0].parents, vec![c1.clone()]);

        let second = commits.iter().find(|c| c.subject == "second").expect("second commit");
        assert!(second.refs.contains(&Decoration::CurrentBranch("main".to_string())));
        assert!(second.refs.contains(&Decoration::Tag("v1".to_string())));
        assert!(second.refs.contains(&Decoration::Remote("origin/main".to_string())));

        let first = commits.iter().find(|c| c.id == c1).expect("first commit");
        assert_eq!(first.refs, vec![Decoration::Branch("feature".to_string())]);
        assert!(first.parents.is_empty(), "root commit has no parents");
    }

    #[test]
    fn parse_log_real_repo_merge_commit() {
        let mut repo = TempRepo::new();
        repo.commit_file("a.txt", "a", "first");
        repo.git(&["checkout", "-q", "-b", "topic"]);
        let on_topic = repo.commit_file("b.txt", "b", "on topic");
        repo.git(&["checkout", "-q", "main"]);
        let on_main = repo.commit_file("c.txt", "c", "on main");
        let merge = {
            repo.git(&["merge", "-q", "--no-ff", "-m", "merge topic", "topic"]);
            repo.git(&["rev-parse", "HEAD"])
        };

        let args = log_args(&[]);
        let out = repo.git_raw(&args_to_str(&args));
        let commits = parse_log(&out);

        let m = commits.iter().find(|c| c.id == merge).expect("merge commit");
        assert_eq!(m.parents, vec![on_main, on_topic]);
    }

    /// Makes `main` a commit with a (fake) `gpgsig` header and message `signed subject` + a body,
    /// and writes a stub `gpg.program` that "verifies" it. Returns the commit's hash and the
    /// `-c` value selecting the stub. The header just needs to look enough like a signature
    /// that git invokes the program — it never checks whether it's really valid — so no key
    /// or real GPG is needed.
    fn signed_commit_with_stub_gpg(repo: &TempRepo) -> (String, String) {
        let tree = repo.git(&["write-tree"]);
        let commit_text = format!(
            "tree {tree}\n\
             author Ada Tester <ada@example.com> 1700000000 +0000\n\
             committer Ada Tester <ada@example.com> 1700000000 +0000\n\
             gpgsig -----BEGIN PGP SIGNATURE-----\n\
             \x20\n\
             \x20garbagebase64data==\n\
             \x20-----END PGP SIGNATURE-----\n\
             \n\
             signed subject\n\
             \n\
             signed body\n"
        );
        repo.write("commit-msg.txt", &commit_text);
        let hash = repo.git(&["hash-object", "-t", "commit", "-w", "commit-msg.txt"]);
        repo.git(&["update-ref", "refs/heads/main", &hash]);

        let stub_path = repo.write(
            "stub-gpg.sh",
            "#!/bin/sh\n\
             echo 'gpg: Signature made Thu Jan  1 00:00:00 1970 UTC' >&2\n\
             echo 'gpg: Good signature from \"Test User <test@example.com>\"' >&2\n\
             exit 0\n",
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&stub_path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        (hash, format!("gpg.program={}", stub_path.display()))
    }

    #[test]
    fn log_args_show_signature_does_not_pollute_commit_id() {
        // Regression for a user with `log.showSignature=true`: without `--no-show-signature`,
        // git writes the configured `gpg.program`'s verification chatter to stdout right before
        // the record, and it has no separator from the id field.
        let repo = TempRepo::new();
        let (hash, gpg_program_cfg) = signed_commit_with_stub_gpg(&repo);
        let mut args: Vec<&str> = vec!["-c", "log.showSignature=true", "-c", &gpg_program_cfg];
        let fixed_args = log_args(&[]);
        args.extend(fixed_args.iter().map(String::as_str));

        let out = repo.git_raw(&args);
        let commits = parse_log(&out);

        assert_eq!(commits.len(), 1, "exactly one commit, not split by the gpg chatter");
        assert_eq!(commits[0].id, hash, "id must be the real hash, not text glued on by --show-signature");
    }

    #[test]
    fn parse_log_tolerates_non_utf8_commit_metadata_from_real_git() {
        // Regression: a commit object with no `encoding` header carries its raw bytes as-is
        // (git assumes UTF-8 and never re-encodes), so a legacy CVS/SVN import can have Latin-1
        // bytes in the author name. Such a commit must survive `parse_log`, not vanish from it.
        let repo = TempRepo::new();
        let tree = repo.git(&["write-tree"]);

        let mut commit_bytes = Vec::new();
        commit_bytes.extend_from_slice(format!("tree {tree}\nauthor Jos").as_bytes());
        commit_bytes.push(0xE9); // raw Latin-1 'é', not a valid UTF-8 continuation of "Jos"
        commit_bytes.extend_from_slice(b" <jose@example.com> 1700000000 +0000\ncommitter Jos");
        commit_bytes.push(0xE9);
        commit_bytes.extend_from_slice(b" <jose@example.com> 1700000000 +0000\n\nlegacy import\n");
        std::fs::write(repo.path().join("commit-msg.bin"), &commit_bytes).expect("write");

        let hash = repo.git(&["hash-object", "-t", "commit", "-w", "commit-msg.bin"]);
        repo.git(&["update-ref", "refs/heads/main", &hash]);

        let args = log_args(&[]);
        let out = repo.git_raw(&args_to_str(&args));
        let commits = parse_log(&out);

        assert_eq!(commits.len(), 1, "the commit must not be dropped for having non-UTF-8 metadata");
        assert_eq!(commits[0].id, hash);
        assert_eq!(commits[0].author, "Jos\u{FFFD}", "the bad byte becomes U+FFFD, not a rejection");
    }

    // ---- split_message ----

    #[test]
    fn split_message_subject_only() {
        assert_eq!(split_message("Fix the thing"), ("Fix the thing".to_string(), String::new()));
    }

    #[test]
    fn split_message_subject_with_trailing_newline_no_body() {
        assert_eq!(split_message("Fix the thing\n"), ("Fix the thing".to_string(), String::new()));
    }

    #[test]
    fn split_message_subject_and_body_with_blank_line() {
        let full = "Subject line\n\nBody line one\nBody line two\n";
        assert_eq!(
            split_message(full),
            ("Subject line".to_string(), "Body line one\nBody line two".to_string())
        );
    }

    #[test]
    fn split_message_multiple_leading_blank_lines_trimmed() {
        let full = "Subject\n\n\n  \nBody\n";
        assert_eq!(split_message(full), ("Subject".to_string(), "Body".to_string()));
    }

    #[test]
    fn split_message_trailing_whitespace_trimmed() {
        let full = "Subject\n\nBody\n\n\n   ";
        assert_eq!(split_message(full), ("Subject".to_string(), "Body".to_string()));
    }

    #[test]
    fn split_message_preserves_indentation_after_leading_blank_lines() {
        let full = "Subject\n\n  - item one\n  - item two\n";
        assert_eq!(split_message(full), ("Subject".to_string(), "  - item one\n  - item two".to_string()));
    }

    #[test]
    fn split_message_crlf_tolerant() {
        let full = "Subject\r\n\r\nBody one\r\nBody two\r\n";
        assert_eq!(split_message(full), ("Subject".to_string(), "Body one\nBody two".to_string()));
    }

    #[test]
    fn split_message_subject_trailing_whitespace_trimmed() {
        assert_eq!(split_message("Subject   \nBody"), ("Subject".to_string(), "Body".to_string()));
    }

    #[test]
    fn split_message_empty_input() {
        assert_eq!(split_message(""), (String::new(), String::new()));
    }

    #[test]
    fn message_args_show_signature_does_not_pollute_the_message() {
        // Regression for a user with `log.showSignature=true` who signs their own commits: the
        // verification chatter landed ahead of `%B`, so it became the amend editor's subject
        // (and then the amended commit's) and the start of the details body.
        let repo = TempRepo::new();
        let (hash, gpg_program_cfg) = signed_commit_with_stub_gpg(&repo);
        for rev in ["HEAD", hash.as_str()] {
            let mut args: Vec<&str> = vec!["-c", "log.showSignature=true", "-c", &gpg_program_cfg];
            args.extend_from_slice(&message_args(rev));
            let full = String::from_utf8_lossy(&repo.git_raw(&args)).into_owned();
            assert_eq!(
                split_message(&full),
                ("signed subject".to_string(), "signed body".to_string()),
                "{rev}"
            );
        }
    }

    #[test]
    fn message_args_rev_is_not_taken_for_a_file_of_the_same_name() {
        // Regression: with a revision argument (unlike the `git log -1` it replaced), `git show`
        // refuses a name that is both a revision and a worktree file: "ambiguous argument
        // 'HEAD': both revision and filename". That broke the amend prefill.
        let mut repo = TempRepo::new();
        let hash = repo.commit_file("a.txt", "a", "Subject\n\nBody");
        repo.write("HEAD", "not a revision");
        repo.write(&hash, "not a revision either");
        for rev in ["HEAD", hash.as_str()] {
            let full = String::from_utf8_lossy(&repo.git_raw(&message_args(rev))).into_owned();
            assert_eq!(split_message(&full), ("Subject".to_string(), "Body".to_string()), "{rev}");
        }
    }

    #[test]
    fn split_message_real_git_multiline() {
        let mut repo = TempRepo::new();
        let hash = repo.commit_file("a.txt", "a", "Short subject\n\nLong body\nwith two lines.");
        let full = repo.git(&["show", "-s", "--format=%B", &hash]);
        let (subject, body) = split_message(&full);
        assert_eq!(subject, "Short subject");
        assert_eq!(body, "Long body\nwith two lines.");
    }
}
