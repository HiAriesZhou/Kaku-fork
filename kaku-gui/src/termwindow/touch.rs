//! Remote touch support. Remote desktop clients such as UU deliver a finger
//! as a plain left button: a tap is a click and a long press arrives as a
//! left press held for seconds, never as a right click. Kaku therefore turns
//! a held, still left press into the right click the native menus answer
//! to, in every mode. Touch Mode itself only adds a drawn close button to
//! every tab of the retro bar, which has no hover to reveal one; the bar
//! keeps its normal height and look.

use super::box_model::*;
use super::{TermWindow, UIItem, UIItemType};
use crate::customglyph::{BlockAlpha, BlockCoord, Poly, PolyCommand, PolyStyle};
use crate::tabbar::TabBarItem;
use config::{Dimension, DimensionContext};
use std::time::{Duration, Instant};
use window::{MouseButtons, MouseEvent, MouseEventKind, MousePress, WindowOps};

/// Clicks from UU measured 0.07 to 0.13s. A full second felt slow over UU
/// (a 0.9s press missed) and 600ms still read as a wait, so 450ms, above a
/// hesitant desktop click of about 300ms.
const LONG_PRESS: Duration = Duration::from_millis(450);
/// Menus opened by a long press give each item a row this tall, for
/// fingers on a remote desktop. The titles keep the system font: an 18pt
/// font (0bed9c94) mostly grew the text, which read as coarse over the
/// compressed stream, while the rows barely followed it. 30pt gives 36pt
/// rows, 1.5x the system 24pt; 36pt (42pt rows) read as too large.
pub(super) const LONG_PRESS_MENU_ROW_PT: f64 = 30.;
/// Finger jitter allowed before a press counts as a drag. 8pt cancelled
/// ordinary long presses over UU, where a resting finger drifts further.
const LONG_PRESS_SLOP_PT: f32 = 24.;
const CLOSE_ICON: &[Poly] = &[
    Poly {
        path: &[
            PolyCommand::MoveTo(BlockCoord::Zero, BlockCoord::Zero),
            PolyCommand::LineTo(BlockCoord::One, BlockCoord::One),
        ],
        intensity: BlockAlpha::Full,
        style: PolyStyle::Outline,
    },
    Poly {
        path: &[
            PolyCommand::MoveTo(BlockCoord::One, BlockCoord::Zero),
            PolyCommand::LineTo(BlockCoord::Zero, BlockCoord::One),
        ],
        intensity: BlockAlpha::Full,
        style: PolyStyle::Outline,
    },
];

#[derive(Clone, Debug)]
pub struct LongPress {
    started: Instant,
    x: isize,
    y: isize,
    /// A tab reached through the bottom bar's touch band above it.
    band_item: Option<UIItem>,
}

impl LongPress {
    fn moved_beyond(&self, x: isize, y: isize, slop: f32) -> bool {
        (x - self.x).abs() as f32 > slop || (y - self.y).abs() as f32 > slop
    }
}

/// Whether a left press held still for `held` turns into a right click.
fn long_press_as_right_click(
    held: Duration,
    on_tab: bool,
    on_other_ui: bool,
    grabbed: bool,
) -> bool {
    held >= LONG_PRESS && (on_tab || (!on_other_ui && !grabbed))
}

impl TermWindow {
    fn touch_scale(&self) -> f32 {
        self.dimensions.dpi as f32 / 72.
    }

    pub(super) fn toggle_touch_mode(&mut self) {
        self.touch_mode = !self.touch_mode;
        self.sync_tab_bar_visibility_for_window_state("touch-mode");
        self.invalidate_fancy_tab_bar();
        if let Some(window) = self.window.clone() {
            self.apply_dimensions(&self.dimensions.clone(), None, &window, false);
            self.update_title();
            window.invalidate();
        }
    }

