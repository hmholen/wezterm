//! Remote tabs: attaching a client domain *into* a local tab.
//!
//! The classic attach mirrors every remote window as a local window, in a
//! local workspace named after the remote one. In this mode the remote
//! side lives inside ordinary local tabs instead, the way `ssh host` and
//! `tmux attach` live inside one tab of a terminal emulator:
//!
//! * Each *slot* is a position in a local window that shows one remote
//!   tab of one remote *session* (a workspace on the server). The local
//!   tab that was there before stays alive, hidden, and is put back when
//!   the slot is detached.
//! * Every remote tab still gets a local mirror `Tab` with `ClientPane`s,
//!   exactly as in the classic mode, but only the ones shown in a slot
//!   are placed in a window. The rest stay in the mux without a window,
//!   so switching between them is instant.
//! * Switching session or remote tab swaps which mirror sits in the
//!   slot's position; other local tabs are not affected.

use super::ClientDomain;
use crate::pane::ClientPane;
use anyhow::{anyhow, bail};
use codec::SpawnV2;
use config::keyassignment::SpawnTabDomain;
use mux::domain::{Domain, DomainId, DomainState};
use mux::pane::Pane;
use mux::tab::{Tab, TabId};
use mux::window::WindowId;
use mux::Mux;
use std::collections::HashSet;
use std::convert::TryFrom;
use std::sync::Arc;
use wezterm_term::TerminalSize;

/// A remote tab as of the most recent pane list from the server.
#[derive(Clone, Debug)]
pub(super) struct RemoteTabInfo {
    pub remote_tab_id: TabId,
    pub remote_window_id: WindowId,
    pub workspace: String,
}

#[derive(Clone, Debug)]
struct Slot {
    window_id: WindowId,
    /// The local tab that the slot replaced; put back on detach.
    local_tab_id: TabId,
    /// The local mirror of `remote_tab_id`, which is what the window holds.
    view_tab_id: TabId,
    remote_tab_id: TabId,
    session: String,
}

#[derive(Default)]
pub(super) struct InTabState {
    /// The domain was attached with `attach_in_tab` and does not mirror
    /// remote windows as local windows.
    pub enabled: bool,
    slots: Vec<Slot>,
    /// Remote tabs in the order the server lists them.
    pub remote_tabs: Vec<RemoteTabInfo>,
}

/// True when every pane of `tab` is dead or gone from the mux, which is
/// when `Mux::prune_dead_windows` would remove the tab.
fn tab_is_gone(tab: &Tab) -> bool {
    let mux = Mux::get();
    tab.iter_panes_ignoring_zoom()
        .iter()
        .all(|p| p.pane.is_dead() || mux.get_pane(p.pane.pane_id()).is_none())
}

/// Put `new_tab` in the window position held by `old_tab_id`, at the old
/// tab's size.
fn swap_in_window(window_id: WindowId, old_tab_id: TabId, new_tab: &Arc<Tab>) -> anyhow::Result<()> {
    let mux = Mux::get();
    let size = {
        let mut window = mux
            .get_window_mut(window_id)
            .ok_or_else(|| anyhow!("window {window_id} is gone"))?;
        let idx = window
            .get_tab_idx_for_id(old_tab_id)
            .ok_or_else(|| anyhow!("tab {old_tab_id} is not in window {window_id}"))?;
        window.replace_tab_at_idx(idx, new_tab).get_size()
    };
    new_tab.resize(size);
    Ok(())
}

/// Remove a tab later, outside of whatever mux operation is running now
/// (`Mux::remove_tab` prunes windows, which may be what called us).
fn remove_tab_later(tab_id: TabId) {
    promise::spawn::spawn_into_main_thread(async move {
        if let Some(mux) = Mux::try_get() {
            mux.remove_tab(tab_id);
        }
    })
    .detach();
}

