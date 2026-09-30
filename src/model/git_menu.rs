//! Git pane menus and operation prompts, headless: the ordered commit context menu (§5.4), what
//! a graph row's checkout means (§5.9), live ref-name validation for the W19 popovers, and the
//! content of every §5.20 confirmation the branch / tag / rewrite / remote / worktree flows show
//! (§5.9–5.11, §5.16, §5.21). `ui::git_pane::menus` and `ops_ui` only render these.

use crate::git::log::Decoration;
use crate::git::ops::{Op, ResetMode, short_hash};
use crate::git::remote::validate_branch_name;
use crate::model::confirm::{Confirm, ConfirmKind};
use crate::model::fuzzy;

/// The refs a graph row carries, sorted out of its decorations.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RowRefs {
    /// The checked-out branch, when this row is its tip.
    pub current: Option<String>,
    /// Every local branch on the row, the current one included.
    pub locals: Vec<String>,
    /// Remote-tracking branches, `origin/main`.
    pub remotes: Vec<String>,
    pub tags: Vec<String>,
}

impl RowRefs {
    pub fn from_decorations(decs: &[Decoration]) -> Self {
        let mut r = RowRefs::default();
        for d in decs {
            match d {
                Decoration::CurrentBranch(n) => {
                    r.current = Some(n.clone());
                    r.locals.push(n.clone());
                }
                Decoration::Branch(n) => r.locals.push(n.clone()),
                Decoration::Remote(n) => r.remotes.push(n.clone()),
                Decoration::Tag(n) => r.tags.push(n.clone()),
                Decoration::Head | Decoration::Other(_) => {}
            }
        }
        r
    }

    /// The branch the row's push / upstream items act on: the current one, else the first.
    fn primary_local(&self) -> Option<&str> {
        self.current.as_deref().or(self.locals.first().map(String::as_str))
    }
}

/// Repository facts the menu depends on besides the row itself.
#[derive(Debug, Clone, Copy, Default)]
pub struct MenuContext<'a> {
    /// HEAD's commit id.
    pub head: Option<&'a str>,
    /// `None` when detached.
    pub current_branch: Option<&'a str>,
    /// The remote push and new-branch upstreams default to (§5.10).
    pub default_remote: Option<&'a str>,
    pub remotes: &'a [String],
    pub local_branches: &'a [String],
    /// "commit ab12cd3": the newest entry undo would revert, if any.
    pub undo: Option<&'a str>,
    pub redo: Option<&'a str>,
    /// A forge web URL is known for the default remote (permalink / open in browser).
    pub web: bool,
    /// Whether the row's commit is in HEAD's history (`None`: not known from the loaded
    /// graph). Cherry-pick hides for an ancestor (it would be empty), Revert for a non-ancestor.
    pub in_head: Option<bool>,
}

/// Whether `target` is reachable from `from` through `parents` (`None` for a commit not
/// loaded): `Some(true)` once found, `Some(false)` only when every reachable commit was loaded.
pub fn reaches<'a>(from: &str, target: &str, parents: impl Fn(&str) -> Option<&'a [String]>) -> Option<bool> {
    let mut seen = std::collections::HashSet::new();
    let mut stack = vec![from.to_string()];
    let mut complete = true;
    while let Some(id) = stack.pop() {
        if id == target {
            return Some(true);
        }
        if !seen.insert(id.clone()) {
            continue;
        }
        match parents(&id) {
            Some(ps) => stack.extend(ps.iter().cloned()),
            None => complete = false,
        }
    }
    complete.then_some(false)
}

/// What a checkout of a row, chip, or Refs row does (§5.9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckoutTarget {
    Branch(String),
    /// A remote branch: a tracking local branch of the same name.
    Remote {
        remote: String,
        branch: String,
    },
    /// Detached HEAD at a commit or tag.
    Detached(String),
}

/// Every action the commit context menu can trigger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuAction {
    Checkout(CheckoutTarget),
    CreateBranch,
    CreateTag,
    /// `remote: None` pushes to the default (bound) remote.
    Push {
        branch: String,
        remote: Option<String>,
    },
    ForcePushWithLease {
        branch: String,
    },
    /// §5.11 **Push to…** on a tag; `remote: None` pushes to the default (bound) remote.
    PushTag {
        tag: String,
        remote: Option<String>,
    },
    SetUpstream {
        branch: String,
    },
    Pull,
    Merge {
        rev: String,
    },
    Rebase {
        onto: String,
    },
    CherryPick,
    Revert,
    Reset(ResetMode),
    Undo,
    Redo,
    RenameBranch(String),
    DeleteBranch(String),
    DeleteTag(String),
    CopyHash,
    CopySubject,
    CopyPermalink,
    OpenInBrowser,
    /// §5.13 **Compare with current**: compare mode, `current`…`rev`.
    CompareWithCurrent {
        rev: String,
    },
    /// §5.13 **Compare with…**: compare mode with `rev` on the right and the left picker open.
    CompareWith {
        rev: String,
    },
    /// §5.14 **Cherry-pick N commits onto…** ▸ `onto` (a local branch).
    CherryPickSelection {
        onto: String,
    },
    /// §5.14 **Revert N commits**.
    RevertSelection,
    /// §5.14 **Squash into one**.
    SquashSelection,
    /// §5.14 **Copy hashes**.
    CopyHashes,
    /// §5.14 **Compare oldest…newest**.
    CompareSelection,
    /// §5.16 **Interactively rebase from here**.
    InteractiveRebase,
    /// §5.16 **Edit commit message**.
    EditMessage,
    /// §5.16 **Squash into parent**.
    SquashIntoParent,
    /// §5.16 **Fixup into parent**.
    FixupIntoParent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuEntry {
    Item { label: String, action: MenuAction, enabled: bool },
    Submenu { label: String, entries: Vec<MenuEntry> },
    Separator,
}

fn item(label: impl Into<String>, action: MenuAction) -> MenuEntry {
    MenuEntry::Item { label: label.into(), action, enabled: true }
}

/// Splits `origin/feat/x` into (`origin`, `feat/x`), preferring the longest known remote name
/// (a remote may itself contain `/`), else the first segment.
pub fn split_remote_branch(short: &str, remotes: &[String]) -> Option<(String, String)> {
    let known = remotes
        .iter()
        .filter(|r| {
            short.len() > r.len() + 1 && short.starts_with(r.as_str()) && short[r.len()..].starts_with('/')
        })
        .max_by_key(|r| r.len());
    match known {
        Some(r) => Some((r.clone(), short[r.len() + 1..].to_string())),
        None => short.split_once('/').map(|(a, b)| (a.to_string(), b.to_string())),
    }
}

/// §5.4 item 1: the row's branch if it has one that isn't checked out, else a remote branch
/// (preferring one with no local namesake: a tracking branch is created; with one, the checkout
/// asks, [`RemoteCheckout`]), else the commit detached. `None` when the row is exactly what HEAD
/// already is.
pub fn checkout_choice(id: &str, refs: &RowRefs, cx: &MenuContext) -> Option<CheckoutTarget> {
    if let Some(b) = refs.locals.iter().find(|b| Some(b.as_str()) != cx.current_branch) {
        return Some(CheckoutTarget::Branch(b.clone()));
    }
    if refs.current.is_some() {
        return None;
    }
    let remotes: Vec<(String, String)> = refs
        .remotes
        .iter()
        .filter_map(|r| split_remote_branch(r, cx.remotes))
        .filter(|(_, branch)| branch != "HEAD")
        .collect();
    let untracked = remotes.iter().find(|(_, branch)| !cx.local_branches.iter().any(|l| l == branch));
    let is_current = |(_, b): &&(String, String)| Some(b.as_str()) == cx.current_branch;
    // A remote branch whose namesake is what HEAD is on already would only ask to check it out.
    if let Some((remote, branch)) = untracked.or_else(|| remotes.iter().find(|r| !is_current(r))).cloned() {
        return Some(CheckoutTarget::Remote { remote, branch });
    }
    if cx.head == Some(id) && cx.current_branch.is_none() {
        return None; // already detached here
    }
    Some(CheckoutTarget::Detached(id.to_string()))
}