    /// Follows plain left presses and, when one is released after being held
    /// still, queues it as a right click for `replay_long_press`. The press
    /// and release themselves are handled as usual. Panes that grab the mouse
    /// (vim, htop) get no synthetic right click.
    pub(super) fn track_long_press(&mut self, event: &MouseEvent) {
        match event.kind {
            MouseEventKind::Press(MousePress::Left) if event.modifiers.is_empty() => {
                self.mouse.long_press = Some(LongPress {
                    started: Instant::now(),
                    x: event.coords.x,
                    y: event.coords.y,
                    band_item: None,
                });
            }
            MouseEventKind::Move => {
                let slop = LONG_PRESS_SLOP_PT * self.touch_scale();
                if let Some(press) = self.mouse.long_press.as_ref() {
                    if press.moved_beyond(event.coords.x, event.coords.y, slop) {
                        if self.config.debug_key_events {
                            log::info!(
                                "long_press cancelled after moving {}x{} px",
                                event.coords.x - press.x,
                                event.coords.y - press.y
                            );
                        }
                        self.mouse.long_press = None;
                    }
                }
            }
            MouseEventKind::Release(MousePress::Left) => {
                let Some(press) = self.mouse.long_press.take() else {
                    return;
                };
                // Remote clients may send no motion at all, so check the
                // release point too.
                if press.moved_beyond(
                    event.coords.x,
                    event.coords.y,
                    LONG_PRESS_SLOP_PT * self.touch_scale(),
                ) {
                    return;
                }
                let band_tab = press.band_item.filter(|item| {
                    matches!(item.item_type, UIItemType::TabBar(TabBarItem::Tab { .. }))
                });
                let item = band_tab
                    .clone()
                    .or_else(|| self.resolve_ui_item(event))
                    .map(|item| item.item_type);
                let on_tab = matches!(item, Some(UIItemType::TabBar(TabBarItem::Tab { .. })));
                let grabbed = self
                    .get_active_pane_or_overlay()
                    .is_some_and(|pane| pane.is_mouse_grabbed());
                let replay = long_press_as_right_click(
                    press.started.elapsed(),
                    on_tab,
                    item.is_some(),
                    grabbed,
                );
                if self.config.debug_key_events {
                    // Remote clients differ in what reaches the Mac; log the
                    // decision so a long press that opens nothing can be
                    // traced (see the mouse_event lines next to it).
                    log::info!(
                        "long_press held={:?} item={:?} grabbed={grabbed} replay={replay}",
                        press.started.elapsed(),
                        item
                    );
                }
                if !replay {
                    return;
                }
                let mut right = event.clone();
                // The band only takes plain left presses, so aim a band tab's
                // right click at the tab's own row.
                if let Some(tab) = &band_tab {
                    right.coords.y = tab.y as isize;
                }
                right.kind = MouseEventKind::Press(MousePress::Right);
                right.mouse_buttons = MouseButtons::RIGHT;
                self.mouse.long_press_replay = Some(right);
            }
            MouseEventKind::Press(_) => self.mouse.long_press = None,
            _ => {}
        }
    }

    /// Delivers a queued long press as a right click, in the same window event
    /// dispatch as the release that ended it. A deferred `TermWindowNotif`
    /// does not work: the window is borrowed while notifications run, so the
    /// menu could not reach its NSWindow and silently never opened.
    pub(super) fn replay_long_press(&mut self, context: &dyn WindowOps) {
        let Some(right) = self.mouse.long_press_replay.take() else {
            return;
        };
        let mut release = right.clone();
        self.mouse.menu_from_long_press = true;
        self.mouse_event_impl(right, context);
        self.mouse.menu_from_long_press = false;
        release.kind = MouseEventKind::Release(MousePress::Right);
        release.mouse_buttons = MouseButtons::NONE;
        self.mouse_event_impl(release, context);
    }

    /// Gives a context menu taller rows when a long press opened it; a
    /// mouse right click keeps the system size.
    pub(super) fn size_menu_for_touch(&self, menu: &window::os::macos::menu::Menu) {
        if self.mouse.menu_from_long_press {
            menu.set_row_height(LONG_PRESS_MENU_ROW_PT);
        }
    }

    /// Records the tab a press reached through the bottom bar's touch band.
    pub(super) fn note_long_press_band_item(&mut self, item: &UIItem) {
        if let Some(press) = self.mouse.long_press.as_mut() {
            press.band_item = Some(item.clone());
        }
    }

