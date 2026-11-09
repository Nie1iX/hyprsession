//! The placement queue: tiled windows are claimed as they appear but placed per workspace,
//! all at once, in a deliberate order. Dwindle's tree depends on the order windows are
//! inserted, so placing each window the moment it maps (arrival order = app launch latency)
//! cannot reproduce a layout. Pure bookkeeping: no compositor access, no ordering logic.

use crate::ipc::Client;
use crate::load::ExecEntry;
use hyprland::shared::Address;
use std::collections::{HashMap, HashSet};

/// Canonical dispatch selector; display-name changes do not change identity.
pub type WsKey = String;

pub fn ws_key(client: &Client) -> &str {
    client.workspace.selector()
}

/// A claimed, tiled live window waiting for its workspace to flush
#[derive(Clone, Debug)]
pub struct Held {
    /// Index of the claimed saved client in `LoadPlan::saved`
    pub saved_index: usize,
    pub saved: Client,
    pub live: Address,
}

#[derive(Debug, Default)]
pub struct PlacementQueue {
    enabled: bool,
    /// Launches are still to run: a workspace then waits for every tiled save (or the end of the
    /// load) rather than for a launch phase to end, because windows arrive after their phase
    launching: bool,
    /// Tiled saved clients per workspace
    expected: HashMap<WsKey, usize>,
    held: HashMap<WsKey, Vec<Held>>,
    /// Launch entries that may still produce a window for the workspace
    pending: HashMap<WsKey, usize>,
    /// Entries known to have a group with unknown workspaces (no group info): pending everywhere
    ungrouped_pending: usize,
    flushed: HashSet<WsKey>,
}

/// Tiled windows are placed together on each numbered, named or special workspace.
pub fn is_queued(saved: &Client) -> bool {
    !saved.floating
}

impl PlacementQueue {
    /// A queue that holds nothing (legacy loads: windows are placed by their launch rules)
    pub fn disabled() -> Self {
        PlacementQueue::default()
    }

    /// `entries`: the launch entries still to run (empty when only adjusting)
    pub fn new(saved: &[Client], entries: &[ExecEntry]) -> Self {
        let mut queue = PlacementQueue {
            enabled: true,
            launching: !entries.is_empty(),
            ..Default::default()
        };
        for client in saved.iter().filter(|c| is_queued(c)) {
            *queue.expected.entry(ws_key(client).to_owned()).or_default() += 1;
        }
        for entry in entries {
            match &entry.group {
                Some(group) => {
                    for key in Self::group_workspaces(saved, &group.saved) {
                        *queue.pending.entry(key).or_default() += 1;
                    }
                }
                None => queue.ungrouped_pending += 1,
            }
        }
        queue
    }

    fn group_workspaces(saved: &[Client], indices: &[usize]) -> HashSet<WsKey> {
        indices
            .iter()
            .filter_map(|&i| saved.get(i))
            .filter(|c| is_queued(c))
            .map(|c| ws_key(c).to_owned())
            .collect()
    }

    /// Whether a claim for this saved client is held rather than placed on the spot
    pub fn holds(&self, saved: &Client) -> bool {
        self.enabled && is_queued(saved) && !self.flushed.contains(ws_key(saved))
    }

    pub fn hold(&mut self, held: Held) {
        self.held
            .entry(ws_key(&held.saved).to_owned())
            .or_default()
            .push(held);
    }

    pub fn is_launching(&self) -> bool {
        self.launching
    }

    pub fn launches_sent(&mut self) {
        self.launching = false;
    }

    pub fn held_count(&self) -> usize {
        self.held.values().map(Vec::len).sum()
    }

    /// An entry's phase is over: it will produce no more windows
    pub fn entry_done(&mut self, saved: &[Client], entry: &ExecEntry) {
        match &entry.group {
            Some(group) => {
                for key in Self::group_workspaces(saved, &group.saved) {
                    if let Some(n) = self.pending.get_mut(&key) {
                        *n = n.saturating_sub(1);
                    }
                }
            }
            None => self.ungrouped_pending = self.ungrouped_pending.saturating_sub(1),
        }
    }