fn detach_later(domain_id: DomainId) {
    promise::spawn::spawn_into_main_thread(async move {
        if let Some(mux) = Mux::try_get() {
            if let Some(domain) = mux.get_domain(domain_id) {
                if let Some(domain) = domain.downcast_ref::<ClientDomain>() {
                    if domain.in_tab.lock().unwrap().slots.is_empty() {
                        domain.perform_detach();
                    }
                }
            }
        }
    })
    .detach();
}

/// Put the local tab back in place of the slot's remote tab. If the local
/// tab is gone (its shell exited while hidden), just take the remote tab
/// out of the window.
fn restore_slot(slot: &Slot) {
    let mux = Mux::get();
    if mux.get_window(slot.window_id).is_none() {
        // The window was closed; the hidden local tab goes with it.
        remove_tab_later(slot.local_tab_id);
        return;
    }
    match mux.get_tab(slot.local_tab_id) {
        Some(local) if !tab_is_gone(&local) => {
            if let Err(err) = swap_in_window(slot.window_id, slot.view_tab_id, &local) {
                log::error!("restoring local tab {}: {err:#}", slot.local_tab_id);
            }
        }
        _ => {
            if let Some(mut window) = mux.get_window_mut(slot.window_id) {
                window.remove_tab_id(slot.view_tab_id);
            }
        }
    }
}

impl ClientDomain {
    pub fn is_in_tab_mode(&self) -> bool {
        self.in_tab.lock().unwrap().enabled
    }

    /// The remote session shown in local tab `tab_id`, if that tab is a
    /// remote tab of this domain.
    pub fn session_for_tab(&self, tab_id: TabId) -> Option<String> {
        self.in_tab
            .lock()
            .unwrap()
            .slots
            .iter()
            .find(|s| s.view_tab_id == tab_id)
            .map(|s| s.session.clone())
    }

    /// The remote sessions, in the order the server lists them.
    pub fn remote_sessions(&self) -> Vec<String> {
        let state = self.in_tab.lock().unwrap();
        let mut sessions: Vec<String> = vec![];
        for info in &state.remote_tabs {
            if !sessions.contains(&info.workspace) {
                sessions.push(info.workspace.clone());
            }
        }
        sessions
    }

    fn local_tab_for_remote(&self, remote_tab_id: TabId) -> Option<Arc<Tab>> {
        let inner = self.inner()?;
        let tab_id = inner.remote_to_local_tab_id(remote_tab_id)?;
        Mux::get().get_tab(tab_id)
    }

    fn slot_for_view(&self, view_tab_id: TabId) -> anyhow::Result<Slot> {
        self.in_tab
            .lock()
            .unwrap()
            .slots
            .iter()
            .find(|s| s.view_tab_id == view_tab_id)
            .cloned()
            .ok_or_else(|| anyhow!("tab {view_tab_id} is not a remote tab"))
    }

    /// Remote tabs of `session` that are alive, in server order, leaving
    /// out those shown by slots other than the one showing `view_tab_id`.
    fn selectable_tabs(&self, session: &str, view_tab_id: Option<TabId>) -> Vec<TabId> {
        let (in_session, shown_elsewhere): (Vec<TabId>, HashSet<TabId>) = {
            let state = self.in_tab.lock().unwrap();
            (
                state
                    .remote_tabs
                    .iter()
                    .filter(|info| info.workspace == session)
                    .map(|info| info.remote_tab_id)
                    .collect(),
                state
                    .slots
                    .iter()
                    .filter(|s| Some(s.view_tab_id) != view_tab_id)
                    .map(|s| s.remote_tab_id)
                    .collect(),
            )
        };
        in_session
            .into_iter()
            .filter(|remote| !shown_elsewhere.contains(remote))
            .filter(|&remote| {
                self.local_tab_for_remote(remote)
                    .map(|tab| !tab_is_gone(&tab))
                    .unwrap_or(false)
            })
            .collect()
    }

