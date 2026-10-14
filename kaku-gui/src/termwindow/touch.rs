//! Remote touch support. Remote desktop clients such as UU deliver a finger
//! as a plain left button: a tap is a click and a long press arrives as a
//! left press held for seconds, never as a right click. Kaku therefore turns
//! a held, still left press into the right click the native menus answer
//! to, in every mode. Touch Mode itself only adds a drawn close button to
//! every tab of the retro bar, which has no hover to reveal one; the bar
//! keeps its normal height and look.

use super::box_model::*;
use super::{TermWindow, TermWindowNotif, UIItemType};
use crate::customglyph::{BlockAlpha, BlockCoord, Poly, PolyCommand, PolyStyle};
use crate::tabbar::TabBarItem;
use config::{Dimension, DimensionContext};
use std::time::{Duration, Instant};
use window::{MouseButtons, MouseEvent, MouseEventKind, MousePress, WindowOps};

/// Clicks from UU measured 0.07 to 0.13s, long presses 2.9s and up.
const LONG_PRESS: Duration = Duration::from_millis(500);
/// Finger jitter allowed before a press counts as a drag.
const LONG_PRESS_SLOP_PT: f32 = 8.;
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

#[derive(Clone, Copy, Debug)]
pub struct LongPress {
    started: Instant,
    x: isize,
    y: isize,
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
    /// still, replays it as a right click once this event is done. The press
    /// and release themselves are handled as usual. Panes that grab the mouse
    /// (vim, htop) get no synthetic right click.
    pub(super) fn track_long_press(&mut self, event: &MouseEvent) {
        match event.kind {
            MouseEventKind::Press(MousePress::Left) if event.modifiers.is_empty() => {
                self.mouse.long_press = Some(LongPress {
                    started: Instant::now(),
                    x: event.coords.x,
                    y: event.coords.y,
                });
            }
            MouseEventKind::Move => {
                let slop = LONG_PRESS_SLOP_PT * self.touch_scale();
                if self
                    .mouse
                    .long_press
                    .is_some_and(|press| press.moved_beyond(event.coords.x, event.coords.y, slop))
                {
                    self.mouse.long_press = None;
                }
            }
            MouseEventKind::Release(MousePress::Left) => {
                let Some(press) = self.mouse.long_press.take() else {
                    return;
                };
                let item = self.resolve_ui_item(event).map(|item| item.item_type);
                let on_tab = matches!(item, Some(UIItemType::TabBar(TabBarItem::Tab { .. })));
                let grabbed = self
                    .get_active_pane_or_overlay()
                    .is_some_and(|pane| pane.is_mouse_grabbed());
                if !long_press_as_right_click(
                    press.started.elapsed(),
                    on_tab,
                    item.is_some(),
                    grabbed,
                ) {
                    return;
                }
                let Some(window) = self.window.clone() else {
                    return;
                };
                let mut right = event.clone();
                right.kind = MouseEventKind::Press(MousePress::Right);
                right.mouse_buttons = MouseButtons::RIGHT;
                window.notify(TermWindowNotif::Apply(Box::new(move |tw| {
                    let Some(window) = tw.window.clone() else {
                        return;
                    };
                    let mut release = right.clone();
                    tw.mouse_event_impl(right, &window);
                    release.kind = MouseEventKind::Release(MousePress::Right);
                    release.mouse_buttons = MouseButtons::NONE;
                    tw.mouse_event_impl(release, &window);
                })));
            }
            MouseEventKind::Press(_) => self.mouse.long_press = None,
            _ => {}
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
        };
        assert!(!press.moved_beyond(108, 92, 8.));
        assert!(press.moved_beyond(109, 100, 8.));
        assert!(press.moved_beyond(100, 91, 8.));
    }
}
