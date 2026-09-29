//! The clone sheet's form (§5.3), headless: URL (auto-filled from the clipboard), destination
//! folder, auto-derived folder name, shallow depth, **Open as workspace**, and **Clone on remote
//! host**. Live validation per field and the resolved request the UI hands to `git::net::clone`
//! on a worker. The sheet itself is `ui::clone`.

use crate::git::Location;
use crate::git::net;
use std::path::{Path, PathBuf};

/// Remote destinations start in the host's home directory.
pub const REMOTE_DEFAULT_DIR: &str = "~";

/// The sheet's live state. Edit the fields directly, then call the matching `*_changed` so the
/// derived values (folder name, default destination) follow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloneForm {
    pub url: String,
    /// The destination's parent folder: local (`~` expands to the home directory), or a path on
    /// [`CloneForm::host`] when one is chosen.
    pub dest: String,
    /// The new folder inside `dest`; derived from the URL until the user edits it.
    pub name: String,
    pub shallow: bool,
    pub depth: String,
    pub open_as_workspace: bool,
    /// **Clone on remote host**: an index into `hosts`, `None` = off.
    pub host: Option<usize>,
    /// Concrete `Host` aliases from `~/.ssh/config`.
    pub hosts: Vec<String>,
    /// Settings → **Default clone directory**, restored when switching back to local.
    local_default: String,
    name_edited: bool,
    dest_edited: bool,
}

/// What a valid form asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloneRequest {
    pub url: String,
    /// The folder the clone runs in (git is bound here, local or on the host).
    pub parent: Location,
    pub name: String,
    pub depth: Option<u32>,
    pub open_as_workspace: bool,
}

impl CloneRequest {
    /// The progress pill's label: "Cloning conduit".
    pub fn label(&self) -> String {
        format!("Cloning {}", self.name)
    }
}

impl CloneForm {
    /// A fresh sheet: destination from Settings → **Default clone directory**, shallow off with
    /// depth 1 ready, **Open as workspace** on (§5.3).
    pub fn new(default_dir: &str) -> Self {
        Self {
            url: String::new(),
            dest: default_dir.to_string(),
            name: String::new(),
            shallow: false,
            depth: "1".to_string(),
            open_as_workspace: true,
            host: None,
            hosts: Vec::new(),
            local_default: default_dir.to_string(),
            name_edited: false,
            dest_edited: false,
        }
    }

    /// Call after `url` changed: re-derive the folder name unless the user typed their own.
    pub fn url_changed(&mut self) {
        if !self.name_edited {
            self.name = net::clone_folder_name(&self.url).unwrap_or_default();
        }
    }

    /// Call after the user edited `name`. Clearing it hands the name back to the URL.
    pub fn name_changed(&mut self) {
        self.name_edited = !self.name.trim().is_empty();
        if !self.name_edited {
            self.url_changed();
        }
    }

    /// Call after the user edited `dest`: it no longer follows the local/remote default.
    pub fn dest_changed(&mut self) {
        self.dest_edited = true;
    }

    /// Call after `host` changed. An untouched destination switches between the local default
    /// and the host's home; a typed one is kept.
    pub fn host_changed(&mut self) {
        if self.host.is_some_and(|i| i >= self.hosts.len()) {
            self.host = None;
        }
        if !self.dest_edited {
            self.dest = match self.host {
                Some(_) => REMOTE_DEFAULT_DIR.to_string(),
                None => self.local_default.clone(),
            };
        }
    }

    /// Clipboard text arrived (read on a worker when the sheet opened): fill an empty URL when
    /// the text looks like a git URL (§5.3). Returns whether it did.
    pub fn offer_clipboard(&mut self, text: &str) -> bool {
        if !self.url.trim().is_empty() || !net::looks_like_git_url(text) {
            return false;
        }
        self.url = text.trim().to_string();
        self.url_changed();
        true
    }

    /// The ssh config's hosts arrived. Keeps the chosen host by name if it is still listed.
    pub fn set_hosts(&mut self, hosts: Vec<String>) {
        let chosen = self.host_name().map(str::to_string);
        self.hosts = hosts;
        self.host = chosen.and_then(|h| self.hosts.iter().position(|x| *x == h));
        self.host_changed();
    }

    pub fn host_name(&self) -> Option<&str> {
        self.host.and_then(|i| self.hosts.get(i)).map(String::as_str)
    }