    fn remote_window_of_session(&self, session: &str, prefer_tab: Option<TabId>) -> Option<WindowId> {
        let state = self.in_tab.lock().unwrap();
        let in_session = || state.remote_tabs.iter().filter(|i| i.workspace == session);
        in_session()
            .find(|i| Some(i.remote_tab_id) == prefer_tab)
            .or_else(|| in_session().next())
            .map(|i| i.remote_window_id)
    }

    fn new_session_name(&self) -> String {
        let existing = self.remote_sessions();
        let mux = Mux::get();
        loop {
            let name = mux.generate_workspace_name();
            if !existing.contains(&name) {
                return name;
            }
        }
    }

    /// Show remote tab `remote_tab_id` in the slot whose window currently
    /// holds `view_tab_id`.
    fn show_in_slot(&self, view_tab_id: TabId, remote_tab_id: TabId) -> anyhow::Result<()> {
        let slot = self.slot_for_view(view_tab_id)?;
        let new_tab = self
            .local_tab_for_remote(remote_tab_id)
            .ok_or_else(|| anyhow!("remote tab {remote_tab_id} has no local tab"))?;
        if new_tab.tab_id() != view_tab_id {
            swap_in_window(slot.window_id, view_tab_id, &new_tab)?;
        }
        let mut state = self.in_tab.lock().unwrap();
        let session = state
            .remote_tabs
            .iter()
            .find(|i| i.remote_tab_id == remote_tab_id)
            .map(|i| i.workspace.clone())
            .unwrap_or(slot.session);
        if let Some(slot) = state
            .slots
            .iter_mut()
            .find(|s| s.view_tab_id == view_tab_id)
        {
            slot.view_tab_id = new_tab.tab_id();
            slot.remote_tab_id = remote_tab_id;
            slot.session = session;
        }
        Ok(())
    }

    /// Create a new remote tab in `session` (a new session if the server
    /// has no such workspace) and its local mirror, which is not placed in
    /// any window. Returns the remote tab id.
    async fn spawn_remote_tab(
        &self,
        session: &str,
        remote_window_id: Option<WindowId>,
        size: TerminalSize,
    ) -> anyhow::Result<TabId> {
        let inner = self
            .inner()
            .ok_or_else(|| anyhow!("domain is not attached"))?;
        let result = inner
            .client
            .spawn_v2(SpawnV2 {
                domain: SpawnTabDomain::DefaultDomain,
                window_id: remote_window_id,
                size,
                command: None,
                command_dir: None,
                workspace: session.to_string(),
            })
            .await?;

        let pane: Arc<dyn Pane> = Arc::new(ClientPane::new(
            &inner,
            result.tab_id,
            result.pane_id,
            size,
            "wezterm",
        ));
        let tab = Arc::new(Tab::new(&size));
        tab.assign_pane(&pane);
        inner.remove_old_tab_mapping(result.tab_id);
        inner.record_remote_to_local_tab_mapping(result.tab_id, tab.tab_id());
        Mux::get().add_tab_and_active_pane(&tab)?;

        let mut state = self.in_tab.lock().unwrap();
        if !state
            .remote_tabs
            .iter()
            .any(|i| i.remote_tab_id == result.tab_id)
        {
            state.remote_tabs.push(RemoteTabInfo {
                remote_tab_id: result.tab_id,
                remote_window_id: result.window_id,
                workspace: session.to_string(),
            });
        }
        Ok(result.tab_id)
    }

