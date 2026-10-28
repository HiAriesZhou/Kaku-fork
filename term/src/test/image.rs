//! Tests for inline image protocol handling

use super::{assert_eq, *};

/// A tiny but valid 11x11 PNG, base64 encoded.
/// Taken from the reproduction in <https://github.com/wezterm/wezterm/issues/6344>.
const TINY_PNG_BASE64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAsAAAALCAYAAACprHcmAAAACXBIWXMAAAGKAAABigEzlzBYAAAAOUlEQVQYlZXOwQ0AMAzCQEdi7yaT0xWAN7JuDCac2PQKYxflycOoICOKtPIuqFCg4/LzKxiz6xjyAYh9DR1sLUN1AAAAAElFTkSuQmCC";

/// Feeding a Kitty graphics escape that requests a zero-sized placement (here `r=0,h=0`)
/// must not panic the terminal.
/// Prior to the fix for <https://github.com/wezterm/wezterm/issues/6344> this divided by zero
/// while computing the per-cell pixel deltas and took down the whole pane.
#[test]
fn kitty_zero_dimension_image_does_not_panic() {
    let mut term = TestTerm::new(3, 10, 0);

    // a=T: transmit and display, t=d: data is directly embedded,
    // f=100: PNG, r=0/h=0: zero rows / zero source height.
    let seq = format!("\x1b_Gr=0,h=0,a=T,t=d,f=100;{}\x1b\\", TINY_PNG_BASE64);
    term.print(seq.as_bytes());

    // The image is refused, so the cursor never moved;
    // Printing normal text and observing it confirms we recovered rather than crashing.
    term.print(b"ok");
    assert_visible_contents(&term, file!(), line!(), &["ok", "", ""]);
}

/// A well-formed Kitty graphic with non-zero dimensions should continue to be accepted.
/// The test passes as long as processing the image does not panic and the terminal remains usable.
#[test]
fn kitty_valid_image_is_accepted() {
    let mut term = TestTerm::new(3, 10, 0);

    let seq = format!("\x1b_Ga=T,t=d,f=100;{}\x1b\\", TINY_PNG_BASE64);
    term.print(seq.as_bytes());

    // Printing normal text and observing it shifted confirms the terminal is usable.
    term.print(b"ok");
    assert_visible_contents(&term, file!(), line!(), &["  ok", "", ""]);
}

/// When the pty has no pixel size, `cell_pixel_width`/`cell_pixel_height` are zero.
/// Displaying an image sized in cells (ie: without explicit `c=`/`r=`) must not divide by zero.
/// This is a distinct crash from the zero-dimension image above and is not caught by that guard.
/// See <https://github.com/wezterm/wezterm/issues/6344>.
#[test]
fn kitty_image_with_zero_pixel_dimensions_does_not_panic() {
    let mut term = Terminal::new(
        TerminalSize {
            rows: 3,
            cols: 80,
            // No pixel size!
            pixel_width: 0,
            pixel_height: 0,
            dpi: 0,
        },
        Arc::new(TestTermConfig { scrollback: 0 }),
        "WezTerm",
        "O_o",
        Box::new(Vec::new()),
    );

    // No `c=`/`r=`, so the placement is computed from the (zero) cell pixel
    // size, exercising the divide that previously panicked.
    let seq = format!("\x1b_Ga=T,t=d,f=100;{}\x1b\\", TINY_PNG_BASE64);
    term.advance_bytes(seq.as_bytes());

    // The image is refused, so the cursor never moved;
    // Printing normal text and observing it confirms we recovered rather than crashing.
    term.advance_bytes(b"ok");
    assert_visible_contents(&term, file!(), line!(), &["ok", "", ""]);
}

#[test]
fn kitty_virtual_placement_does_not_draw_or_move_cursor() {
    for action in ["T", "p"] {
        let mut term = TestTerm::new(4, 10, 0);
        term.print(format!("\x1b_Ga=t,i=42,f=100;{TINY_PNG_BASE64}\x1b\\"));
        term.print(format!(
            "\x1b_Ga={action},U=1,i=42,c=3,r=2,f=100;{TINY_PNG_BASE64}\x1b\\"
        ));
        assert_eq!((term.cursor_pos().x, term.cursor_pos().y), (0, 0));
        for line in term.screen().visible_lines() {
            assert!(line
                .visible_cells()
                .all(|cell| cell.attrs().images().is_none()));
        }
    }
}