/// §5.9 "asking only if the local name exists": what checking out `origin/feat` does when a
/// local `feat` already exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteCheckout {
    /// Checkout the existing local branch.
    Local,
    /// A new tracking branch under another name.
    NewBranch,
    /// The remote branch's commit, detached.
    Detached,
}

/// The name offered for a new tracking branch when `branch` is taken: `origin-feat`, then
/// `origin-feat-2`, ….
pub fn tracking_name_suggestion(remote: &str, branch: &str, existing: &[String]) -> String {
    let base = format!("{}-{branch}", remote.replace('/', "-"));
    let taken = |n: &str| existing.iter().any(|e| e == n);
    if !taken(&base) {
        return base;
    }
    (2..).map(|i| format!("{base}-{i}")).find(|n| !taken(n)).unwrap_or(base)
}

/// The operations a [`RemoteCheckout`] choice runs, in order.
pub fn remote_checkout_ops(remote: &str, branch: &str, choice: RemoteCheckout, name: &str) -> Vec<Op> {
    let tracking = format!("{remote}/{branch}");
    match choice {
        RemoteCheckout::Local => vec![Op::Checkout { branch: branch.to_string(), force: false }],
        RemoteCheckout::NewBranch => vec![
            Op::CreateBranch { name: name.to_string(), base: tracking.clone(), checkout: true },
            Op::SetUpstream { branch: name.to_string(), upstream: Some(tracking) },
        ],
        RemoteCheckout::Detached => vec![Op::CheckoutDetached { rev: tracking }],
    }
}

pub fn checkout_label(target: &CheckoutTarget) -> String {
    match target {
        CheckoutTarget::Branch(b) => format!("Checkout `{b}`"),
        CheckoutTarget::Remote { remote, branch } => format!("Checkout `{remote}/{branch}`"),
        CheckoutTarget::Detached(_) => "Checkout commit".to_string(),
    }
}

/// `main`, or `HEAD` when detached: what merge / rebase / reset act on.
fn current_name<'a>(cx: &MenuContext<'a>) -> &'a str {
    cx.current_branch.unwrap_or("HEAD")
}

/// The commit context menu (§5.4), in the spec's order, with items that don't apply hidden
/// (Undo / Redo stay, disabled, when their stack is empty). Groups are separated by
/// [`MenuEntry::Separator`]. Item 8 (interactive rebase, edit message, squash, fixup) is
/// [`rewrite_entries`], placed by [`insert_group_after_undo`]; 9 (compare) is added here.
pub fn commit_menu(id: &str, refs: &RowRefs, cx: &MenuContext) -> Vec<MenuEntry> {
    let mut groups: Vec<Vec<MenuEntry>> = Vec::new();
    let current = current_name(cx);
    let is_head = cx.head == Some(id);

    // 1. Checkout
    groups.push(
        checkout_choice(id, refs, cx)
            .map(|t| item(checkout_label(&t), MenuAction::Checkout(t)))
            .into_iter()
            .collect(),
    );

    // 2. Create branch here… · Create tag…
    groups.push(vec![
        item("Create branch here…", MenuAction::CreateBranch),
        item("Create tag…", MenuAction::CreateTag),
    ]);

    // 3. Push / Push to… / Force push with lease / Set upstream… (+ Pull on the current branch)
    let mut push = Vec::new();
    if let Some(branch) = refs.primary_local()
        && !cx.remotes.is_empty()
    {
        let remote = cx.default_remote.unwrap_or(&cx.remotes[0]);
        push.push(item(
            format!("Push `{branch}` to {remote}"),
            MenuAction::Push { branch: branch.to_string(), remote: None },
        ));
        push.push(MenuEntry::Submenu {
            label: "Push to…".to_string(),
            entries: cx
                .remotes
                .iter()
                .map(|r| {
                    item(r.clone(), MenuAction::Push { branch: branch.to_string(), remote: Some(r.clone()) })
                })
                .collect(),
        });
        push.push(item(
            "Force push with lease",
            MenuAction::ForcePushWithLease { branch: branch.to_string() },
        ));
        push.push(item("Set upstream…", MenuAction::SetUpstream { branch: branch.to_string() }));
        if refs.current.is_some() {
            push.push(item("Pull", MenuAction::Pull));
        }
    } else if !cx.remotes.is_empty() {
        // A row carrying only tags: §5.11 **Push to…** on each tag.
        let remote = cx.default_remote.unwrap_or(&cx.remotes[0]);
        for t in &refs.tags {
            push.push(item(
                format!("Push tag `{t}` to {remote}"),
                MenuAction::PushTag { tag: t.clone(), remote: None },
            ));
            push.push(MenuEntry::Submenu {
                label: format!("Push tag `{t}` to…"),
                entries: cx
                    .remotes
                    .iter()
                    .map(|r| item(r.clone(), MenuAction::PushTag { tag: t.clone(), remote: Some(r.clone()) }))
                    .collect(),
            });
        }
    }
    groups.push(push);

    // 4. Merge into current · Rebase current onto (a branch that is not current)
    let other = refs
        .locals
        .iter()
        .find(|b| Some(b.as_str()) != cx.current_branch)
        .or_else(|| refs.remotes.iter().find(|r| !r.ends_with("/HEAD")));
    groups.push(match other {
        Some(b) if !is_head => vec![
            item(format!("Merge `{b}` into `{current}`"), MenuAction::Merge { rev: b.clone() }),
            item(format!("Rebase `{current}` onto `{b}`"), MenuAction::Rebase { onto: b.clone() }),
        ],
        _ => Vec::new(),
    });

    // 5. Cherry-pick onto current · Revert
    let mut pick = Vec::new();
    if !is_head && cx.head.is_some() && cx.in_head != Some(true) {
        pick.push(item(format!("Cherry-pick onto `{current}`"), MenuAction::CherryPick));
    }
    if cx.head.is_some() && cx.in_head != Some(false) {
        pick.push(item("Revert", MenuAction::Revert));
    }
    groups.push(pick);

    // 6. Reset `main` to here ▸ Soft / Mixed / Hard
    groups.push(if is_head || cx.head.is_none() {
        Vec::new()
    } else {
        vec![MenuEntry::Submenu {
            label: format!("Reset `{current}` to here"),
            entries: vec![
                item("Soft — keep changes staged", MenuAction::Reset(ResetMode::Soft)),
                item("Mixed — keep changes unstaged", MenuAction::Reset(ResetMode::Mixed)),
                item("Hard — discard changes", MenuAction::Reset(ResetMode::Hard)),
            ],
        }]
    });

    // 7. Undo · Redo (disabled with their description when the stack is empty)
    let stack = |verb: &str, desc: Option<&str>, action: MenuAction| MenuEntry::Item {
        label: match desc {
            Some(d) => format!("{verb}: {d}"),
            None => format!("{verb} last operation"),
        },
        action,
        enabled: desc.is_some(),
    };
    groups.push(vec![stack("Undo", cx.undo, MenuAction::Undo), stack("Redo", cx.redo, MenuAction::Redo)]);

    // 9. Compare with current · Compare with… (§5.13)
    let rev = compare_rev(id, refs, cx);
    let mut compare = Vec::new();
    if !is_head && cx.head.is_some() {
        compare.push(item("Compare with current", MenuAction::CompareWithCurrent { rev: rev.clone() }));
    }
    compare.push(item("Compare with…", MenuAction::CompareWith { rev }));
    groups.push(compare);

    // 10. Rename branch · Delete branch · Delete tag (per ref on the row)
    let mut per_ref = Vec::new();
    for b in &refs.locals {
        per_ref.push(item(format!("Rename branch `{b}`…"), MenuAction::RenameBranch(b.clone())));
        if Some(b.as_str()) != cx.current_branch {
            per_ref.push(item(format!("Delete branch `{b}`"), MenuAction::DeleteBranch(b.clone())));
        }
    }
    for t in &refs.tags {
        per_ref.push(item(format!("Delete tag `{t}`"), MenuAction::DeleteTag(t.clone())));
    }
    groups.push(per_ref);

    // 11. Copy hash · Copy subject · Copy permalink · Open in browser
    let mut copy =
        vec![item("Copy hash", MenuAction::CopyHash), item("Copy subject", MenuAction::CopySubject)];
    if cx.web {
        copy.push(item("Copy permalink", MenuAction::CopyPermalink));
        copy.push(item("Open in browser", MenuAction::OpenInBrowser));
    }
    groups.push(copy);

    let mut out = Vec::new();
    for g in groups.into_iter().filter(|g| !g.is_empty()) {
        if !out.is_empty() {
            out.push(MenuEntry::Separator);
        }
        out.extend(g);
    }
    out
}

