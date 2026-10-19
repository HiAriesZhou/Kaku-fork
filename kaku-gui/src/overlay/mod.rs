use crate::termwindow::TermWindow;
use mux::pane::{Pane, PaneId};
use mux::tab::{Tab, TabId};
use mux::termwiztermtab::{allocate, TermWizTerminal};
use std::pin::Pin;
use std::sync::Arc;
use termwiz::color::SrgbaTuple;
use wezterm_term::{TerminalConfiguration, TerminalSize};

pub mod ai_chat;
pub mod confirm;
pub mod confirm_close_pane;
pub mod copy;
pub mod launcher;
pub mod prompt;
pub mod quickselect;
pub mod selector;

pub use confirm_close_pane::{
    confirm_apply_update, confirm_close_pane, confirm_close_tab, confirm_quit_program,
};
pub use copy::{CopyModeParams, CopyOverlay};
pub use launcher::{launcher, LauncherArgs, LauncherFlags};
pub use quickselect::QuickSelectOverlay;

/// How far overlay frames and rules sit from the background toward the
/// foreground. Bright black or the foreground itself made box lines read
/// louder than the text they frame on dark themes.
const DIVIDER_BLEND: f32 = 0.25;

/// Color for overlay frames and rules, derived from the active palette so it
/// stays quiet on both light and dark themes.
pub(crate) fn divider_color(bg: SrgbaTuple, fg: SrgbaTuple) -> SrgbaTuple {
    SrgbaTuple(
        bg.0 + (fg.0 - bg.0) * DIVIDER_BLEND,
        bg.1 + (fg.1 - bg.1) * DIVIDER_BLEND,
        bg.2 + (fg.2 - bg.2) * DIVIDER_BLEND,
        1.0,
    )
}

pub fn start_overlay<T, F>(
    term_window: &TermWindow,
    tab: &Arc<Tab>,
    func: F,
) -> (
    Arc<dyn Pane>,
    Pin<Box<dyn std::future::Future<Output = anyhow::Result<T>>>>,
)
where
    T: Send + 'static,
    F: Send + 'static + FnOnce(TabId, TermWizTerminal) -> anyhow::Result<T>,
{
    let tab_id = tab.tab_id();
    let tab_size = tab.get_size();
    let term_config: Arc<dyn TerminalConfiguration + Send + Sync> =
        Arc::new(config::TermConfig::with_config(term_window.config.clone()));
    let (tw_term, tw_tab) = allocate(tab_size, term_config);

    let window = term_window.window.clone().unwrap();

    let overlay_pane_id = tw_tab.pane_id();

    let future = promise::spawn::spawn_into_new_thread(move || {
        let res = func(tab_id, tw_term);
        TermWindow::schedule_cancel_overlay(window, tab_id, Some(overlay_pane_id));
        res
    });

    (tw_tab, Box::pin(future))
}

pub fn start_overlay_pane<T, F>(
    term_window: &TermWindow,
    pane: &Arc<dyn Pane>,
    func: F,
) -> (
    Arc<dyn Pane>,
    Pin<Box<dyn std::future::Future<Output = anyhow::Result<T>>>>,
)
where
    T: Send + 'static,
    F: Send + 'static + FnOnce(PaneId, TermWizTerminal) -> anyhow::Result<T>,
{
    let pane_id = pane.pane_id();
    let dims = pane.get_dimensions();
    let size = TerminalSize {
        cols: dims.cols,
        rows: dims.viewport_rows,
        pixel_width: term_window.render_metrics.cell_size.width as usize * dims.cols,
        pixel_height: term_window.render_metrics.cell_size.height as usize * dims.viewport_rows,
        dpi: dims.dpi,
    };
    let term_config: Arc<dyn TerminalConfiguration + Send + Sync> =
        Arc::new(config::TermConfig::with_config(term_window.config.clone()));
    let (tw_term, tw_tab) = allocate(size, term_config);

    let window = term_window.window.clone().unwrap();

    let future = promise::spawn::spawn_into_new_thread(move || {
        let res = func(pane_id, tw_term);
        TermWindow::schedule_cancel_overlay_for_pane(window, pane_id);
        res
    });

    (tw_tab, Box::pin(future))
}

#[cfg(test)]
mod tests {
    use super::divider_color;
    use termwiz::color::SrgbaTuple;

    #[test]
    fn divider_sits_between_background_and_bright_black() {
        // Kaku Dark: frames used bright black #6d6d6d or the foreground.
        let bg = SrgbaTuple::from((0x15u8, 0x14u8, 0x1bu8));
        let fg = SrgbaTuple::from((0xd5u8, 0xd4u8, 0xd6u8));
        assert_eq!(divider_color(bg, fg).to_srgb_u8(), (0x45, 0x44, 0x49, 0xff));

        // Kaku Light keeps a visible but quiet line on its paper background.
        let bg = SrgbaTuple::from((0xffu8, 0xfcu8, 0xf0u8));
        let fg = SrgbaTuple::from((0x10u8, 0x0fu8, 0x0fu8));
        assert_eq!(divider_color(bg, fg).to_srgb_u8(), (0xc3, 0xc0, 0xb7, 0xff));
    }
}