    /// Draws Touch Mode's close buttons over the cells the retro bar kept
    /// for them, centered in each slot.
    pub(super) fn paint_tab_close_buttons(&mut self, y: f32, height: f32) -> anyhow::Result<()> {
        let slots = self.tab_bar.close_slots().to_vec();
        if slots.is_empty() {
            return Ok(());
        }
        let font = self.fonts.default_font()?;
        let metrics = self.render_metrics;
        let colors = self
            .config
            .resolved_palette
            .tab_bar
            .clone()
            .unwrap_or_default();
        let cell_width = metrics.cell_size.width as f32;
        let icon = (metrics.cell_size.height as f32 * 0.32).round();
        let line_width = (self.touch_scale() * 1.25).ceil() as isize;
        let gl = self.render_state.as_ref().unwrap();
        for (_, active, cell, cells) in slots {
            let style = if active {
                colors.active_tab()
            } else {
                colors.inactive_tab()
            };
            let element = Element::new(
                &font,
                ElementContent::Poly {
                    line_width,
                    poly: SizedPoly {
                        poly: CLOSE_ICON,
                        width: Dimension::Pixels(icon),
                        height: Dimension::Pixels(icon),
                    },
                },
            )
            .colors(ElementColors {
                border: BorderColor::default(),
                bg: style.bg_color.to_linear().into(),
                text: style.fg_color.to_linear().into(),
            });
            let rect = euclid::rect(
                cell as f32 * cell_width,
                y,
                cells as f32 * cell_width,
                height,
            );
            let mut computed = self.compute_element(
                &LayoutContext {
                    height: DimensionContext {
                        dpi: self.dimensions.dpi as f32,
                        pixel_max: self.dimensions.pixel_height as f32,
                        pixel_cell: metrics.cell_size.height as f32,
                    },
                    width: DimensionContext {
                        dpi: self.dimensions.dpi as f32,
                        pixel_max: self.dimensions.pixel_width as f32,
                        pixel_cell: cell_width,
                    },
                    bounds: rect,
                    metrics: &metrics,
                    gl_state: gl,
                    zindex: 30,
                },
                &element,
            )?;
            computed.translate(euclid::vec2(
                rect.min_x() + (rect.width() - computed.bounds.width()).max(0.) / 2.
                    - computed.bounds.min_x(),
                rect.min_y() + (rect.height() - computed.bounds.height()).max(0.) / 2.
                    - computed.bounds.min_y(),
            ));
            self.render_element(&computed, gl, None)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_held_press_off_grabbing_panes_becomes_a_right_click() {
        let click = Duration::from_millis(130);
        let hold = Duration::from_millis(2900);
        // A hesitant desktop click is not a long press, a 0.9s hold over UU is.
        assert!(long_press_as_right_click(
            Duration::from_millis(900),
            false,
            false,
            false
        ));
        assert!(long_press_as_right_click(
            Duration::from_millis(450),
            false,
            false,
            false
        ));
        assert!(!long_press_as_right_click(
            Duration::from_millis(300),
            false,
            false,
            false
        ));
        // UU clicks stay clicks everywhere.
        assert!(!long_press_as_right_click(click, true, true, false));
        assert!(!long_press_as_right_click(click, false, false, false));
        // A held press opens the tab menu, even over a TUI.
        assert!(long_press_as_right_click(hold, true, true, true));
        // On the terminal it opens the terminal menu unless the pane grabs
        // the mouse, where a right click would reach the program instead.
        assert!(long_press_as_right_click(hold, false, false, false));
        assert!(!long_press_as_right_click(hold, false, false, true));
        // Other bar items (new tab button, close buttons, scrollbar) keep
        // their own click.
        assert!(!long_press_as_right_click(hold, false, true, false));
    }

    #[test]
    fn jitter_within_the_slop_keeps_the_press() {
        let press = LongPress {
            started: Instant::now(),
            x: 100,
            y: 100,
            band_item: None,
        };
        assert!(!press.moved_beyond(108, 92, 8.));
        assert!(press.moved_beyond(109, 100, 8.));
        assert!(press.moved_beyond(100, 91, 8.));
    }
}