    pub fn url_error(&self) -> Option<&'static str> {
        let url = self.url.trim();
        if url.is_empty() {
            Some("Enter a repository URL")
        } else if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
            Some("A URL can't contain spaces")
        } else {
            None
        }
    }

    pub fn dest_error(&self, home: Option<&Path>) -> Option<&'static str> {
        let dest = self.dest.trim();
        if dest.is_empty() {
            return Some("Choose a destination folder");
        }
        if dest.chars().any(char::is_control) {
            return Some("A folder can't contain control characters");
        }
        if self.host.is_some() {
            return None;
        }
        match expand_local(dest, home) {
            Some(path) if path.is_absolute() => None,
            _ => Some("Use an absolute path or one starting with ~/"),
        }
    }

    pub fn name_error(&self) -> Option<&'static str> {
        let name = self.name.trim();
        if name.is_empty() {
            Some("Enter a folder name")
        } else if name == "." || name == ".." || name.contains('/') || name.chars().any(char::is_control) {
            Some("A folder name can't be . or .. or contain /")
        } else {
            None
        }
    }

    pub fn depth_error(&self) -> Option<&'static str> {
        match (self.shallow, self.depth.trim().parse::<u32>()) {
            (false, _) | (true, Ok(1..)) => None,
            (true, _) => Some("Depth must be a whole number of at least 1"),
        }
    }

    /// Where the clone will land, for the sheet's note: `~/src/conduit`, `gpu-box:~/conduit`.
    pub fn target_label(&self) -> String {
        let (dest, name) = (self.dest.trim().trim_end_matches('/'), self.name.trim());
        match self.host_name() {
            Some(host) => format!("{host}:{dest}/{name}"),
            None => format!("{dest}/{name}"),
        }
    }

    /// The request when every field is valid, else the first field's message.
    pub fn request(&self, home: Option<&Path>) -> Result<CloneRequest, &'static str> {
        if let Some(e) =
            self.url_error().or(self.dest_error(home)).or(self.name_error()).or(self.depth_error())
        {
            return Err(e);
        }
        let dest = self.dest.trim();
        let parent = match self.host_name() {
            Some(host) if !crate::ssh::valid_host(host) => return Err("That host alias can't be used"),
            Some(host) => Location::Remote { host: host.to_string(), path: dest.to_string() },
            None => Location::Local { path: expand_local(dest, home).ok_or("Choose a destination folder")? },
        };
        Ok(CloneRequest {
            url: self.url.trim().to_string(),
            parent,
            name: self.name.trim().to_string(),
            depth: if self.shallow { self.depth.trim().parse().ok() } else { None },
            open_as_workspace: self.open_as_workspace,
        })
    }
}