    fn pending_for(&self, key: &WsKey) -> usize {
        self.pending.get(key).copied().unwrap_or(0) + self.ungrouped_pending
    }

    /// Workspaces to flush now, in a stable order: every tiled save claimed. With nothing left to
    /// launch (adjust-only) whatever is held is placed at once. While launching, an incomplete
    /// workspace waits for the end of the load: a launch phase ending does not mean its window
    /// has arrived (slow apps, other launches' late windows), and a workspace flushed before its
    /// full membership exists cannot regrow its tree.
    pub fn ready(&self) -> Vec<WsKey> {
        let mut keys: Vec<WsKey> = self
            .held
            .iter()
            .filter(|(key, held)| {
                !held.is_empty()
                    && !self.flushed.contains(*key)
                    && (held.len() >= self.expected.get(*key).copied().unwrap_or(0)
                        || (!self.launching && self.pending_for(key) == 0))
            })
            .map(|(key, _)| key.clone())
            .collect();
        keys.sort();
        keys
    }

    /// Every workspace that still holds windows (the final safety net)
    pub fn all_held(&self) -> Vec<WsKey> {
        let mut keys: Vec<WsKey> = self
            .held
            .iter()
            .filter(|(_, h)| !h.is_empty())
            .map(|(k, _)| k.clone())
            .collect();
        keys.sort();
        keys
    }

    /// The held windows of a workspace, in claim order; the workspace is flushed from now on
    pub fn take(&mut self, key: &WsKey) -> Vec<Held> {
        self.flushed.insert(key.clone());
        self.held.remove(key).unwrap_or_default()
    }

