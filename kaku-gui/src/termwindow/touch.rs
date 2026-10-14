use super::box_model::*;
use super::{TermWindow, TermWindowNotif, UIItem, UIItemType};
use crate::customglyph::{BlockAlpha, BlockCoord, Poly, PolyCommand, PolyStyle};
use config::keyassignment::{
    ClipboardCopyDestination, ClipboardPasteSource, KeyAssignment, SpawnTabDomain,
};
use config::{Dimension, DimensionContext};
use mux::pane::{CloseReason, PaneId};
use mux::tab::TabId;
use mux::Mux;
use std::time::Instant;
use window::{MouseEvent, MouseEventKind, MousePress, WindowOps};

const CONTROL_PT: f32 = 40.;
const BAR_PT: f32 = 40.;
const TOGGLE_PT: f32 = 44.;
/// Nerd Font md-gesture_tap: a pointing hand, for the Touch Mode toggle.
const TOUCH_GLYPH: &str = "\u{f0741}";
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TouchCommand {
    Copy,
    Paste,
    Larger,
    Smaller,
    Zoom,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TouchAction {
    Toggle,
    Tabs(usize),
    Menu(Option<TabId>, usize),
    NewTab,
    Activate(TabId),
    Close(TabId),
    ConfirmClose(TabId),
    Command(PaneId, TouchCommand),
    Dismiss,
    ScrollStrip,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TouchPanel {
    Tabs(usize),
    Menu(Option<TabId>, usize),
    Confirm(TabId),
}
pub struct TouchPress {
    action: TouchAction,
    item: UIItem,
    x: isize,
    y: isize,
    scroll_start: f32,
    started: Instant,
    held: bool,
    pub(super) cancelled: bool,
}
impl TouchPress {
    fn moved(&mut self, x: isize, y: isize, scale: f32) {
        if (x - self.x).abs() as f32 > 8. * scale || (y - self.y).abs() as f32 > 8. * scale {
            self.cancelled = true;
        }
    }
    fn release(&self, item: Option<&UIItem>) -> Option<TouchAction> {
        if !self.cancelled && !self.held && item == Some(&self.item) {
            Some(self.action)
        } else {
            None
        }
    }
}

/// Clamp the strip after tab removal, resizing, and drag/wheel scrolling.
fn clamped_scroll(offset: f32, content_width: f32, viewport_width: f32) -> f32 {
    offset.max(0.).min((content_width - viewport_width).max(0.))
}
fn reveal_tab(offset: f32, start: f32, end: f32, viewport: f32) -> f32 {
    if start < offset {
        start
    } else if end > offset + viewport {
        end - viewport
    } else {
        offset
    }
}

fn panel_range(
    height: f32,
    top: f32,
    bottom: f32,
    bar_y: f32,
    bar_height: f32,
    at_bottom: bool,
) -> (f32, f32) {
    if at_bottom {
        (top, bar_y)
    } else {
        (bar_y + bar_height, height - bottom)
    }
}

impl TermWindow {
    fn touch_scale(&self) -> f32 {
        self.dimensions.dpi as f32 / 72.
    }
    pub(super) fn touch_toggle_width(&self) -> f32 {
        (TOGGLE_PT * self.touch_scale()).max(self.render_metrics.cell_size.width as f32 * 4.)
    }
    /// Tab bar columns kept free for the toggle, in front of the right status.
    pub(super) fn touch_toggle_cols(&self) -> usize {
        let cell = self.render_metrics.cell_size.width.max(1) as f32;
        (self.touch_toggle_width() / cell).ceil() as usize
    }
    pub(super) fn touch_bar_height(&self) -> f32 {
        (BAR_PT * self.touch_scale())
            .max(self.render_metrics.cell_size.height as f32 + 4. * self.touch_scale())
    }
    pub(super) fn toggle_touch_mode(&mut self) {
        self.touch_mode = !self.touch_mode;
        self.touch_panel = None;
        self.touch_press = None;
        self.sync_tab_bar_visibility_for_window_state("touch-mode");
        self.invalidate_fancy_tab_bar();
        if let Some(window) = self.window.clone() {
            self.apply_dimensions(&self.dimensions.clone(), None, &window, false);
            self.update_title();
            window.invalidate();
        }
    }

    fn touch_component(
        &mut self,
        text: &str,
        action: TouchAction,
        rect: window::RectF,
        active: bool,
        close_icon: bool,
    ) -> anyhow::Result<()> {
        let font = if !self.show_tab_bar {
            self.fonts.title_font()?
        } else {
            self.fonts.default_font()?
        };
        let metrics = if !self.show_tab_bar {
            crate::utilsprites::RenderMetrics::with_font_metrics(&font.metrics())
        } else {
            self.render_metrics
        };
        let colors = self
            .config
            .resolved_palette
            .tab_bar
            .clone()
            .unwrap_or_default();
        let style = if active {
            colors.active_tab()
        } else {
            colors.inactive_tab()
        };
        let style = if self
            .touch_press
            .as_ref()
            .is_some_and(|press| press.action == action && !press.cancelled)
        {
            colors.inactive_tab_hover()
        } else {
            style
        };
        let foreground = if matches!(
            action,
            TouchAction::NewTab | TouchAction::Menu(..) | TouchAction::Tabs(_)
        ) {
            self.config
                .resolved_palette
                .foreground
                .unwrap_or(style.fg_color)
        } else {
            style.fg_color
        };
        let color = ElementColors {
            border: BorderColor::default(),
            bg: style.bg_color.to_linear().into(),
            text: foreground.to_linear().into(),
        };
        let content = if close_icon {
            ElementContent::Poly {
                line_width: (self.touch_scale() * 1.5).ceil() as isize,
                poly: SizedPoly {
                    poly: CLOSE_ICON,
                    width: Dimension::Pixels(12. * self.touch_scale()),
                    height: Dimension::Pixels(12. * self.touch_scale()),
                },
            }
        } else {
            ElementContent::Text(text.to_string())
        };
        let element = Element::new(&font, content)
            .colors(color.clone())
            .hover_colors(Some(ElementColors {
                border: BorderColor::default(),
                bg: colors.inactive_tab_hover().bg_color.to_linear().into(),
                text: colors.inactive_tab_hover().fg_color.to_linear().into(),
            }))
            .max_width(Some(Dimension::Pixels(rect.width())));
        let gl = self.render_state.as_ref().unwrap();
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
                    pixel_cell: metrics.cell_size.width as f32,
                },
                bounds: rect,
                metrics: &metrics,
                gl_state: gl,
                zindex: 30,
            },
            &element,
        )?;
        let x = if close_icon
            || matches!(
                action,
                TouchAction::Toggle
                    | TouchAction::NewTab
                    | TouchAction::Tabs(_)
                    | TouchAction::Menu(..)
            ) {
            (rect.width() - computed.bounds.width()).max(0.) / 2.
        } else {
            8. * self.touch_scale()
        };
        computed.translate(euclid::vec2(
            rect.min_x() + x - computed.bounds.min_x(),
            rect.min_y() + (rect.height() - computed.bounds.height()).max(0.) / 2.
                - computed.bounds.min_y(),
        ));
        // Fill the hit area independently of glyph metrics, including the
        // centered close icon, so neither the font baseline nor title length
        // controls the target's size.
        let background = Element::new(&font, ElementContent::Text(String::new()))
            .min_width(Some(Dimension::Pixels(rect.width())))
            .min_height(Some(Dimension::Pixels(rect.height())))
            .colors(color)
            .item_type(UIItemType::Touch(action));
        let bg = self.compute_element(
            &LayoutContext {
                height: DimensionContext {
                    dpi: self.dimensions.dpi as f32,
                    pixel_max: self.dimensions.pixel_height as f32,
                    pixel_cell: metrics.cell_size.height as f32,
                },
                width: DimensionContext {
                    dpi: self.dimensions.dpi as f32,
                    pixel_max: self.dimensions.pixel_width as f32,
                    pixel_cell: metrics.cell_size.width as f32,
                },
                bounds: rect,
                metrics: &metrics,
                gl_state: gl,
                zindex: 30,
            },
            &background,
        )?;
        self.render_element(&bg, gl, None)?;
        self.render_element(&computed, gl, None)?;
        self.ui_items.push(UIItem {
            x: rect.min_x().max(0.) as usize,
            y: rect.min_y().max(0.) as usize,
            width: rect.width() as usize,
            height: rect.height() as usize,
            item_type: UIItemType::Touch(action),
        });
        Ok(())
    }

    pub(super) fn paint_touch_toggle(&mut self, y: f32) -> anyhow::Result<()> {
        let pixel_width = self.dimensions.pixel_width as f32;
        let (x, width) = if self.touch_mode || self.config.use_fancy_tab_bar {
            let width = self.touch_toggle_width().min(pixel_width);
            (pixel_width - width, width)
        } else {
            // The retro bar left a gap of whole cells in front of the right
            // status for the toggle.
            let cell = self.render_metrics.cell_size.width as f32;
            let cols = self.touch_toggle_cols();
            let start = self.tab_bar.right_status_start().saturating_sub(cols);
            (start as f32 * cell, (cols as f32 * cell).min(pixel_width))
        };
        let rect = euclid::rect(x, y, width, self.tab_bar_pixel_height()?);
        self.touch_component(
            TOUCH_GLYPH,
            TouchAction::Toggle,
            rect,
            self.touch_mode,
            false,
        )
    }

    pub(super) fn paint_hidden_touch_entry(&mut self) -> anyhow::Result<()> {
        if !self.config.enable_tab_bar {
            return Ok(());
        }
        let height = self.get_os_border().top.get() as f32;
        // Reuse the integrated titlebar's existing clearance, never cover a
        // terminal row. Configurations with no chrome retain the Window menu.
        if height >= 12. * self.touch_scale() {
            let width = self
                .touch_toggle_width()
                .min(self.dimensions.pixel_width as f32);
            self.touch_component(
                TOUCH_GLYPH,
                TouchAction::Toggle,
                euclid::rect(
                    self.dimensions.pixel_width as f32 - width,
                    0.,
                    width,
                    height,
                ),
                false,
                false,
            )?;
        }
        Ok(())
    }
    pub(super) fn paint_touch_bar(&mut self) -> anyhow::Result<()> {
        let border = self.get_os_border();
        let height = self.touch_bar_height();
        let y = if self.config.tab_bar_at_bottom {
            (self.dimensions.pixel_height as f32 - height - border.bottom.get() as f32).max(0.)
        } else {
            border.top.get() as f32
        };
        let scale = self.touch_scale();
        let width = self.dimensions.pixel_width as f32;
        let control = CONTROL_PT * scale;
        let toggle = self.touch_toggle_width();
        let tabs = self.get_tab_information();
        let menu_width = (48. * scale).max(self.render_metrics.cell_size.width as f32 * 5.);
        let available = (width - toggle - control - menu_width).max(0.);
        let titles: Vec<String> = tabs
            .iter()
            .map(|tab| {
                let mut title = crate::tabbar::compute_tab_plain_title(tab);
                if matches!(
                    tab.progress,
                    wezterm_term::Progress::Paused | wezterm_term::Progress::Error(_)
                ) || tab.has_unread_bell
                {
                    title.insert_str(0, "• ");
                }
                title
            })
            .collect();
        let widths: Vec<f32> = titles
            .iter()
            .map(|title| {
                (termwiz::cell::unicode_column_width(title, None) as f32
                    * self.render_metrics.cell_size.width as f32
                    + 16. * scale
                    + control)
                    .clamp(104. * scale, 180. * scale)
                    .min(available)
            })
            .collect();
        let total: f32 = widths.iter().sum();
        let active = tabs.iter().position(|tab| tab.is_active).unwrap_or(0);
        if let Some(tab) = tabs.get(active) {
            if self.touch_last_active_tab != Some(tab.tab_id) {
                let start: f32 = widths.iter().take(active).sum();
                self.touch_scroll_offset = reveal_tab(
                    self.touch_scroll_offset,
                    start,
                    start + widths[active],
                    available,
                );
                self.touch_last_active_tab = Some(tab.tab_id);
            }
        }
        self.touch_scroll_offset = clamped_scroll(self.touch_scroll_offset, total, available);
        self.touch_scroll_max = (total - available).max(0.);
        self.touch_component(
            "",
            TouchAction::ScrollStrip,
            euclid::rect(0., y, available, height),
            false,
            false,
        )?;
        let mut x = -self.touch_scroll_offset;
        for ((tab, title), tab_width) in tabs.iter().zip(titles.iter()).zip(widths.iter()) {
            let right = x + tab_width;
            if right > 0. && x < available {
                let left = x.max(0.);
                let title_right = (right - control).min(available);
                if title_right > left {
                    let skip =
                        ((-x).max(0.) / self.render_metrics.cell_size.width as f32).ceil() as usize;
                    let title: String = finl_unicode::grapheme_clusters::Graphemes::new(title)
                        .skip(skip)
                        .collect();
                    self.touch_component(
                        &title,
                        TouchAction::Activate(tab.tab_id),
                        euclid::rect(left, y, title_right - left, height),
                        tab.is_active,
                        false,
                    )?;
                }
                // A partially clipped close target is not drawn or clickable.
                if x >= 0. && right <= available {
                    self.touch_component(
                        "",
                        TouchAction::Close(tab.tab_id),
                        euclid::rect(right - control, y, control, height),
                        tab.is_active,
                        true,
                    )?;
                }
            }
            x = right;
        }
        // Unused strip space is still part of the bar, never terminal input.
        self.touch_component(
            "+",
            TouchAction::NewTab,
            euclid::rect(available, y, control, height),
            false,
            false,
        )?;
        self.touch_component(
            "Menu",
            TouchAction::Menu(None, 0),
            euclid::rect(available + control, y, menu_width, height),
            false,
            false,
        )?;
        self.paint_touch_toggle(y)?;
        self.paint_touch_panel(y)
    }

    fn paint_touch_panel(&mut self, bar_y: f32) -> anyhow::Result<()> {
        let Some(panel) = self.touch_panel else {
            return Ok(());
        };
        let tabs = self.get_tab_information();
        let row = CONTROL_PT * self.touch_scale();
        let border = self.get_os_border();
        let (panel_top, panel_bottom) = panel_range(
            self.dimensions.pixel_height as f32,
            border.top.get() as f32,
            border.bottom.get() as f32,
            bar_y,
            self.touch_bar_height(),
            self.config.tab_bar_at_bottom,
        );
        let available_height = (panel_bottom - panel_top).max(0.);
        if available_height < 1. {
            return Ok(());
        }
        let max_rows = (available_height / row).floor().max(1.) as usize;
        let mut entries: Vec<(String, TouchAction)> = vec![];
        match panel {
            TouchPanel::Tabs(page) => {
                let capacity = max_rows.saturating_sub(3).max(1);
                let start = page.min(tabs.len().saturating_sub(1) / capacity) * capacity;
                for tab in tabs.iter().skip(start).take(capacity) {
                    let title = if tab.tab_title.is_empty() {
                        tab.active_pane
                            .as_ref()
                            .map(|p| p.title.clone())
                            .unwrap_or_else(|| "Tab".into())
                    } else {
                        tab.tab_title.clone()
                    };
                    entries.push((title, TouchAction::Activate(tab.tab_id)));
                }
                if start > 0 {
                    entries.push((
                        "Previous tabs".into(),
                        TouchAction::Tabs(page.saturating_sub(1)),
                    ));
                }
                if start + capacity < tabs.len() {
                    entries.push(("More tabs".into(), TouchAction::Tabs(page + 1)));
                }
            }
            TouchPanel::Menu(target, page) => {
                let tab = target
                    .and_then(|id| tabs.iter().find(|t| t.tab_id == id))
                    .or_else(|| {
                        if target.is_none() {
                            tabs.iter().find(|t| t.is_active)
                        } else {
                            None
                        }
                    });
                if let Some(tab) = tab {
                    let pane_id = if tab.is_active {
                        self.get_active_pane_or_overlay().map(|pane| pane.pane_id())
                    } else {
                        tab.active_pane.as_ref().map(|pane| pane.pane_id)
                    };
                    if let Some(pane_id) = pane_id {
                        for (label, command) in [
                            ("Copy", TouchCommand::Copy),
                            ("Paste", TouchCommand::Paste),
                            ("Larger text", TouchCommand::Larger),
                            ("Smaller text", TouchCommand::Smaller),
                            ("Zoom pane", TouchCommand::Zoom),
                        ] {
                            entries.push((label.into(), TouchAction::Command(pane_id, command)));
                        }
                    }
                    entries.push(("Tabs".into(), TouchAction::Tabs(0)));
                    entries.push(("Close tab".into(), TouchAction::Close(tab.tab_id)));
                }
                let capacity = if entries.len() < max_rows {
                    max_rows - 1
                } else {
                    max_rows.saturating_sub(3).max(1)
                };
                let start = page.min(entries.len().saturating_sub(1) / capacity) * capacity;
                let total = entries.len();
                entries = entries.into_iter().skip(start).take(capacity).collect();
                if start > 0 {
                    entries.push((
                        "Previous actions".into(),
                        TouchAction::Menu(target, page.saturating_sub(1)),
                    ));
                }
                if start + capacity < total {
                    entries.push(("More actions".into(), TouchAction::Menu(target, page + 1)));
                }
            }
            TouchPanel::Confirm(id) => {
                entries.push(("Close this tab?".into(), TouchAction::Dismiss));
                entries.push(("All panes will close".into(), TouchAction::Dismiss));
                entries.push(("Close tab".into(), TouchAction::ConfirmClose(id)));
            }
        }
        entries.push(("Cancel".into(), TouchAction::Dismiss));
        // Paginated tab lists fit at every height. At unusually short window
        // heights use smaller rows only for this panel, never off-screen rows.
        let row = row.min(available_height / entries.len().max(1) as f32);
        let panel_height = entries.len() as f32 * row;
        let y = if self.config.tab_bar_at_bottom {
            panel_bottom - panel_height
        } else {
            panel_top
        };
        let width = (320. * self.touch_scale()).min(self.dimensions.pixel_width as f32);
        let x = (self.dimensions.pixel_width as f32 - width).max(0.);
        for (i, (label, action)) in entries.iter().enumerate() {
            self.touch_component(
                label,
                *action,
                euclid::rect(x, y + i as f32 * row, width, row),
                false,
                false,
            )?;
        }
        Ok(())
    }

    pub(super) fn touch_mouse_event(
        &mut self,
        event: &MouseEvent,
        context: &dyn WindowOps,
    ) -> bool {
        let item = self
            .ui_items
            .iter()
            .rev()
            .find(|i| i.hit_test(event.coords.x, event.coords.y))
            .cloned();
        let action = item.as_ref().and_then(|i| {
            if let UIItemType::Touch(a) = i.item_type {
                Some(a)
            } else {
                None
            }
        });
        match event.kind {
            MouseEventKind::Release(MousePress::Right) if self.touch_panel.is_some() => {
                return true
            }
            MouseEventKind::Press(MousePress::Right)
                if matches!(
                    action,
                    Some(TouchAction::Activate(_) | TouchAction::Close(_))
                ) =>
            {
                if let Some(TouchAction::Activate(id) | TouchAction::Close(id)) = action {
                    self.touch_panel = Some(TouchPanel::Menu(Some(id), 0));
                }
                context.invalidate();
                return true;
            }
            MouseEventKind::HorzWheel(amount) | MouseEventKind::VertWheel(amount)
                if self.touch_mode
                    && self.touch_panel.is_none()
                    && matches!(
                        action,
                        Some(
                            TouchAction::Activate(_)
                                | TouchAction::Close(_)
                                | TouchAction::ScrollStrip
                        )
                    ) =>
            {
                self.touch_scroll_offset = clamped_scroll(
                    self.touch_scroll_offset - amount as f32 * 32. * self.touch_scale(),
                    self.touch_scroll_max,
                    0.,
                );
                context.invalidate();
                return true;
            }
            MouseEventKind::Press(MousePress::Left) if action.is_some() => {
                let action = action.unwrap();
                self.touch_press = Some(TouchPress {
                    action,
                    item: item.unwrap(),
                    x: event.coords.x,
                    y: event.coords.y,
                    scroll_start: self.touch_scroll_offset,
                    started: Instant::now(),
                    held: false,
                    cancelled: false,
                });
                if event.modifiers.contains(window::Modifiers::CTRL) {
                    if let TouchAction::Activate(id) | TouchAction::Close(id) = action {
                        self.touch_press.as_mut().unwrap().held = true;
                        self.touch_panel = Some(TouchPanel::Menu(Some(id), 0));
                        context.invalidate();
                        return true;
                    }
                }
                if matches!(action, TouchAction::Activate(_)) {
                    if let Some(window) = self.window.clone() {
                        let started = self.touch_press.as_ref().unwrap().started;
                        promise::spawn::spawn_into_main_thread(async move {
                            smol::Timer::after(std::time::Duration::from_millis(500)).await;
                            window.notify(TermWindowNotif::Apply(Box::new(move |tw| {
                                if let Some(press) = tw.touch_press.as_mut() {
                                    if press.started == started && !press.cancelled {
                                        if let TouchAction::Activate(id) = press.action {
                                            press.held = true;
                                            tw.touch_panel = Some(TouchPanel::Menu(Some(id), 0));
                                            if let Some(window) = &tw.window {
                                                window.invalidate();
                                            }
                                        }
                                    }
                                }
                            })));
                        })
                        .detach();
                    }
                }
                context.invalidate();
                return true;
            }
            MouseEventKind::Move if self.touch_press.is_some() => {
                let scale = self.touch_scale();
                let press = self.touch_press.as_mut().unwrap();
                press.moved(event.coords.x, event.coords.y, scale);
                if press.cancelled
                    && !press.held
                    && matches!(
                        press.action,
                        TouchAction::Activate(_) | TouchAction::ScrollStrip
                    )
                {
                    self.touch_scroll_offset = clamped_scroll(
                        press.scroll_start + (press.x - event.coords.x) as f32,
                        self.touch_scroll_max,
                        0.,
                    );
                    context.invalidate();
                }
                return true;
            }
            MouseEventKind::Release(MousePress::Left) if self.touch_press.is_some() => {
                let press = self.touch_press.take().unwrap();
                if let Some(action) = press.release(item.as_ref()) {
                    self.perform_touch_action(action);
                }
                context.invalidate();
                return true;
            }
            MouseEventKind::Press(_) if self.touch_panel.is_some() && action.is_none() => {
                self.touch_panel = None;
                if matches!(event.kind, MouseEventKind::Press(MousePress::Left)) {
                    self.touch_press = Some(TouchPress {
                        action: TouchAction::Dismiss,
                        item: UIItem {
                            x: 0,
                            y: 0,
                            width: 0,
                            height: 0,
                            item_type: UIItemType::Touch(TouchAction::Dismiss),
                        },
                        x: event.coords.x,
                        y: event.coords.y,
                        scroll_start: self.touch_scroll_offset,
                        started: Instant::now(),
                        held: false,
                        cancelled: true,
                    });
                }
                context.invalidate();
                return true;
            }
            _ => {}
        }
        false
    }

    fn touch_tab_index(&self, id: TabId) -> Option<usize> {
        Mux::get()
            .get_window(self.mux_window_id)
            .and_then(|w| w.iter().position(|t| t.tab_id() == id))
    }
    fn touch_close_confirmed(&mut self, id: TabId) {
        if self.touch_tab_index(id).is_some() {
            if let Some(tab) = Mux::get().get_tab(id) {
                self.record_closed_tab_cwd(&tab);
                Mux::get().remove_tab(id);
            }
        }
    }
    fn perform_touch_action(&mut self, action: TouchAction) {
        self.touch_panel = None;
        match action {
            TouchAction::Toggle => self.toggle_touch_mode(),
            TouchAction::Tabs(page) => self.touch_panel = Some(TouchPanel::Tabs(page)),
            TouchAction::Menu(id, page) => self.touch_panel = Some(TouchPanel::Menu(id, page)),
            TouchAction::Activate(id) => {
                if let Some(idx) = self.touch_tab_index(id) {
                    let _ = self.activate_tab(idx as isize);
                }
            }
            TouchAction::Close(id) => {
                if self.touch_tab_index(id).is_some() {
                    if let Some(tab) = Mux::get().get_tab(id) {
                        if self.config.tab_close_confirmation.should_prompt(true, || {
                            tab.can_close_without_prompting(CloseReason::Tab)
                        }) {
                            self.touch_panel = Some(TouchPanel::Confirm(id));
                        } else {
                            self.touch_close_confirmed(id);
                        }
                    }
                }
            }
            TouchAction::ConfirmClose(id) => {
                self.touch_close_confirmed(id);
            }
            TouchAction::Command(id, cmd) => {
                let active_pane = self
                    .get_active_pane_or_overlay()
                    .filter(|pane| pane.pane_id() == id);
                let target_tab = Mux::get()
                    .get_window(self.mux_window_id)
                    .and_then(|window| {
                        window.iter().position(|tab| {
                            tab.iter_panes_ignoring_zoom()
                                .iter()
                                .any(|p| p.pane.pane_id() == id)
                        })
                    });
                if let Some(pane) = active_pane.clone().or_else(|| Mux::get().get_pane(id)) {
                    // A snapshot may outlive pane removal or a move to another
                    // window. Commands must still belong to this window.
                    if active_pane.is_some() || target_tab.is_some() {
                        if let Some(idx) = target_tab {
                            let _ = self.activate_tab(idx as isize);
                        }
                        let assignment = match cmd {
                            TouchCommand::Copy => {
                                KeyAssignment::CopyTo(ClipboardCopyDestination::Clipboard)
                            }
                            TouchCommand::Paste => {
                                KeyAssignment::PasteFrom(ClipboardPasteSource::Clipboard)
                            }
                            TouchCommand::Larger => KeyAssignment::IncreaseFontSize,
                            TouchCommand::Smaller => KeyAssignment::DecreaseFontSize,
                            TouchCommand::Zoom => KeyAssignment::TogglePaneZoomState,
                        };
                        let _ = self.perform_key_assignment(&pane, &assignment);
                    }
                }
            }
            TouchAction::NewTab => {
                if let Some(pane) = self.get_active_pane_or_overlay() {
                    let _ = self.perform_key_assignment(
                        &pane,
                        &KeyAssignment::SpawnTab(SpawnTabDomain::CurrentPaneDomain),
                    );
                }
            }
            TouchAction::Dismiss | TouchAction::ScrollStrip => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scrolling_reaches_every_tab_and_reveals_keyboard_selection() {
        for width in [320., 375., 599., 600., 601., 768., 1280.] {
            let viewport = width - 56. - 40. - 48.;
            let total = 20. * 140.;
            assert_eq!(clamped_scroll(-40., total, viewport), 0.);
            assert_eq!(clamped_scroll(total, total, viewport) + viewport, total);
            for tab in 0..20 {
                let start = tab as f32 * 140.;
                let scroll = clamped_scroll(
                    reveal_tab(0., start, start + 140., viewport),
                    total,
                    viewport,
                );
                assert!(start >= scroll && start + 140. <= scroll + viewport);
            }
        }
        assert_eq!(clamped_scroll(800., 100., 300.), 0.);
    }
    #[test]
    fn panels_stay_between_native_borders_and_the_bar() {
        for height in [240., 360., 640., 1024.] {
            for top in [0., 6., 16., 32.] {
                for bottom in [0., 8., 16.] {
                    for at_bottom in [false, true] {
                        let bar_y = if at_bottom {
                            height - 52. - bottom
                        } else {
                            top
                        };
                        let (start, end) = panel_range(height, top, bottom, bar_y, 52., at_bottom);
                        assert!(start >= top && end <= height - bottom);
                        if at_bottom {
                            assert!(end <= bar_y);
                        } else {
                            assert!(start >= bar_y + 52.);
                        }
                    }
                }
            }
        }
    }
    #[test]
    fn releasing_over_a_replaced_tab_cannot_close_it() {
        let first = TouchAction::Close(TabId::from(1));
        let item = UIItem {
            x: 0,
            y: 0,
            width: 44,
            height: 52,
            item_type: UIItemType::Touch(first),
        };
        let press = TouchPress {
            action: first,
            item: item.clone(),
            x: 10,
            y: 10,
            scroll_start: 0.,
            started: Instant::now(),
            held: false,
            cancelled: false,
        };
        let mut replacement = item.clone();
        replacement.item_type = UIItemType::Touch(TouchAction::Close(TabId::from(2)));
        assert_eq!(press.release(Some(&replacement)), None);
        assert_eq!(press.release(Some(&item)), Some(first));
    }
    #[test]
    fn touch_release_cancels_drag_out_and_preserves_stable_target() {
        let item = UIItem {
            x: 0,
            y: 0,
            width: 44,
            height: 52,
            item_type: UIItemType::Touch(TouchAction::Toggle),
        };
        let mut press = TouchPress {
            action: TouchAction::Toggle,
            item: item.clone(),
            x: 10,
            y: 10,
            scroll_start: 0.,
            started: Instant::now(),
            held: false,
            cancelled: false,
        };
        assert_eq!(press.release(Some(&item)), Some(TouchAction::Toggle));
        assert_eq!(press.release(None), None);
        press.moved(30, 10, 1.);
        assert_eq!(press.release(Some(&item)), None);
        press.cancelled = false;
        press.held = true;
        assert_eq!(press.release(Some(&item)), None);
    }
}