    /// Turn local tab `tab_id` of `window_id` into a remote tab: connect if
    /// needed, then show a remote session in its place. The local tab is
    /// kept and comes back with `detach_slot`.
    pub async fn attach_in_tab(&self, window_id: WindowId, tab_id: TabId) -> anyhow::Result<()> {
        if self.session_for_tab(tab_id).is_some() {
            return Ok(());
        }
        match self.state() {
            DomainState::Attached => {
                if !self.is_in_tab_mode() {
                    bail!(
                        "domain `{}` is attached in its own windows; detach it first",
                        self.domain_name()
                    );
                }
            }
            DomainState::Detached => {
                self.in_tab.lock().unwrap().enabled = true;
                if let Err(err) = self.attach(Some(window_id)).await {
                    self.in_tab.lock().unwrap().enabled = false;
                    return Err(err);
                }
            }
        }

        let size = Mux::get()
            .get_tab(tab_id)
            .ok_or_else(|| anyhow!("tab {tab_id} is gone"))?
            .get_size();

        // Prefer a session that no other tab shows, and `default` among those.
        let sessions = self.remote_sessions();
        let shown: HashSet<String> = self
            .in_tab
            .lock()
            .unwrap()
            .slots
            .iter()
            .map(|s| s.session.clone())
            .collect();
        let free: Vec<&String> = sessions.iter().filter(|s| !shown.contains(*s)).collect();
        let pick = free
            .iter()
            .find(|s| s.as_str() == "default")
            .or_else(|| free.first())
            .map(|s| s.to_string());

        let existing = pick
            .as_deref()
            .and_then(|s| self.selectable_tabs(s, None).first().copied());
        let remote_tab_id = match existing {
            Some(remote) => remote,
            None => {
                let session = match pick {
                    Some(s) => s,
                    None if sessions.is_empty() => "default".to_string(),
                    None => self.new_session_name(),
                };
                let remote_window = self.remote_window_of_session(&session, None);
                self.spawn_remote_tab(&session, remote_window, size).await?
            }
        };

        let new_tab = self
            .local_tab_for_remote(remote_tab_id)
            .ok_or_else(|| anyhow!("remote tab {remote_tab_id} has no local tab"))?;
        swap_in_window(window_id, tab_id, &new_tab)?;
        // The connection UI may have shown up as a tab of its own, and
        // removing it moves the focus to some other tab; come back here.
        if let Some(mut window) = Mux::get().get_window_mut(window_id) {
            if let Some(idx) = window.get_tab_idx_for_id(new_tab.tab_id()) {
                window.set_active_tab_idx_without_saving(idx);
            }
        }

        let mut state = self.in_tab.lock().unwrap();
        let session = state
            .remote_tabs
            .iter()
            .find(|i| i.remote_tab_id == remote_tab_id)
            .map(|i| i.workspace.clone())
            .unwrap_or_default();
        state.slots.push(Slot {
            window_id,
            local_tab_id: tab_id,
            view_tab_id: new_tab.tab_id(),
            remote_tab_id,
            session,
        });
        Ok(())
    }

    /// Show the remote tab `delta` places away within the session.
    pub fn session_tab_relative(&self, view_tab_id: TabId, delta: isize) -> anyhow::Result<()> {
        let slot = self.slot_for_view(view_tab_id)?;
        let tabs = self.selectable_tabs(&slot.session, Some(view_tab_id));
        if tabs.is_empty() {
            return Ok(());
        }
        let pos = tabs
            .iter()
            .position(|&t| t == slot.remote_tab_id)
            .unwrap_or(0) as isize;
        let next = tabs[(pos + delta).rem_euclid(tabs.len() as isize) as usize];
        self.show_in_slot(view_tab_id, next)
    }

    /// Show the `n`th remote tab of the session; negative counts from the end.
    pub fn activate_session_tab(&self, view_tab_id: TabId, n: isize) -> anyhow::Result<()> {
        let slot = self.slot_for_view(view_tab_id)?;
        let tabs = self.selectable_tabs(&slot.session, Some(view_tab_id));
        let idx = if n < 0 { tabs.len() as isize + n } else { n };
        match usize::try_from(idx).ok().and_then(|i| tabs.get(i)) {
            Some(&remote) => self.show_in_slot(view_tab_id, remote),
            None => Ok(()),
        }
    }