/// `~` and `~/…` against `home`; anything else as typed. `None` for `~` without a home.
fn expand_local(dest: &str, home: Option<&Path>) -> Option<PathBuf> {
    match dest.strip_prefix('~') {
        Some("") => home.map(Path::to_path_buf),
        Some(rest) if rest.starts_with('/') => home.map(|h| h.join(rest.trim_start_matches('/'))),
        _ => Some(PathBuf::from(dest)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> PathBuf {
        PathBuf::from("/home/me") // portability: allow
    }

    fn form() -> CloneForm {
        CloneForm::new("~/src")
    }

    #[test]
    fn new_sheet_uses_the_default_dir_and_opens_as_workspace() {
        let f = form();
        assert_eq!(f.dest, "~/src");
        assert!(f.open_as_workspace, "Open as workspace defaults on (§5.3)");
        assert!(!f.shallow);
        assert_eq!(f.host, None);
    }

    #[test]
    fn folder_name_follows_the_url_until_edited() {
        let mut f = form();
        f.url = "git@github.com:acme/conduit.git".into();
        f.url_changed();
        assert_eq!(f.name, "conduit");

        f.name = "my-conduit".into();
        f.name_changed();
        f.url = "https://github.com/acme/other".into();
        f.url_changed();
        assert_eq!(f.name, "my-conduit", "a typed name is kept");

        f.name.clear();
        f.name_changed();
        assert_eq!(f.name, "other", "clearing the name hands it back to the URL");
    }

    #[test]
    fn clipboard_fills_only_an_empty_url_with_a_git_url() {
        let mut f = form();
        assert!(!f.offer_clipboard("https://example.com/docs"), "ordinary web link");
        assert!(!f.offer_clipboard("hello world"));
        assert!(f.url.is_empty());
        assert!(f.offer_clipboard("  git@github.com:acme/conduit.git\n"));
        assert_eq!(f.url, "git@github.com:acme/conduit.git");
        assert_eq!(f.name, "conduit");

        let mut typed = form();
        typed.url = "ssh://host/x.git".into();
        assert!(!typed.offer_clipboard("git@github.com:acme/conduit.git"), "never overwrites");
        assert_eq!(typed.url, "ssh://host/x.git");
    }

    #[test]
    fn url_validation() {
        let mut f = form();
        assert_eq!(f.url_error(), Some("Enter a repository URL"));
        f.url = "https://x/y z".into();
        assert!(f.url_error().is_some());
        f.url = " https://github.com/acme/conduit ".into();
        assert_eq!(f.url_error(), None);
    }

    #[test]
    fn name_validation() {
        let mut f = form();
        for bad in ["", "  ", ".", "..", "a/b"] {
            f.name = bad.into();
            assert!(f.name_error().is_some(), "{bad:?} accepted");
        }
        for good in ["conduit", "my repo", "-dash", "ünïcode"] {
            f.name = good.into();
            assert_eq!(f.name_error(), None, "{good:?} rejected");
        }
    }

    #[test]
    fn depth_is_checked_only_when_shallow() {
        let mut f = form();
        f.depth = "zero".into();
        assert_eq!(f.depth_error(), None);
        f.shallow = true;
        assert!(f.depth_error().is_some());
        for bad in ["0", "-1", "", "1.5"] {
            f.depth = bad.into();
            assert!(f.depth_error().is_some(), "{bad:?} accepted");
        }
        f.depth = " 50 ".into();
        assert_eq!(f.depth_error(), None);
    }

    #[test]
    fn local_destination_must_be_absolute_or_home_relative() {
        let mut f = form();
        let h = home();
        assert_eq!(f.dest_error(Some(&h)), None);
        f.dest = "relative/dir".into();
        assert!(f.dest_error(Some(&h)).is_some());
        f.dest = "~".into();
        assert!(f.dest_error(None).is_some(), "~ without a home");
        f.dest = String::new();
        assert_eq!(f.dest_error(Some(&h)), Some("Choose a destination folder"));
        f.dest = "/srv/git".into(); // portability: allow
        assert_eq!(f.dest_error(None), None);
    }

    #[test]
    fn request_resolves_a_local_parent_under_home() {
        let mut f = form();
        f.url = "git@github.com:acme/conduit.git".into();
        f.url_changed();
        f.shallow = true;
        f.depth = "10".into();
        let req = f.request(Some(&home())).expect("valid");
        assert_eq!(req.parent, Location::Local { path: home().join("src") });
        assert_eq!(req.name, "conduit");
        assert_eq!(req.depth, Some(10));
        assert!(req.open_as_workspace);
        assert_eq!(req.label(), "Cloning conduit");

        f.shallow = false;
        assert_eq!(f.request(Some(&home())).expect("valid").depth, None, "depth ignored unless shallow");
        f.dest = "~".into();
        assert_eq!(f.request(Some(&home())).expect("valid").parent, Location::Local { path: home() });
    }

    #[test]
    fn request_reports_the_first_invalid_field() {
        let mut f = form();
        assert_eq!(f.request(Some(&home())), Err("Enter a repository URL"));
        f.url = "https://github.com/acme/conduit".into();
        f.url_changed();
        f.dest = "rel".into();
        assert!(f.request(Some(&home())).is_err());
    }

    #[test]
    fn remote_host_makes_the_destination_a_remote_path() {
        let mut f = form();
        f.url = "https://github.com/acme/conduit".into();
        f.url_changed();
        f.set_hosts(vec!["gpu-box".into(), "build".into()]);
        f.host = Some(0);
        f.host_changed();
        assert_eq!(f.dest, "~", "untouched destination follows the host's home");
        f.dest = "~/work".into();
        f.dest_changed();
        assert_eq!(f.dest_error(None), None, "remote paths are not expanded locally");
        let req = f.request(None).expect("valid");
        assert_eq!(req.parent, Location::Remote { host: "gpu-box".into(), path: "~/work".into() });
        assert_eq!(f.target_label(), "gpu-box:~/work/conduit");

        f.host = None;
        f.host_changed();
        assert_eq!(f.dest, "~/work", "a typed destination is kept");
    }

    #[test]
    fn switching_back_to_local_restores_the_default_dir() {
        let mut f = form();
        f.set_hosts(vec!["gpu-box".into()]);
        f.host = Some(0);
        f.host_changed();
        f.host = None;
        f.host_changed();
        assert_eq!(f.dest, "~/src");
    }

    #[test]
    fn hosts_reload_keeps_the_choice_by_name() {
        let mut f = form();
        f.set_hosts(vec!["a".into(), "b".into()]);
        f.host = Some(1);
        f.host_changed();
        f.set_hosts(vec!["b".into()]);
        assert_eq!(f.host_name(), Some("b"));
        f.set_hosts(vec!["c".into()]);
        assert_eq!(f.host, None, "a host no longer in the config is dropped");
    }

    #[test]
    fn unusable_host_alias_is_refused() {
        let mut f = form();
        f.url = "https://github.com/acme/conduit".into();
        f.url_changed();
        f.set_hosts(vec!["-oProxyCommand=x".into()]);
        f.host = Some(0);
        f.host_changed();
        assert!(f.request(None).is_err());
    }

    #[test]
    fn target_label_shows_where_the_clone_lands() {
        let mut f = form();
        f.name = "conduit".into();
        assert_eq!(f.target_label(), "~/src/conduit");
        f.dest = "/".into();
        assert_eq!(f.target_label(), "/conduit");
    }
}
