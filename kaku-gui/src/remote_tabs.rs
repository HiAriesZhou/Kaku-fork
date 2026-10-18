//! Performs tab changes requested by the web terminal (kaku-remote) through
//! the window's own paths: Cmd+T's SpawnTab for a new tab and the tab menu's
//! in-place close, so the browser behaves like the Mac does. Renaming and
//! reordering change the mux the way the tab bar's own editor and MoveTab do.

use crate::frontend::front_end;
use crate::termwindow::TermWindowNotif;
use config::keyassignment::{KeyAssignment, SpawnTabDomain};
use kaku_remote::{claim_tab_size_on_gui_thread, set_tab_action_handler, TabAction};
use mux::pane::PaneId;
use mux::Mux;
use std::sync::Arc;
use window::WindowOps;

pub fn register() {
    set_tab_action_handler(Arc::new(|action: TabAction| {
        promise::spawn::spawn_into_main_thread(async move { perform(action) }).detach();
    }));
}

fn perform(action: TabAction) {
    let pane_id = match &action {
        TabAction::New { pane_id }
        | TabAction::Close { pane_id }
        | TabAction::Seen { pane_id }
        | TabAction::Split { pane_id, .. }
        | TabAction::ClosePane { pane_id }
        | TabAction::Activate { pane_id }
        | TabAction::Rename { pane_id, .. }
        | TabAction::Move { pane_id, .. }
        | TabAction::Zoom { pane_id, .. } => PaneId::from(*pane_id),
    };
    let mux = Mux::get();
    let Some((_, window_id, tab_id)) = mux.resolve_pane_id(pane_id) else {
        return;
    };
    let Some(gui) = front_end().gui_window_for_mux_window(window_id) else {
        return;
    };
    match action {
        TabAction::New { .. } => {
            let (tx, rx) = smol::channel::bounded(1);
            gui.window.notify(TermWindowNotif::PerformAssignment {
                pane_id,
                assignment: KeyAssignment::SpawnTab(SpawnTabDomain::CurrentPaneDomain),
                tx: Some(tx),
            });
            promise::spawn::spawn(async move {
                if let Ok(Err(err)) = rx.recv().await {
                    log::warn!("kaku-remote web: new tab failed: {err:#}");
                }
            })
            .detach();
        }
        TabAction::Split { direction, .. } => {
            use config::keyassignment::PaneDirection;
            use mux::tab::{SplitDirection, SplitRequest, SplitSize};
            let request = SplitRequest {
                direction: match direction {
                    PaneDirection::Up | PaneDirection::Down => SplitDirection::Vertical,
                    PaneDirection::Left | PaneDirection::Right => SplitDirection::Horizontal,
                    _ => return,
                },
                target_is_second: matches!(direction, PaneDirection::Right | PaneDirection::Down),
                size: SplitSize::Percent(50),
                top_level: false,
            };
            // Native spawn_command resolves the active pane after queuing.
            // Carry the menu's stable id into mux instead; it also inherits
            // cwd, domain, encoding and pane config from that source pane.
            promise::spawn::spawn(async move {
                let _activity = mux::activity::Activity::new();
                let mux = Mux::get();
                if !matches!(mux.resolve_pane_id(pane_id), Some((_, window, _)) if window == window_id) {
                    return;
                }
                if let Err(err) = mux.split_pane(pane_id, request,
                    mux::domain::SplitSource::Spawn { command: None, command_dir: None },
                    SpawnTabDomain::CurrentPaneDomain).await {
                    log::warn!("kaku-remote web: split failed: {err:#}");
                }
            }).detach();
        }
        TabAction::ClosePane { .. } => {
            gui.window
                .notify(TermWindowNotif::Apply(Box::new(move |tw| {
                    let mux = Mux::get();
                    let Some((_, window, tab_id)) = mux.resolve_pane_id(pane_id) else {
                        return;
                    };
                    if window != tw.mux_window_id {
                        return;
                    }
                    let Some(tab) = mux.get_tab(tab_id) else {
                        return;
                    };
                    // Already confirmed in the browser. confirm:false still asks
                    // again under AlwaysPrompt, which would hide the dialog on Mac.
                    if tab.count_panes_blocking() == 1 {
                        tw.record_closed_tab_cwd(&tab);
                        mux.remove_tab(tab_id);
                    } else {
                        mux.remove_pane(pane_id);
                    }
                })));
        }
        TabAction::Close { .. } => {
            gui.window
                .notify(TermWindowNotif::CloseConfirmedTab { tab_id });
        }
        TabAction::Activate { .. } => {
            let Some(idx) = mux.get_window(window_id).and_then(|w| w.idx_by_id(tab_id)) else {
                return;
            };
            // The browser may be looking at a split that is not the tab's
            // focused one; focus it on the Mac too, as a click there would.
            if let (Some(tab), Some(pane)) = (mux.get_tab(tab_id), mux.get_pane(pane_id)) {
                if tab.get_active_pane().map(|p| p.pane_id()) != Some(pane_id) {
                    tab.set_active_pane(&pane);
                }
            }
            gui.window.notify(TermWindowNotif::PerformAssignment {
                pane_id,
                assignment: KeyAssignment::ActivateTab(idx as isize),
                tx: None,
            });
        }
        TabAction::Zoom { claimed, .. } => {
            // The browser shows one pane at a time; zooming the one it is on
            // lets that pane take the whole tab, and so the whole page.
            if let (Some(tab), Some(pane)) = (mux.get_tab(tab_id), mux.get_pane(pane_id)) {
                if tab.get_active_pane().map(|p| p.pane_id()) != Some(pane_id) {
                    tab.set_active_pane(&pane);
                }
                tab.toggle_zoom();
                if let Some((cols, rows)) = claimed {
                    claim_tab_size_on_gui_thread(pane_id.into(), cols, rows);
                }
            }
        }
        TabAction::Rename { title, .. } => {
            // An empty title goes back to the computed one, as clearing the
            // tab bar's editor does.
            if let Some(tab) = mux.get_tab(tab_id) {
                tab.set_title(title.trim());
            }
            gui.window.notify(TermWindowNotif::Apply(Box::new(|tw| {
                tw.update_title_impl()
            })));
        }
        TabAction::Move { index, .. } => {
            {
                let Some(mut window) = mux.get_window_mut(window_id) else {
                    return;
                };
                let (Some(from), Some(active)) = (
                    window.idx_by_id(tab_id),
                    window.get_active().map(|t| t.tab_id()),
                ) else {
                    return;
                };
                let to = index.min(window.len() - 1);
                if from == to {
                    return;
                }
                let tab = window.remove_by_idx(from);
                window.insert(to, &tab);
                // Keep the same tab active wherever it ended up.
                if let Some(idx) = window.idx_by_id(active) {
                    window.set_active_without_saving(idx);
                }
            }
            gui.window.notify(TermWindowNotif::Apply(Box::new(|tw| {
                tw.update_title_impl()
            })));
        }
        TabAction::Seen { .. } => {
            gui.window
                .notify(TermWindowNotif::Apply(Box::new(move |tw| {
                    tw.mark_pane_seen(pane_id)
                })));
        }
    }
}