fn virtual_image(term: &mut TestTerm, id: u32, placement: u32) {
    // Matches the 3 x 2 cell rectangle at the test terminal's 8 x 16 metrics.
    let pixels = "/".repeat(24 * 32 * 4);
    term.print(format!(
        "\x1b_Ga=T,U=1,i={id},p={placement},c=3,r=2,f=24,s=24,v=32;{}\x1b\\",
        pixels
    ));
}

fn placeholder_images(term: &TestTerm, row: usize) -> Vec<wezterm_cell::image::ImageCell> {
    let lines = term.screen().visible_lines();
    let line = term.resolve_kitty_placeholder_line(&lines[row]);
    assert!(!line.as_str().contains('\u{10eeee}'));
    line.visible_cells()
        .flat_map(|cell| cell.attrs().images().unwrap_or_default())
        .collect()
}

#[test]
fn kitty_placeholder_explicit_slices() {
    let mut term = TestTerm::new(4, 10, 0);
    virtual_image(&mut term, 42, 7);
    term.print("\x1b[38;5;42m\x1b[58;5;7m\u{10eeee}\u{030d}\u{030e}");
    let images = placeholder_images(&term, 0);
    assert_eq!(images.len(), 1);
    assert_eq!(images[0].image_id(), Some(42));
    assert_eq!(images[0].placement_id(), Some(7));
    assert_eq!(images[0].top_left().x.into_inner(), 2.0 / 3.0);
    assert_eq!(images[0].top_left().y.into_inner(), 0.5);
    assert_eq!(images[0].bottom_right().x.into_inner(), 1.0);
    assert_eq!(images[0].bottom_right().y.into_inner(), 1.0);
}

#[test]
fn kitty_placeholder_inherits_grid_coordinates() {
    let mut term = TestTerm::new(4, 10, 0);
    virtual_image(&mut term, 42, 0);
    term.print("\x1b[38;5;42m\u{10eeee}\u{0305}\u{10eeee}\u{10eeee}\r\n\u{10eeee}\u{030d}\u{10eeee}\u{10eeee}");
    for row in 0..2 {
        let images = placeholder_images(&term, row);
        assert_eq!(images.len(), 3);
        for (col, image) in images.iter().enumerate() {
            assert_eq!(image.top_left().x.into_inner(), col as f32 / 3.0);
            assert_eq!(image.top_left().y.into_inner(), row as f32 / 2.0);
        }
    }
    // Resolving must not destroy the underlying placeholder text.
    assert!(term.screen().visible_lines()[0]
        .as_str()
        .contains('\u{10eeee}'));
}

#[test]
fn kitty_placeholder_full_image_id_and_partial_inheritance() {
    let mut term = TestTerm::new(4, 10, 0);
    let id = 0x02010203;
    virtual_image(&mut term, id, 0);
    assert_eq!((term.cursor_pos().x, term.cursor_pos().y), (0, 0));
    // Explicit high byte, row-only, then row and column: all inherit byte 2.
    term.print("\x1b[38;2;1;2;3m\u{10eeee}\u{0305}\u{0305}\u{030e}\u{10eeee}\u{0305}\u{10eeee}\u{0305}\u{030e}");
    let images = placeholder_images(&term, 0);
    assert_eq!(images.len(), 3);
    assert!(images.iter().all(|image| image.image_id() == Some(id)));
}

#[test]
fn kitty_placeholder_deleted_or_missing_image_is_blank() {
    let mut term = TestTerm::new(4, 10, 0);
    virtual_image(&mut term, 42, 0);
    term.print("\x1b[38;5;42m\u{10eeee}\u{0305}\u{0305}");
    assert_eq!(placeholder_images(&term, 0).len(), 1);
    let seqno = term.current_seqno();
    term.print("\x1b_Ga=d,d=I,i=42\x1b\\");
    assert!(placeholder_images(&term, 0).is_empty());
    assert!(term.screen().visible_lines()[0].changed_since(seqno));
    term.print("\x1b[38;5;99m\u{10eeee}\u{0305}\u{0305}");
    assert!(placeholder_images(&term, 0).is_empty());
    virtual_image(&mut term, 42, 0);
    assert_eq!(placeholder_images(&term, 0).len(), 1);
}

#[test]
fn kitty_placeholder_inheritance_stops_at_text_and_color_changes() {
    let mut term = TestTerm::new(4, 10, 0);
    virtual_image(&mut term, 0x0200002a, 7);
    term.print("\x1b[38;5;42m\x1b[58;5;7m\u{10eeee}\u{0305}\u{0305}\u{030e} \u{10eeee}\x1b[58;5;8m\u{10eeee}");
    assert_eq!(placeholder_images(&term, 0).len(), 1);
}