    pub fn is_flushed(&self, key: &WsKey) -> bool {
        self.flushed.contains(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::load::{ExecRules, GroupInfo};

    fn saved(ws: i32, name: &str, floating: bool) -> Client {
        let mut c = crate::load::placeholder_client();
        c.workspace = if name.starts_with("special:") {
            crate::ipc::Workspace::from_selector(name)
        } else {
            crate::ipc::Workspace::numbered(ws)
        };
        c.workspace.name = name.to_string();
        c.floating = floating;
        c
    }

    fn entry(indices: Vec<usize>) -> ExecEntry {
        ExecEntry {
            rules: ExecRules::default(),
            command: "x".into(),
            group: Some(GroupInfo {
                label: "x".into(),
                saved: indices,
            }),
        }
    }

    fn held(i: usize, saved: Client) -> Held {
        Held {
            saved_index: i,
            saved,
            live: Address::new(format!("0x{i}")),
        }
    }

    #[test]
    fn renamed_workspace_keeps_placement_identity() {
        let before = saved(3, "Work", false);
        let mut after = before.clone();
        after.workspace.name = "Focus".into();
        assert_eq!(ws_key(&before), ws_key(&after));
    }

    #[test]
    fn test_floating_windows_and_disabled_queue_are_not_held() {
        let queue = PlacementQueue::new(&[], &[]);
        assert!(!queue.holds(&saved(1, "1", true)));
        assert!(queue.holds(&saved(1, "1", false)));
        assert!(!PlacementQueue::disabled().holds(&saved(1, "1", false)));
    }

    #[test]
    fn test_flushes_when_every_tiled_save_is_claimed() {
        let s = vec![
            saved(2, "2", false),
            saved(2, "2", false),
            saved(2, "2", true),
        ];
        let mut queue = PlacementQueue::new(&s, &[entry(vec![0, 1, 2])]);
        queue.hold(held(0, s[0].clone()));
        assert!(queue.ready().is_empty());
        queue.hold(held(1, s[1].clone()));
        // the floating save does not count towards "complete"
        assert_eq!(queue.ready(), vec!["2".to_string()]);
    }

    #[test]
    fn test_an_incomplete_workspace_waits_for_the_end_of_the_load_while_launching() {
        let s = vec![saved(2, "2", false), saved(2, "2", false)];
        let e = entry(vec![0, 1]);
        let mut queue = PlacementQueue::new(&s, std::slice::from_ref(&e));
        queue.hold(held(0, s[0].clone()));
        assert!(
            queue.ready().is_empty(),
            "an entry is still pending for the workspace"
        );
        queue.entry_done(&s, &e);
        assert!(
            queue.ready().is_empty(),
            "the phase ending does not mean the window has arrived"
        );
        assert_eq!(
            queue.all_held(),
            vec!["2".to_string()],
            "the end-of-load flush still takes it"
        );
    }

    #[test]
    fn test_an_entry_for_another_workspace_does_not_hold_this_one_back() {
        let s = vec![
            saved(1, "1", false),
            saved(1, "1", false),
            saved(2, "2", false),
        ];
        let e1 = entry(vec![0]);
        let e2 = entry(vec![2]);
        let mut queue = PlacementQueue::new(&s, &[e1.clone(), e2.clone()]);
        queue.hold(held(0, s[0].clone()));
        queue.entry_done(&s, &e1);
        assert!(
            queue.ready().is_empty(),
            "ws 1 has one tiled save still to come"
        );
        queue.hold(held(1, s[1].clone()));
        assert_eq!(
            queue.ready(),
            vec!["1".to_string()],
            "complete: flushed without waiting for ws 2's launch"
        );
    }

    #[test]
    fn test_a_group_spanning_workspaces_is_pending_for_each() {
        let s = vec![saved(1, "1", false), saved(2, "2", false)];
        let e = entry(vec![0, 1]);
        let mut queue = PlacementQueue::new(&s, std::slice::from_ref(&e));
        queue.hold(held(0, s[0].clone()));
        queue.hold(held(1, s[1].clone()));
        assert_eq!(queue.ready().len(), 2, "both complete");
        let mut queue = PlacementQueue::new(
            &[s[0].clone(), saved(2, "2", false), saved(2, "2", false)],
            std::slice::from_ref(&e),
        );
        queue.hold(held(0, s[0].clone()));
        queue.entry_done(&s, &e);
        assert_eq!(
            queue.ready(),
            vec!["1".to_string()],
            "ws 1 is complete; ws 2 still waits"
        );
    }

    #[test]
    fn test_group_less_entry_is_pending_everywhere() {
        let s = vec![saved(1, "1", false), saved(1, "1", false)];
        let e = ExecEntry {
            rules: ExecRules::default(),
            command: "x".into(),
            group: None,
        };
        let mut queue = PlacementQueue::new(&s, std::slice::from_ref(&e));
        queue.hold(held(0, s[0].clone()));
        assert!(queue.ready().is_empty());
        queue.entry_done(&s, &e);
        assert!(
            queue.ready().is_empty(),
            "launching: waits for the full set"
        );
        queue.hold(held(1, s[1].clone()));
        assert_eq!(queue.ready().len(), 1);
    }

    #[test]
    fn test_adjust_only_has_nothing_pending() {
        let s = vec![saved(1, "1", false), saved(1, "1", false)];
        let mut queue = PlacementQueue::new(&s, &[]);
        queue.hold(held(0, s[0].clone()));
        assert_eq!(queue.ready().len(), 1);
    }

    #[test]
    fn test_a_workspace_flushes_once_and_later_claims_are_not_held() {
        let s = vec![saved(1, "1", false), saved(1, "1", false)];
        let mut queue = PlacementQueue::new(&s, &[]);
        queue.hold(held(0, s[0].clone()));
        let taken = queue.take(&"1".to_string());
        assert_eq!(taken.len(), 1);
        assert!(queue.is_flushed(&"1".to_string()));
        assert!(!queue.holds(&s[1]));
        assert!(queue.ready().is_empty());
        assert_eq!(queue.held_count(), 0);
    }

    #[test]
    fn test_named_specials_with_one_id_are_separate_workspaces() {
        let s = vec![
            saved(-98, "special:a", false),
            saved(-98, "special:b", false),
        ];
        let mut queue = PlacementQueue::new(&s, &[]);
        queue.hold(held(0, s[0].clone()));
        assert_eq!(queue.ready(), vec!["special:a".to_string()]);
        queue.take(&"special:a".to_string());
        assert!(queue.holds(&s[1]));
    }
}