    /// Open a new remote tab in the session and show it.
    pub async fn spawn_session_tab(&self, view_tab_id: TabId) -> anyhow::Result<()> {
        let slot = self.slot_for_view(view_tab_id)?;
        let size = Mux::get()
            .get_tab(view_tab_id)
            .ok_or_else(|| anyhow!("tab {view_tab_id} is gone"))?
            .get_size();
        let remote_window = self.remote_window_of_session(&slot.session, Some(slot.remote_tab_id));
        let remote = self
            .spawn_remote_tab(&slot.session, remote_window, size)
            .await?;
        self.show_in_slot(view_tab_id, remote)
    }

    /// Show session `name` (a new session if `None` or if the server has no
    /// such workspace) in the slot.
    pub async fn switch_session(&self, view_tab_id: TabId, name: Option<String>) -> anyhow::Result<()> {
        let slot = self.slot_for_view(view_tab_id)?;
        let name = name.unwrap_or_else(|| self.new_session_name());
        if name == slot.session {
            return Ok(());
        }
        let size = Mux::get()
            .get_tab(view_tab_id)
            .ok_or_else(|| anyhow!("tab {view_tab_id} is gone"))?
            .get_size();
        let remote = match self.selectable_tabs(&name, Some(view_tab_id)).first() {
            Some(&remote) => remote,
            None => {
                // Every tab of that session is shown elsewhere (or there is
                // no such session): give this slot a tab of its own.
                let remote_window = self.remote_window_of_session(&name, None);
                self.spawn_remote_tab(&name, remote_window, size).await?
            }
        };
        self.show_in_slot(view_tab_id, remote)
    }

    /// Rename the remote session shown in `view_tab_id` on the server.
    pub async fn rename_session(&self, view_tab_id: TabId, new_name: String) -> anyhow::Result<()> {
        let slot = self.slot_for_view(view_tab_id)?;
        if new_name.is_empty() || new_name == slot.session {
            return Ok(());
        }
        if self.remote_sessions().contains(&new_name) {
            bail!("there is already a remote session named `{new_name}`");
        }
        let inner = self
            .inner()
            .ok_or_else(|| anyhow!("domain is not attached"))?;
        inner
            .client
            .rename_workspace(codec::RenameWorkspace {
                old_workspace: slot.session.clone(),
                new_workspace: new_name.clone(),
            })
            .await?;
        self.remote_session_renamed(&slot.session, &new_name);
        Ok(())
    }

    /// A remote session was renamed (by us, or by the server telling us).
    /// In this mode remote sessions are not local workspaces, so only our
    /// own record of them changes.
    pub fn remote_session_renamed(&self, old_name: &str, new_name: &str) {
        let mut state = self.in_tab.lock().unwrap();
        for info in state.remote_tabs.iter_mut() {
            if info.workspace == old_name {
                info.workspace = new_name.to_string();
            }
        }
        for slot in state.slots.iter_mut() {
            if slot.session == old_name {
                slot.session = new_name.to_string();
            }
        }
    }

    /// Put the local tab back in place of the remote tab `view_tab_id`.
    /// Disconnects when no remote tabs are left.
    pub fn detach_slot(&self, view_tab_id: TabId) -> anyhow::Result<()> {
        let (slot, none_left) = {
            let mut state = self.in_tab.lock().unwrap();
            let idx = state
                .slots
                .iter()
                .position(|s| s.view_tab_id == view_tab_id)
                .ok_or_else(|| anyhow!("tab {view_tab_id} is not a remote tab"))?;
            let slot = state.slots.remove(idx);
            (slot, state.slots.is_empty())
        };
        restore_slot(&slot);
        if none_left {
            self.perform_detach();
        }
        Ok(())
    }

    /// Called from `perform_detach`: put every local tab back and return
    /// whether the domain was in this mode.
    pub(super) fn restore_all_slots(&self) -> bool {
        let (enabled, slots) = {
            let mut state = self.in_tab.lock().unwrap();
            let enabled = state.enabled;
            state.enabled = false;
            state.remote_tabs.clear();
            (enabled, std::mem::take(&mut state.slots))
        };
        for slot in &slots {
            restore_slot(slot);
        }
        enabled
    }