/// What a row's **Compare with…** compares (§5.13): its first branch that isn't current, else
/// its current branch, a remote branch, a tag, and finally the commit itself.
pub fn compare_rev(id: &str, refs: &RowRefs, cx: &MenuContext) -> String {
    refs.locals
        .iter()
        .find(|b| Some(b.as_str()) != cx.current_branch)
        .or(refs.current.as_ref())
        .or_else(|| refs.remotes.iter().find(|r| !r.ends_with("/HEAD")))
        .or(refs.tags.first())
        .cloned()
        .unwrap_or_else(|| id.to_string())
}

/// Repository facts the multi-selection menu (§5.14) depends on.
#[derive(Debug, Clone, Copy, Default)]
pub struct SelectionContext<'a> {
    /// How many commits are selected (2 or more).
    pub count: usize,
    /// `None` when detached.
    pub current_branch: Option<&'a str>,
    pub local_branches: &'a [String],
    /// Contiguous rows forming one first-parent line of non-merge commits, all in the current
    /// branch's history: **Squash into one** applies.
    pub squashable: bool,
    /// Any selected commit is a merge: revert and cherry-pick need a mainline per commit, so
    /// they are left to the single-commit menu.
    pub has_merge: bool,
    /// Every selected commit is already in the current branch: picking them onto it would be
    /// empty, so it is not offered.
    pub in_current: bool,
}

/// The context menu of a multi-commit selection (§5.14): **Cherry-pick N commits onto…** ▸
/// local branches (the current one first), **Revert N commits**, **Squash into one** (only when
/// squashable), **Copy hashes**, **Compare oldest…newest**.
pub fn selection_menu(cx: &SelectionContext) -> Vec<MenuEntry> {
    let n = cx.count;
    let mut onto: Vec<MenuEntry> = Vec::new();
    if let Some(current) = cx.current_branch.filter(|_| !cx.in_current) {
        onto.push(item(
            format!("`{current}` (current)"),
            MenuAction::CherryPickSelection { onto: current.into() },
        ));
    }
    for b in cx.local_branches.iter().filter(|b| Some(b.as_str()) != cx.current_branch) {
        onto.push(item(format!("`{b}`"), MenuAction::CherryPickSelection { onto: b.clone() }));
    }
    let mut out = Vec::new();
    let pick = format!("Cherry-pick {n} commits onto…");
    if cx.has_merge {
        // `git cherry-pick` of a merge needs `-m`: left to single-commit cherry-pick, like revert.
        out.push(MenuEntry::Item { label: pick, action: MenuAction::CopyHashes, enabled: false });
    } else if !onto.is_empty() {
        out.push(MenuEntry::Submenu { label: pick, entries: onto });
    }
    out.push(MenuEntry::Item {
        label: format!("Revert {n} commits"),
        action: MenuAction::RevertSelection,
        enabled: !cx.has_merge,
    });
    if cx.squashable {
        out.push(item("Squash into one", MenuAction::SquashSelection));
    }
    out.push(MenuEntry::Separator);
    out.push(item("Copy hashes", MenuAction::CopyHashes));
    out.push(item("Compare oldest…newest", MenuAction::CompareSelection));
    out
}

/// §5.4 item 8, the history-rewriting group: **Interactively rebase from here** · **Edit
/// message** · **Squash into parent** · **Fixup into parent**. Only for a commit in the current
/// branch's history (`in_head`); the last two need a single parent (`parents`), itself rewritable.
pub fn rewrite_entries(cx: &MenuContext, parents: usize) -> Vec<MenuEntry> {
    if cx.current_branch.is_none() || cx.in_head != Some(true) {
        return Vec::new();
    }
    let mut out = vec![
        item("Interactively rebase from here…", MenuAction::InteractiveRebase),
        item("Edit commit message…", MenuAction::EditMessage),
    ];
    if parents == 1 {
        out.push(item("Squash into parent", MenuAction::SquashIntoParent));
        out.push(item("Fixup into parent", MenuAction::FixupIntoParent));
    }
    out
}

/// Puts `group` into a [`commit_menu`] where §5.4 orders it: after the Undo / Redo group (7),
/// else at the end, separated from its neighbours.
pub fn insert_group_after_undo(menu: &mut Vec<MenuEntry>, group: Vec<MenuEntry>) {
    if group.is_empty() {
        return;
    }
    let redo = menu
        .iter()
        .position(|e| matches!(e, MenuEntry::Item { action: MenuAction::Redo, .. }))
        .map_or(menu.len(), |i| i + 1);
    let mut block = if redo > 0 { vec![MenuEntry::Separator] } else { Vec::new() };
    block.extend(group);
    if redo < menu.len() && !matches!(menu[redo], MenuEntry::Separator) {
        block.push(MenuEntry::Separator);
    }
    menu.splice(redo..redo, block);
}

/// The remote push, pull, fetch, and new-branch upstreams default to (§5.10): the remote the
/// workspace is bound to, else the current branch's upstream remote, else `origin`, else the
/// first remote.
pub fn default_remote(bound: Option<&str>, upstream: Option<&str>, remotes: &[String]) -> Option<String> {
    bound
        .filter(|b| remotes.iter().any(|r| r == b))
        .map(str::to_string)
        .or_else(|| upstream.and_then(|u| split_remote_branch(u, remotes)).map(|(r, _)| r))
        .filter(|r| remotes.contains(r))
        .or_else(|| remotes.iter().find(|r| *r == "origin").cloned())
        .or_else(|| remotes.first().cloned())
}

