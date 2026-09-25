//! Listening-port detection (§5.27 "Port detection", W18): every 5 s while workspaces exist,
//! find the TCP ports in LISTEN state owned by any process under each workspace's shells, on a
//! worker. New ports toast once ("Listening on :3000"); all appear in the sidebar and status bar.

use super::{App, Msg};
use crate::ui::chrome::Toast;
use crate::ui::jobs;
use std::collections::{HashMap, HashSet};

pub(super) const SCAN_EVERY_SECS: f64 = 5.0;

impl App {
    pub(super) fn scan_ports(&mut self, ctx: &egui::Context) {
        if self.time - self.last_port_scan < SCAN_EVERY_SECS || self.live.is_empty() {
            return;
        }
        self.last_port_scan = self.time;
        let roots: Vec<(String, u32)> = self
            .live
            .iter()
            .flat_map(|(ws, live)| live.panes.values().map(move |t| (ws.clone(), t.pid())))
            .collect();
        jobs::spawn(ctx, &self.tx, move || Msg::Ports(listening_by_workspace(&roots)));
        ctx.request_repaint_after(std::time::Duration::from_secs_f64(SCAN_EVERY_SECS));
    }

    pub(super) fn on_ports(&mut self, found: HashMap<String, Vec<u16>>) {
        for (ws, ports) in &found {
            let known: HashSet<u16> = self.ports.get(ws).into_iter().flatten().copied().collect();
            for port in ports.iter().filter(|p| !known.contains(p)) {
                self.toast(Toast::info(format!("Listening on :{port}")));
            }
        }
        self.ports = found;
    }
}

/// Ports per workspace, from the shells' whole process trees.
fn listening_by_workspace(roots: &[(String, u32)]) -> HashMap<String, Vec<u16>> {
    let mut sys = sysinfo::System::new();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    let parents: Vec<(u32, u32)> = sys
        .processes()
        .iter()
        .filter_map(|(pid, p)| p.parent().map(|parent| (pid.as_u32(), parent.as_u32())))
        .collect();
    let owner = owners(roots, &parents);
    let pids: Vec<u32> = owner.keys().copied().collect();
    let mut out: HashMap<String, Vec<u16>> = HashMap::new();
    for (pid, port) in crate::platform::listening_ports(&pids) {
        if let Some(ws) = owner.get(&pid) {
            let ports = out.entry(ws.clone()).or_default();
            if !ports.contains(&port) {
                ports.push(port);
            }
        }
    }
    out.values_mut().for_each(|p| p.sort_unstable());
    out
}

/// Map every pid in the trees rooted at `roots` to its root's workspace.
fn owners(roots: &[(String, u32)], parent_of: &[(u32, u32)]) -> HashMap<u32, String> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for &(pid, parent) in parent_of {
        children.entry(parent).or_default().push(pid);
    }
    let mut owner = HashMap::new();
    for (ws, root) in roots {
        let mut stack = vec![*root];
        while let Some(pid) = stack.pop() {
            if owner.insert(pid, ws.clone()).is_none() {
                stack.extend(children.get(&pid).into_iter().flatten().copied());
            }
        }
    }
    owner
}

#[cfg(test)]
mod tests {
    use super::owners;

    #[test]
    fn owners_cover_whole_process_trees() {
        // 10 → 11 → 12 (w1), 20 → 21 (w2), 30 unrelated.
        let parents = [(11, 10), (12, 11), (21, 20), (31, 30)];
        let owner = owners(&[("w1".into(), 10), ("w2".into(), 20)], &parents);
        assert_eq!(owner.get(&12).map(String::as_str), Some("w1"));
        assert_eq!(owner.get(&21).map(String::as_str), Some("w2"));
        assert!(!owner.contains_key(&31));
    }

    #[test]
    fn owners_tolerate_cycles() {
        let owner = owners(&[("w".into(), 1)], &[(2, 1), (1, 2)]);
        assert_eq!(owner.len(), 2);
    }
}