    /// Called from `detach` when the mux closes a window that has remote
    /// tabs. If remote tabs remain in other windows, forget the closed
    /// window's slots and stay connected; returns false if a full detach
    /// is needed instead.
    pub(super) fn detach_closed_windows_only(&self) -> bool {
        let mux = Mux::get();
        let gone: Vec<Slot> = {
            let mut state = self.in_tab.lock().unwrap();
            if !state.enabled {
                return false;
            }
            let (gone, remaining): (Vec<Slot>, Vec<Slot>) = state
                .slots
                .drain(..)
                .partition(|s| mux.get_window(s.window_id).is_none());
            if gone.is_empty() || remaining.is_empty() {
                state.slots = gone.into_iter().chain(remaining).collect();
                return false;
            }
            state.slots = remaining;
            gone
        };
        for slot in gone {
            // The mux is about to remove the window's tabs, which kills
            // their panes; keep the remote panes running, as a detach would.
            if let Some(tab) = mux.get_tab(slot.view_tab_id) {
                for pos in tab.iter_panes_ignoring_zoom() {
                    if let Some(pane) = pos.pane.downcast_ref::<ClientPane>() {
                        pane.ignore_next_kill();
                    }
                }
            }
            remove_tab_later(slot.local_tab_id);
        }
        true
    }

    /// After a pane list from the server: if a remote tab got a new local
    /// mirror, show that one; and pick up session renames.
    pub(super) fn reconcile_slots_after_sync(&self) {
        let slots = self.in_tab.lock().unwrap().slots.clone();
        for slot in slots {
            if let Err(err) = self.show_in_slot(slot.view_tab_id, slot.remote_tab_id) {
                log::debug!("remote tab {}: {err:#}", slot.remote_tab_id);
            }
        }
    }

    /// Before the mux prunes dead tabs: a slot whose remote tab died (the
    /// shell exited, or its last pane was closed) shows another tab of the
    /// session, or else gets its local tab back, so that the local tab
    /// position survives, like a terminal tab after `ssh` exits. A slot
    /// whose tab was closed as a whole is closed along with its local tab.
    pub(super) fn handle_dead_slots(&self) {
        if self.inner().is_none() {
            return;
        }
        let slots = self.in_tab.lock().unwrap().slots.clone();
        if slots.is_empty() {
            return;
        }
        let mux = Mux::get();
        let mut removed_any = false;
        for slot in slots {
            let view = match mux.get_tab(slot.view_tab_id) {
                Some(view) => view,
                None => {
                    // The tab was closed (e.g. CloseCurrentTab).
                    self.in_tab
                        .lock()
                        .unwrap()
                        .slots
                        .retain(|s| s.view_tab_id != slot.view_tab_id);
                    remove_tab_later(slot.local_tab_id);
                    removed_any = true;
                    continue;
                }
            };
            if !tab_is_gone(&view) {
                continue;
            }

            let candidates = self.selectable_tabs(&slot.session, Some(slot.view_tab_id));
            let order: Vec<TabId> = self
                .in_tab
                .lock()
                .unwrap()
                .remote_tabs
                .iter()
                .map(|i| i.remote_tab_id)
                .collect();
            let position = |t: TabId| order.iter().position(|&o| o == t);
            let current = position(slot.remote_tab_id);
            let next = candidates
                .iter()
                .copied()
                .find(|&t| position(t) > current)
                .or_else(|| candidates.last().copied());

            match next {
                Some(remote) => {
                    if let Err(err) = self.show_in_slot(slot.view_tab_id, remote) {
                        log::error!("showing remote tab {remote}: {err:#}");
                    }
                }
                None => {
                    self.in_tab
                        .lock()
                        .unwrap()
                        .slots
                        .retain(|s| s.view_tab_id != slot.view_tab_id);
                    restore_slot(&slot);
                    removed_any = true;
                }
            }
        }
        if removed_any && self.in_tab.lock().unwrap().slots.is_empty() {
            detach_later(self.local_domain_id);
        }
    }
}