// ---- W19 live validation ------------------------------------------------------------------------

/// `git check-ref-format --branch` rules plus "already exists", as the create / rename popovers
/// show them under the field. An empty name is invalid without a message.
pub fn branch_name_error(name: &str, existing: &[String]) -> Option<String> {
    if name.is_empty() {
        return Some(String::new());
    }
    if let Err(why) = validate_branch_name(name) {
        return Some(capitalize(why));
    }
    existing.iter().any(|b| b == name).then(|| format!("Branch `{name}` already exists"))
}

/// Tag names follow the same `check-ref-format` rules.
pub fn tag_name_error(name: &str, existing: &[String]) -> Option<String> {
    if name.is_empty() {
        return Some(String::new());
    }
    if let Err(why) = validate_branch_name(name) {
        return Some(capitalize(&why.replace("a branch", "a tag")));
    }
    existing.iter().any(|t| t == name).then(|| format!("Tag `{name}` already exists"))
}

/// Remote names: a ref component, so the same rules.
pub fn remote_name_error(name: &str, existing: &[String]) -> Option<String> {
    if name.is_empty() {
        return Some(String::new());
    }
    if name.contains('/') {
        return Some("Remote name may not contain '/'".to_string());
    }
    if let Err(why) = validate_branch_name(name) {
        return Some(capitalize(why));
    }
    existing.iter().any(|r| r == name).then(|| format!("Remote `{name}` already exists"))
}

pub fn url_error(url: &str) -> Option<String> {
    let url = url.trim();
    if url.is_empty() {
        Some(String::new())
    } else if url.starts_with('-') {
        Some("URL may not begin with '-'".to_string())
    } else {
        None
    }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}

// ---- pickers --------------------------------------------------------------------------------

/// Candidates for **Set upstream…** / **Merge…** / **Rebase…** (§5.9): fuzzy-ranked by `query`;
/// with an empty query, `primary`'s branches (the bound remote) first, then the rest by name.
pub fn rank_candidates(candidates: &[String], primary: Option<&str>, query: &str) -> Vec<String> {
    if query.trim().is_empty() {
        let mut out: Vec<String> = candidates.to_vec();
        out.sort_by_key(|c| (primary.is_none_or(|p| !c.starts_with(&format!("{p}/"))), c.clone()));
        return out;
    }
    let labels: Vec<&str> = candidates.iter().map(String::as_str).collect();
    fuzzy::rank(query, &labels).into_iter().map(|(i, _)| candidates[i].clone()).collect()
}

/// §5.16: "Merge commits prompt for `-m 1` or `-m 2` with the parent subjects shown". `parents`
/// is each parent's id and subject when known.
pub fn mainline_choices(parents: &[(String, Option<String>)]) -> Vec<(u8, String)> {
    parents
        .iter()
        .enumerate()
        .take(u8::MAX as usize)
        .map(|(i, (id, subject))| {
            let n = (i + 1) as u8;
            let label = match subject {
                Some(s) => format!("-m {n} · {} {s}", short_hash(id)),
                None => format!("-m {n} · {}", short_hash(id)),
            };
            (n, label)
        })
        .collect()
}

/// The anchored popover's sentence for `m`, `r`, `p` (§5.4): "Merge `feat/oauth` into `main`".
pub fn merge_sentence(rev: &str, current: Option<&str>) -> String {
    format!("Merge `{rev}` into `{}`", current.unwrap_or("HEAD"))
}

pub fn rebase_sentence(onto: &str, current: Option<&str>) -> String {
    format!("Rebase `{}` onto `{onto}`", current.unwrap_or("HEAD"))
}

pub fn cherry_pick_sentence(id: &str, current: Option<&str>) -> String {
    format!("Cherry-pick {} onto `{}`", short_hash(id), current.unwrap_or("HEAD"))
}

// ---- §5.20 confirmations ------------------------------------------------------------------------