#[test]
fn kitty_delete_all_preserves_virtual_placements() {
    let mut term = TestTerm::new(4, 10, 0);
    virtual_image(&mut term, 42, 7);
    term.print("\x1b[38;5;42m\u{10eeee}\u{0305}\u{0305}");
    term.print("\x1b_Ga=d,d=A\x1b\\");
    assert_eq!(placeholder_images(&term, 0).len(), 1);
}

#[test]
fn kitty_reset_discards_virtual_placements() {
    let mut term = TestTerm::new(4, 10, 0);
    virtual_image(&mut term, 42, 0);
    term.print("\x1bc\x1b[38;5;42m\u{10eeee}\u{0305}\u{0305}");
    assert!(placeholder_images(&term, 0).is_empty());
}

#[test]
fn kitty_placeholder_render_lines_preserve_screen_text() {
    let mut term = TestTerm::new(4, 10, 0);
    assert!(term.kitty_placeholder_lines(0..2).is_none());
    virtual_image(&mut term, 42, 7);
    term.print("\x1b[38;5;42m\u{10eeee}\u{0305}\u{10eeee}\u{10eeee}");
    let (first, rendered) = term.kitty_placeholder_lines(0..2).unwrap();
    assert_eq!(first, 0);
    assert_eq!(rendered.len(), 2);
    assert_eq!(
        rendered[0]
            .visible_cells()
            .filter(|cell| cell.attrs().images().is_some())
            .count(),
        3
    );
    assert!(!rendered[0].as_str().contains('\u{10eeee}'));
    assert!(term.screen().visible_lines()[0]
        .as_str()
        .contains('\u{10eeee}'));
}

#[test]
fn kitty_placeholder_delete_one_placement_keeps_sibling() {
    let mut term = TestTerm::new(4, 10, 0);
    virtual_image(&mut term, 42, 7);
    term.print("\x1b_Ga=p,U=1,i=42,p=8,c=3,r=2\x1b\\");
    term.print(
        "\x1b[38;5;42m\x1b[58;5;7m\u{10eeee}\u{0305}\u{0305}\x1b[58;5;8m\u{10eeee}\u{0305}\u{0305}",
    );
    assert_eq!(placeholder_images(&term, 0).len(), 2);
    term.print("\x1b_Ga=d,d=I,i=42,p=7\x1b\\");
    let images = placeholder_images(&term, 0);
    assert_eq!(images.len(), 1);
    assert_eq!(images[0].placement_id(), Some(8));
}

#[test]
fn kitty_placeholder_respects_graphics_gate() {
    #[derive(Debug)]
    struct Disabled;
    impl TerminalConfiguration for Disabled {
        fn color_palette(&self) -> ColorPalette {
            ColorPalette::default()
        }
    }
    let mut term = TestTerm::new(4, 10, 0);
    term.set_config(Arc::new(Disabled));
    virtual_image(&mut term, 42, 0);
    term.print("\x1b[38;5;42m\u{10eeee}\u{0305}\u{0305}");
    assert!(term.kitty_placeholder_lines(0..1).is_none());
    term.set_config(Arc::new(TestTermConfig { scrollback: 0 }));
    assert!(placeholder_images(&term, 0).is_empty());
}

#[test]
fn kitty_placeholder_chunked_transmission_and_split_diacritics() {
    let mut term = TestTerm::new(4, 10, 0);
    let data = "/".repeat(24 * 32 * 4);
    term.print(format!(
        "\x1b_Ga=T,U=1,i=42,c=3,r=2,f=24,s=24,v=32,m=1;{}\x1b\\",
        &data[..1024]
    ));
    term.print(format!("\x1b_Gm=0;{}\x1b\\", &data[1024..]));
    assert_eq!((term.cursor_pos().x, term.cursor_pos().y), (0, 0));
    for c in "\x1b[38;5;42m\u{10eeee}\u{030d}\u{030e}".chars() {
        term.print(c.to_string());
    }
    let images = placeholder_images(&term, 0);
    assert_eq!(images.len(), 1);
    assert_eq!(images[0].top_left().x.into_inner(), 2.0 / 3.0);
    assert_eq!(images[0].top_left().y.into_inner(), 0.5);
}