/// A commit as a confirmation lists it: `ab12cd3  Subject`.
pub fn commit_line(id: &str, subject: &str) -> String {
    format!("{}  {subject}", short_hash(id))
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// §5.16: the confirm for all three reset modes lists the commits that leave the branch by
/// subject; Hard also lists the working-tree files that will be lost.
pub fn reset_confirm(
    branch: Option<&str>,
    target: &str,
    mode: ResetMode,
    leaving: &[(String, String)],
    lost_files: &[String],
) -> Confirm {
    let name = branch.unwrap_or("HEAD");
    let (kind, mode_name) = match mode {
        ResetMode::Soft => (ConfirmKind::ResetSoft, "soft"),
        ResetMode::Mixed => (ConfirmKind::ResetMixed, "mixed"),
        ResetMode::Hard => (ConfirmKind::ResetHard, "hard"),
    };
    let commits = if leaving.is_empty() {
        format!("No commits leave `{name}`")
    } else {
        format!("{} will leave `{name}`", plural(leaving.len(), "commit", "commits"))
    };
    let detail = match mode {
        ResetMode::Soft => format!("{commits}; their changes stay staged."),
        ResetMode::Mixed => format!("{commits}; their changes stay in the working tree, unstaged."),
        ResetMode::Hard if lost_files.is_empty() => format!("{commits} and their changes are discarded."),
        ResetMode::Hard => format!(
            "{commits}, and uncommitted changes to {} will be lost (undo restores them):",
            plural(lost_files.len(), "file", "files")
        ),
    };
    let mut lost: Vec<String> = leaving.iter().map(|(id, s)| commit_line(id, s)).collect();
    if mode == ResetMode::Hard {
        lost.extend(lost_files.iter().cloned());
    }
    Confirm::new(kind, format!("Reset `{name}` to {} ({mode_name})?", short_hash(target)))
        .detail(detail)
        .lost(lost)
}

/// §5.9 / W13: delete an unmerged branch. `remote_branch` (`origin/feat`) adds the
/// **Also delete** checkbox.
pub fn delete_branch_confirm(
    name: &str,
    onto: Option<&str>,
    unmerged: &[(String, String)],
    remote_branch: Option<&str>,
) -> Confirm {
    let onto = onto.map_or("HEAD".to_string(), |b| format!("`{b}`"));
    let detail = if unmerged.is_empty() {
        format!("The branch is not fully merged into {onto}.")
    } else {
        format!(
            "{} not on {onto} and will be lost unless another ref points to them:",
            match unmerged.len() {
                1 => "1 commit is".to_string(),
                n => format!("{n} commits are"),
            }
        )
    };
    let mut c = Confirm::new(ConfirmKind::DeleteUnmergedBranch, format!("Delete branch `{name}`?"))
        .detail(detail)
        .lost(unmerged.iter().map(|(id, s)| commit_line(id, s)));
    if let Some(r) = remote_branch {
        c = c.option(format!("Also delete `{r}`"), false);
    }
    c
}

/// The remote-tracking branch a local branch delete offers to delete too (§5.9 "If a remote
/// tracking branch exists"): its configured upstream if that still exists, else a branch of the
/// same name on the default remote.
pub fn remote_branch_for(
    branch: &str,
    upstream: Option<&str>,
    default_remote: Option<&str>,
    remote_branches: &[String],
) -> Option<String> {
    let exists = |r: &str| remote_branches.iter().any(|b| b == r);
    upstream
        .filter(|u| exists(u))
        .map(str::to_string)
        .or_else(|| default_remote.map(|r| format!("{r}/{branch}")).filter(|r| exists(r)))
}

/// §5.11 Delete tag: "Confirm offers **Also delete on remotes**", one checkbox per remote.
pub fn delete_tag_confirm(name: &str, remotes: &[String]) -> Confirm {
    let mut c = Confirm::new(ConfirmKind::DeleteRemoteTag, format!("Delete tag `{name}`?"))
        .detail("The local tag can be restored with undo; a tag deleted on a remote cannot.");
    for r in remotes {
        c = c.option(format!("Also delete on {r}"), false);
    }
    c
}

/// §5.10 Remove remote: its remote-tracking branches go with it.
pub fn remove_remote_confirm(name: &str, tracking: &[String]) -> Confirm {
    let detail = if tracking.is_empty() {
        "The remote's configuration is removed; undo adds it back.".to_string()
    } else {
        format!(
            "{} and upstream settings are removed; undo restores them:",
            plural(tracking.len(), "remote-tracking branch", "remote-tracking branches")
        )
    };
    Confirm::new(ConfirmKind::RemoveRemote, format!("Remove remote `{name}`?"))
        .detail(detail)
        .lost(tracking.to_vec())
}

/// §5.21 Remove a dirty worktree: the file list.
pub fn remove_worktree_confirm(path: &str, files: &[String]) -> Confirm {
    Confirm::new(ConfirmKind::RemoveDirtyWorktree, format!("Remove worktree `{path}`?"))
        .detail(format!(
            "It has uncommitted changes to {} that will be lost:",
            plural(files.len(), "file", "files")
        ))
        .lost(files.to_vec())
}

/// Undo or redo whose plan hard-resets over uncommitted changes (`ops::lost_changes`, §5.9
/// "confirm if working tree dirty").
pub fn lossy_undo_confirm(is_undo: bool, description: &str, files: &[String]) -> Confirm {
    let verb = if is_undo { "Undo" } else { "Redo" };
    Confirm::new(ConfirmKind::ResetHard, format!("{verb} {description}?"))
        .detail(format!(
            "{verb} resets the working tree; uncommitted changes to {} will be lost:",
            plural(files.len(), "file", "files")
        ))
        .lost(files.to_vec())
        .verb(verb)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    fn labels(entries: &[MenuEntry]) -> Vec<String> {
        entries
            .iter()
            .map(|e| match e {
                MenuEntry::Item { label, .. } => label.clone(),
                MenuEntry::Submenu { label, .. } => format!("{label} ▸"),
                MenuEntry::Separator => "---".to_string(),
            })
            .collect()
    }

    fn find<'a>(entries: &'a [MenuEntry], prefix: &str) -> Option<&'a MenuEntry> {
        entries.iter().find(|e| match e {
            MenuEntry::Item { label, .. } | MenuEntry::Submenu { label, .. } => label.starts_with(prefix),
            MenuEntry::Separator => false,
        })
    }

    struct Fixture {
        remotes: Vec<String>,
        locals: Vec<String>,
    }

    impl Fixture {
        fn new() -> Self {
            Fixture { remotes: s(&["origin", "upstream"]), locals: s(&["main", "feat"]) }
        }
        fn cx(&self) -> MenuContext<'_> {
            MenuContext {
                head: Some("h0"),
                current_branch: Some("main"),
                default_remote: Some("origin"),
                remotes: &self.remotes,
                local_branches: &self.locals,
                undo: Some("commit h0h0h0h"),
                redo: None,
                web: true,
                in_head: None,
            }
        }
    }

    fn refs(decs: Vec<Decoration>) -> RowRefs {
        RowRefs::from_decorations(&decs)
    }

    // ---- RowRefs ----

    #[test]
    fn row_refs_sorts_decorations() {
        let r = refs(vec![
            Decoration::Head,
            Decoration::CurrentBranch("main".into()),
            Decoration::Branch("feat".into()),
            Decoration::Remote("origin/main".into()),
            Decoration::Tag("v1".into()),
            Decoration::Other("refs/stash".into()),
        ]);
        assert_eq!(r.current.as_deref(), Some("main"));
        assert_eq!(r.locals, s(&["main", "feat"]));
        assert_eq!(r.remotes, s(&["origin/main"]));
        assert_eq!(r.tags, s(&["v1"]));
    }

    // ---- the §5.4 order ----

    #[test]
    fn menu_for_a_feature_branch_row_follows_the_spec_order() {
        let f = Fixture::new();
        let row = refs(vec![Decoration::Branch("feat".into()), Decoration::Tag("v1".into())]);
        let got = labels(&commit_menu("c1", &row, &f.cx()));
        assert_eq!(
            got,
            [
                "Checkout `feat`",
                "---",
                "Create branch here…",
                "Create tag…",
                "---",
                "Push `feat` to origin",
                "Push to… ▸",
                "Force push with lease",
                "Set upstream…",
                "---",
                "Merge `feat` into `main`",
                "Rebase `main` onto `feat`",
                "---",
                "Cherry-pick onto `main`",
                "Revert",
                "---",
                "Reset `main` to here ▸",
                "---",
                "Undo: commit h0h0h0h",
                "Redo last operation",
                "---",
                "Compare with current",
                "Compare with…",
                "---",
                "Rename branch `feat`…",
                "Delete branch `feat`",
                "Delete tag `v1`",
                "---",
                "Copy hash",
                "Copy subject",
                "Copy permalink",
                "Open in browser",
            ]
        );
    }

    #[test]
    fn head_row_on_current_branch_hides_checkout_merge_pick_reset_and_delete() {
        let f = Fixture::new();
        let row = refs(vec![Decoration::Head, Decoration::CurrentBranch("main".into())]);
        let menu = commit_menu("h0", &row, &f.cx());
        let got = labels(&menu);
        assert!(!got.iter().any(|l| l.starts_with("Checkout")), "{got:?}");
        assert!(!got.iter().any(|l| l.starts_with("Merge") || l.starts_with("Rebase")));
        assert!(!got.iter().any(|l| l.starts_with("Cherry-pick") || l.starts_with("Reset")));
        assert!(!got.iter().any(|l| l.starts_with("Delete branch")));
        assert!(got.contains(&"Pull".to_string()), "the current branch row offers Pull");
        assert!(got.contains(&"Revert".to_string()));
        assert!(got.contains(&"Rename branch `main`…".to_string()));
    }

    #[test]
    fn plain_commit_checks_out_detached_and_has_no_push_items() {
        let f = Fixture::new();
        let menu = commit_menu("c9", &RowRefs::default(), &f.cx());
        assert_eq!(
            find(&menu, "Checkout"),
            Some(&MenuEntry::Item {
                label: "Checkout commit".into(),
                action: MenuAction::Checkout(CheckoutTarget::Detached("c9".into())),
                enabled: true
            })
        );
        assert!(find(&menu, "Push").is_none());
        assert!(find(&menu, "Merge").is_none());
    }

    #[test]
    fn empty_redo_stack_is_disabled_not_hidden() {
        let f = Fixture::new();
        let menu = commit_menu("c9", &RowRefs::default(), &f.cx());
        match find(&menu, "Redo") {
            Some(MenuEntry::Item { enabled, .. }) => assert!(!enabled),
            other => panic!("{other:?}"),
        }
        match find(&menu, "Undo") {
            Some(MenuEntry::Item { enabled, label, .. }) => {
                assert!(enabled);
                assert_eq!(label, "Undo: commit h0h0h0h");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn ancestors_of_head_hide_cherry_pick_and_other_commits_hide_revert() {
        let f = Fixture::new();
        let mut cx = f.cx();
        cx.in_head = Some(true);
        let got = labels(&commit_menu("c1", &RowRefs::default(), &cx));
        assert!(!got.iter().any(|l| l.starts_with("Cherry-pick")), "{got:?}");
        assert!(got.contains(&"Revert".to_string()));
        cx.in_head = Some(false);
        let got = labels(&commit_menu("c1", &RowRefs::default(), &cx));
        assert!(got.iter().any(|l| l.starts_with("Cherry-pick")));
        assert!(!got.contains(&"Revert".to_string()));
    }

    // ---- §5.13 compare / §5.14 selection ----

    #[test]
    fn compare_items_name_the_rows_ref_and_hide_compare_with_current_on_head() {
        let f = Fixture::new();
        let row = refs(vec![Decoration::Branch("feat".into()), Decoration::Tag("v1".into())]);
        let menu = commit_menu("c1", &row, &f.cx());
        assert!(
            menu.contains(&item(
                "Compare with current",
                MenuAction::CompareWithCurrent { rev: "feat".into() }
            ))
        );
        assert!(menu.contains(&item("Compare with…", MenuAction::CompareWith { rev: "feat".into() })));
        let head = commit_menu("h0", &refs(vec![Decoration::CurrentBranch("main".into())]), &f.cx());
        assert!(find(&head, "Compare with current").is_none());
        assert!(head.contains(&item("Compare with…", MenuAction::CompareWith { rev: "main".into() })));
    }

    #[test]
    fn compare_rev_prefers_branches_then_remotes_tags_and_the_hash() {
        let f = Fixture::new();
        let cx = f.cx();
        let r = |d: Vec<Decoration>| compare_rev("c1", &refs(d), &cx);
        assert_eq!(r(vec![Decoration::CurrentBranch("main".into()), Decoration::Branch("x".into())]), "x");
        assert_eq!(
            r(vec![Decoration::Remote("origin/HEAD".into()), Decoration::Remote("origin/a".into())]),
            "origin/a"
        );
        assert_eq!(r(vec![Decoration::Tag("v1".into())]), "v1");
        assert_eq!(r(vec![]), "c1");
    }

    #[test]
    fn selection_menu_follows_the_spec_wording() {
        let locals = s(&["main", "feat"]);
        let cx = SelectionContext {
            count: 5,
            current_branch: Some("feat"),
            local_branches: &locals,
            squashable: true,
            has_merge: false,
            in_current: false,
        };
        let menu = selection_menu(&cx);
        assert_eq!(
            labels(&menu),
            [
                "Cherry-pick 5 commits onto… ▸",
                "Revert 5 commits",
                "Squash into one",
                "---",
                "Copy hashes",
                "Compare oldest…newest"
            ]
        );
        let Some(MenuEntry::Submenu { entries, .. }) = find(&menu, "Cherry-pick") else { panic!("submenu") };
        assert_eq!(labels(entries), ["`feat` (current)", "`main`"]);
        assert!(entries.contains(&item("`main`", MenuAction::CherryPickSelection { onto: "main".into() })));
    }

    #[test]
    fn selection_menu_hides_squash_and_disables_revert_when_they_do_not_apply() {
        let locals = s(&["main"]);
        let cx = SelectionContext {
            count: 2,
            current_branch: None,
            local_branches: &locals,
            squashable: false,
            has_merge: true,
            in_current: false,
        };
        let menu = selection_menu(&cx);
        assert!(find(&menu, "Squash").is_none());
        assert_eq!(
            find(&menu, "Revert"),
            Some(&MenuEntry::Item {
                label: "Revert 2 commits".into(),
                action: MenuAction::RevertSelection,
                enabled: false
            })
        );
        assert!(
            matches!(find(&menu, "Cherry-pick"), Some(MenuEntry::Item { enabled: false, .. })),
            "a merge can't be cherry-picked without -m"
        );
        let cx = SelectionContext { has_merge: false, ..cx };
        let menu = selection_menu(&cx);
        let Some(MenuEntry::Submenu { entries, .. }) = find(&menu, "Cherry-pick") else { panic!("submenu") };
        assert_eq!(labels(entries), ["`main`"], "detached: every local branch, none marked current");
        let both = s(&["main", "feat"]);
        let cx =
            SelectionContext { current_branch: Some("main"), in_current: true, local_branches: &both, ..cx };
        let menu = selection_menu(&cx);
        let Some(MenuEntry::Submenu { entries, .. }) = find(&menu, "Cherry-pick") else { panic!("submenu") };
        assert_eq!(labels(entries), ["`feat`"], "commits already on `main` are not picked onto it");
    }

    #[test]
    fn reaches_walks_parents_and_knows_when_it_is_sure() {
        let graph: std::collections::HashMap<&str, Vec<String>> = [
            ("d", s(&["b", "c"])),
            ("c", s(&["a"])),
            ("b", s(&["a"])),
            ("a", vec![]),
            ("x", s(&["unloaded"])),
        ]
        .into_iter()
        .collect();
        let parents = |id: &str| graph.get(id).map(Vec::as_slice);
        assert_eq!(reaches("d", "a", parents), Some(true));
        assert_eq!(reaches("b", "c", parents), Some(false), "every reachable commit was loaded");
        assert_eq!(reaches("x", "a", parents), None, "history beyond the loaded window");
    }

    #[test]
    fn reset_submenu_has_soft_mixed_hard_in_order() {
        let f = Fixture::new();
        let menu = commit_menu("c9", &RowRefs::default(), &f.cx());
        let Some(MenuEntry::Submenu { entries, .. }) = find(&menu, "Reset") else { panic!() };
        let actions: Vec<_> = entries
            .iter()
            .filter_map(|e| match e {
                MenuEntry::Item { action, .. } => Some(action.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            actions,
            [
                MenuAction::Reset(ResetMode::Soft),
                MenuAction::Reset(ResetMode::Mixed),
                MenuAction::Reset(ResetMode::Hard)
            ]
        );
    }

    #[test]
    fn push_to_lists_every_remote() {
        let f = Fixture::new();
        let row = refs(vec![Decoration::Branch("feat".into())]);
        let menu = commit_menu("c1", &row, &f.cx());
        let Some(MenuEntry::Submenu { entries, .. }) = find(&menu, "Push to") else { panic!() };
        assert_eq!(labels(entries), ["origin", "upstream"]);
    }

    #[test]
    fn no_remotes_means_no_push_group() {
        let f = Fixture::new();
        let mut cx = f.cx();
        cx.remotes = &[];
        let row = refs(vec![Decoration::Branch("feat".into())]);
        assert!(find(&commit_menu("c1", &row, &cx), "Push").is_none());
    }

    #[test]
    fn without_a_forge_url_permalink_and_browser_are_hidden() {
        let f = Fixture::new();
        let mut cx = f.cx();
        cx.web = false;
        let got = labels(&commit_menu("c1", &RowRefs::default(), &cx));
        assert_eq!(got.last().map(String::as_str), Some("Copy subject"));
    }

    #[test]
    fn detached_head_names_head_in_merge_and_reset() {
        let f = Fixture::new();
        let mut cx = f.cx();
        cx.current_branch = None;
        let row = refs(vec![Decoration::Branch("feat".into())]);
        let got = labels(&commit_menu("c1", &row, &cx));
        assert!(got.contains(&"Merge `feat` into `HEAD`".to_string()), "{got:?}");
        assert!(got.contains(&"Reset `HEAD` to here ▸".to_string()));
    }

    // ---- checkout_choice ----

    #[test]
    fn checkout_prefers_a_local_branch_that_is_not_current() {
        let f = Fixture::new();
        let row = refs(vec![Decoration::CurrentBranch("main".into()), Decoration::Branch("feat".into())]);
        assert_eq!(checkout_choice("h0", &row, &f.cx()), Some(CheckoutTarget::Branch("feat".into())));
    }

    #[test]
    fn checkout_of_a_remote_only_branch_creates_a_tracking_branch() {
        let f = Fixture::new();
        let row = refs(vec![Decoration::Remote("upstream/fix/x".into())]);
        assert_eq!(
            checkout_choice("c2", &row, &f.cx()),
            Some(CheckoutTarget::Remote { remote: "upstream".into(), branch: "fix/x".into() })
        );
    }

    #[test]
    fn remote_branch_with_a_local_namesake_is_still_a_remote_checkout_that_asks() {
        let f = Fixture::new();
        let row = refs(vec![Decoration::Remote("origin/feat".into())]);
        assert_eq!(
            checkout_choice("c2", &row, &f.cx()),
            Some(CheckoutTarget::Remote { remote: "origin".into(), branch: "feat".into() })
        );
        // `origin/main` where HEAD is on `main`: nothing to ask about, so the commit detached.
        let row = refs(vec![Decoration::Remote("origin/main".into())]);
        assert_eq!(checkout_choice("c2", &row, &f.cx()), Some(CheckoutTarget::Detached("c2".into())));
        // A remote branch without a namesake wins over one with.
        let row =
            refs(vec![Decoration::Remote("origin/feat".into()), Decoration::Remote("origin/new".into())]);
        assert_eq!(
            checkout_choice("c2", &row, &f.cx()),
            Some(CheckoutTarget::Remote { remote: "origin".into(), branch: "new".into() })
        );
    }

    #[test]
    fn remote_checkout_choices_run_the_right_ops() {
        assert_eq!(
            remote_checkout_ops("origin", "feat", RemoteCheckout::Local, ""),
            vec![Op::Checkout { branch: "feat".into(), force: false }]
        );
        assert_eq!(
            remote_checkout_ops("origin", "feat", RemoteCheckout::NewBranch, "origin-feat"),
            vec![
                Op::CreateBranch { name: "origin-feat".into(), base: "origin/feat".into(), checkout: true },
                Op::SetUpstream { branch: "origin-feat".into(), upstream: Some("origin/feat".into()) },
            ]
        );
        assert_eq!(
            remote_checkout_ops("origin", "feat", RemoteCheckout::Detached, ""),
            vec![Op::CheckoutDetached { rev: "origin/feat".into() }]
        );
    }

    #[test]
    fn tracking_name_suggestion_avoids_existing_names() {
        assert_eq!(tracking_name_suggestion("origin", "feat", &s(&["feat"])), "origin-feat");
        assert_eq!(tracking_name_suggestion("a/b", "x", &[]), "a-b-x");
        assert_eq!(
            tracking_name_suggestion("origin", "feat", &s(&["origin-feat", "origin-feat-2"])),
            "origin-feat-3"
        );
    }

    #[test]
    fn a_tag_only_row_offers_pushing_its_tag() {
        let f = Fixture::new();
        let row = refs(vec![Decoration::Tag("v1".into())]);
        let menu = commit_menu("c1", &row, &f.cx());
        assert!(matches!(
            find(&menu, "Push tag `v1` to origin"),
            Some(MenuEntry::Item { action: MenuAction::PushTag { remote: None, .. }, .. })
        ));
        assert!(
            matches!(find(&menu, "Push tag `v1` to…"), Some(MenuEntry::Submenu { entries, .. }) if entries.len() == 2)
        );
        // A row with a branch pushes the branch instead.
        let row = refs(vec![Decoration::Branch("feat".into()), Decoration::Tag("v1".into())]);
        assert!(find(&commit_menu("c1", &row, &f.cx()), "Push tag").is_none());
    }

    #[test]
    fn checkout_is_hidden_where_head_already_is() {
        let f = Fixture::new();
        let mut cx = f.cx();
        cx.current_branch = None;
        assert_eq!(checkout_choice("h0", &RowRefs::default(), &cx), None);
    }

    // ---- split_remote_branch / default_remote ----

    #[test]
    fn split_remote_prefers_the_longest_known_remote() {
        let remotes = s(&["origin", "origin/mirror"]);
        assert_eq!(
            split_remote_branch("origin/mirror/main", &remotes),
            Some(("origin/mirror".into(), "main".into()))
        );
        assert_eq!(split_remote_branch("origin/feat/x", &remotes), Some(("origin".into(), "feat/x".into())));
        assert_eq!(split_remote_branch("other/x", &remotes), Some(("other".into(), "x".into())));
        assert_eq!(split_remote_branch("nobranch", &remotes), None);
    }

    #[test]
    fn default_remote_from_upstream_then_origin_then_first() {
        let remotes = s(&["fork", "origin"]);
        assert_eq!(default_remote(None, Some("fork/main"), &remotes).as_deref(), Some("fork"));
        assert_eq!(default_remote(None, None, &remotes).as_deref(), Some("origin"));
        assert_eq!(default_remote(None, None, &s(&["fork"])).as_deref(), Some("fork"));
        assert_eq!(default_remote(None, None, &[]), None);
    }

    #[test]
    fn default_remote_is_the_bound_one_when_it_exists() {
        let remotes = s(&["fork", "origin"]);
        assert_eq!(default_remote(Some("fork"), Some("origin/main"), &remotes).as_deref(), Some("fork"));
        // Bound to a remote this clone doesn't have (renamed, removed): fall back.
        assert_eq!(default_remote(Some("gone"), None, &remotes).as_deref(), Some("origin"));
        // A repo with no remotes groups its workspaces under "".
        assert_eq!(default_remote(Some(""), None, &remotes).as_deref(), Some("origin"));
    }

    // ---- validation ----

    #[test]
    fn branch_name_validation_is_live_and_names_the_rule() {
        let existing = s(&["main"]);
        assert_eq!(branch_name_error("fix/refresh-401", &existing), None);
        assert_eq!(branch_name_error("", &existing).as_deref(), Some(""));
        assert_eq!(branch_name_error("a..b", &existing).as_deref(), Some("Ref name may not contain '..'"));
        assert_eq!(branch_name_error("main", &existing).as_deref(), Some("Branch `main` already exists"));
        assert!(branch_name_error("has space", &existing).is_some());
        assert!(branch_name_error("-x", &existing).is_some());
    }

    #[test]
    fn tag_and_remote_names() {
        assert_eq!(tag_name_error("v1.0", &s(&["v0.9"])), None);
        assert_eq!(tag_name_error("v0.9", &s(&["v0.9"])).as_deref(), Some("Tag `v0.9` already exists"));
        assert!(remote_name_error("a/b", &[]).is_some());
        assert_eq!(
            remote_name_error("origin", &s(&["origin"])).as_deref(),
            Some("Remote `origin` already exists")
        );
        assert_eq!(url_error("git@github.com:a/b.git"), None);
        assert!(url_error("--upload-pack=x").is_some());
    }

    // ---- pickers ----

    #[test]
    fn rank_candidates_puts_the_bound_remote_first_on_an_empty_query() {
        let c = s(&["upstream/main", "origin/zed", "origin/main"]);
        assert_eq!(
            rank_candidates(&c, Some("origin"), ""),
            s(&["origin/main", "origin/zed", "upstream/main"])
        );
        assert_eq!(rank_candidates(&c, Some("origin"), "upm")[0], "upstream/main");
        assert!(rank_candidates(&c, None, "qqq").is_empty());
    }

    #[test]
    fn mainline_choices_show_parent_subjects() {
        let got =
            mainline_choices(&[("aaaaaaa111".into(), Some("Main work".into())), ("bbbbbbb222".into(), None)]);
        assert_eq!(got, vec![(1, "-m 1 · aaaaaaa Main work".into()), (2, "-m 2 · bbbbbbb".into())]);
    }

    #[test]
    fn popover_sentences_state_the_exact_operation() {
        assert_eq!(merge_sentence("feat/oauth", Some("main")), "Merge `feat/oauth` into `main`");
        assert_eq!(rebase_sentence("main", Some("feat")), "Rebase `feat` onto `main`");
        assert_eq!(cherry_pick_sentence("ab12cd3ef", None), "Cherry-pick ab12cd3 onto `HEAD`");
    }

    // ---- confirmations ----

    #[test]
    fn hard_reset_lists_commits_then_lost_files() {
        let c = reset_confirm(
            Some("main"),
            "abcdef123",
            ResetMode::Hard,
            &[("1111111aa".into(), "Add retry".into())],
            &s(&["src/a.rs"]),
        );
        assert_eq!(c.kind, ConfirmKind::ResetHard);
        assert_eq!(c.title, "Reset `main` to abcdef1 (hard)?");
        assert_eq!(c.lost, s(&["1111111  Add retry", "src/a.rs"]));
        assert!(c.detail.contains("1 commit will leave `main`"), "{}", c.detail);
        assert!(c.detail.contains("1 file"));
    }

    #[test]
    fn soft_reset_lists_commits_but_not_files() {
        let c = reset_confirm(None, "abc", ResetMode::Soft, &[], &s(&["x"]));
        assert_eq!(c.kind, ConfirmKind::ResetSoft);
        assert!(c.lost.is_empty());
        assert_eq!(c.detail, "No commits leave `HEAD`; their changes stay staged.");
        let m = reset_confirm(Some("main"), "abc", ResetMode::Mixed, &[], &[]);
        assert_eq!(m.kind, ConfirmKind::ResetMixed);
    }

    #[test]
    fn delete_unmerged_branch_matches_w13() {
        let c = delete_branch_confirm(
            "feat/oauth",
            Some("main"),
            &[("ef56aaaa".into(), "Refresh tokens".into()), ("cd34bbbb".into(), "Add OAuth".into())],
            Some("origin/feat/oauth"),
        );
        assert_eq!(c.title, "Delete branch `feat/oauth`?");
        assert_eq!(
            c.detail,
            "2 commits are not on `main` and will be lost unless another ref points to them:"
        );
        assert_eq!(c.lost, s(&["ef56aaa  Refresh tokens", "cd34bbb  Add OAuth"]));
        assert_eq!(c.options.len(), 1);
        assert_eq!(c.options[0].label, "Also delete `origin/feat/oauth`");
        assert!(!c.options[0].checked);
        assert_eq!(c.verb, "Delete");
    }

    #[test]
    fn remote_branch_for_prefers_the_upstream_then_the_same_name() {
        let remote = s(&["origin/feat", "fork/other"]);
        assert_eq!(
            remote_branch_for("x", Some("fork/other"), Some("origin"), &remote).as_deref(),
            Some("fork/other")
        );
        assert_eq!(remote_branch_for("feat", None, Some("origin"), &remote).as_deref(), Some("origin/feat"));
        assert_eq!(
            remote_branch_for("feat", Some("gone/feat"), Some("origin"), &remote).as_deref(),
            Some("origin/feat")
        );
        assert_eq!(remote_branch_for("nope", None, Some("origin"), &remote), None);
    }

    #[test]
    fn delete_tag_offers_each_remote() {
        let c = delete_tag_confirm("v1", &s(&["origin", "fork"]));
        let opts: Vec<_> = c.options.iter().map(|o| o.label.as_str()).collect();
        assert_eq!(opts, ["Also delete on origin", "Also delete on fork"]);
    }

    #[test]
    fn remove_remote_and_worktree_list_what_goes() {
        let c = remove_remote_confirm("fork", &s(&["fork/main"]));
        assert_eq!(c.kind, ConfirmKind::RemoveRemote);
        assert_eq!(c.lost, s(&["fork/main"]));
        let w = remove_worktree_confirm("../wt", &s(&["a", "b"]));
        assert_eq!(w.kind, ConfirmKind::RemoveDirtyWorktree);
        assert!(w.detail.contains("2 files"));
    }

    #[test]
    fn lossy_undo_names_the_entry_and_uses_its_verb() {
        let c = lossy_undo_confirm(true, "merge feat", &s(&["a.txt"]));
        assert_eq!(c.title, "Undo merge feat?");
        assert_eq!(c.verb, "Undo");
        assert!(!c.kind.allows_dont_ask());
    }

    // ---- §5.4 item 8: history rewriting ----

    #[test]
    fn rewrite_group_only_for_commits_in_the_current_branchs_history() {
        let f = Fixture::new();
        let in_head = MenuContext { in_head: Some(true), ..f.cx() };
        assert_eq!(
            labels(&rewrite_entries(&in_head, 1)),
            [
                "Interactively rebase from here…",
                "Edit commit message…",
                "Squash into parent",
                "Fixup into parent"
            ]
        );
        assert_eq!(
            labels(&rewrite_entries(&in_head, 0)),
            ["Interactively rebase from here…", "Edit commit message…"],
            "a root commit has no parent to meld into"
        );
        assert_eq!(labels(&rewrite_entries(&in_head, 2)).len(), 2, "nor does a merge, as one");
        assert!(rewrite_entries(&MenuContext { in_head: Some(false), ..f.cx() }, 1).is_empty());
        assert!(rewrite_entries(&MenuContext { in_head: None, ..f.cx() }, 1).is_empty(), "unknown: hidden");
        let detached = MenuContext { current_branch: None, in_head: Some(true), ..f.cx() };
        assert!(rewrite_entries(&detached, 1).is_empty(), "rebasing needs a branch");
    }

    #[test]
    fn rewrite_group_goes_after_undo_redo() {
        let f = Fixture::new();
        let cx = MenuContext { in_head: Some(true), ..f.cx() };
        let mut menu = commit_menu("c1", &refs(vec![Decoration::Branch("feat".into())]), &cx);
        insert_group_after_undo(&mut menu, rewrite_entries(&cx, 1));
        let got = labels(&menu);
        let redo = got.iter().position(|l| l.starts_with("Redo")).expect("redo");
        assert_eq!(got[redo + 1], "---");
        assert_eq!(got[redo + 2], "Interactively rebase from here…");
        assert_eq!(got[redo + 5], "Fixup into parent");
        assert_eq!(got[redo + 6], "---", "separated from the next group");
        assert!(!got.windows(2).any(|w| w[0] == "---" && w[1] == "---"));

        let mut empty = Vec::new();
        insert_group_after_undo(&mut empty, vec![item("x", MenuAction::EditMessage)]);
        assert_eq!(labels(&empty), ["x"], "no undo group: appended");
    }
}
