//! Rejoin a link that a TUI hard-wrapped across rows.
//!
//! Claude Code and Codex wrap long tokens themselves and emit a real newline
//! plus an indent, so the terminal never sets the wrap attribute and each
//! fragment is scanned on its own (#547). Joining rows by shape alone was
//! reverted twice because unrelated output can fabricate a target, so this
//! module only proposes candidates. The caller keeps a file path only when
//! it exists on disk, and a URL only on the alternate screen across rows
//! whose content fills the full width, which is where Claude Code breaks a
//! token that does not fit; a word wrap always ends short of it.

use std::ops::Range;
use std::sync::Arc;
use termwiz::hyperlink::{Hyperlink, Rule};
use termwiz::surface::Line;

/// Rows above and below the hovered row that a rejoined link may span.
pub const MAX_SPAN: usize = 4;

/// Columns a TUI may leave free at the right edge when it wraps there.
/// Claude Code stops about four columns short; a wide glyph adds one.
const EDGE_SLACK: usize = 6;

pub struct LinkRow<'a> {
    pub text: &'a str,
    /// The terminal wrap attribute: the next row continues this one verbatim.
    pub soft_wrapped: bool,
    /// The row's content runs to the right edge, as it does when a TUI
    /// breaks a token that does not fit.
    pub reaches_edge: bool,
    /// The row's content ends in the last column.
    pub fills_width: bool,
}

impl<'a> LinkRow<'a> {
    pub fn new(line: &Line, text: &'a str, cols: usize) -> Self {
        let content_end = line
            .visible_cells()
            .filter(|cell| !cell.str().trim().is_empty())
            .map(|cell| cell.cell_index() + cell.width())
            .last()
            .unwrap_or(0);
        Self {
            text,
            soft_wrapped: line.last_cell_was_wrapped(),
            reaches_edge: content_end + EDGE_SLACK >= cols,
            fills_width: content_end >= cols,
        }
    }
}

/// A link found by joining rows, with the byte range of each row's text
/// that it covers.
pub struct Rejoined {
    pub link: Arc<Hyperlink>,
    pub segments: Vec<(usize, Range<usize>)>,
}

impl Rejoined {
    pub fn is_file(&self) -> bool {
        self.link.uri().starts_with("file://")
    }
}

/// Byte offset in `line.as_str()` of the cell that covers `column`.
pub fn byte_offset_of_column(line: &Line, column: usize) -> Option<usize> {
    let mut offset = 0;
    for cell in line.visible_cells() {
        let start = cell.cell_index();
        if (start..start + cell.width().max(1)).contains(&column) {
            return Some(offset);
        }
        offset += cell.str().len();
    }
    None
}

/// Cell indices whose text starts inside `bytes` of `line.as_str()`.
pub fn cells_in_byte_range(line: &Line, bytes: &Range<usize>) -> Vec<usize> {
    let mut offset = 0;
    let mut cells = vec![];
    for cell in line.visible_cells() {
        if bytes.contains(&offset) {
            cells.push(cell.cell_index());
        }
        offset += cell.str().len();
    }
    cells
}

/// Links that cover byte `hover_byte` of `rows[hover_row]` once the row is
/// joined with its hard-wrapped neighbours, longest join first. Only links
/// that cross a row boundary are returned, since the hovered fragment on
/// its own is what failed. With `require_edge`, a hard break counts only
/// when the row above it reaches the right edge.
pub fn rejoined_links(
    rows: &[LinkRow],
    hover_row: usize,
    hover_byte: usize,
    rules: &[Rule],
    require_edge: bool,
) -> Vec<Rejoined> {
    if hover_row >= rows.len() {
        return vec![];
    }
    let joins = |upper: usize| -> bool {
        let (above, below) = (&rows[upper], &rows[upper + 1]);
        above.soft_wrapped
            || (!above.text.trim().is_empty()
                && !below.text.trim().is_empty()
                && (!require_edge || above.reaches_edge))
    };
    let mut first = hover_row;
    while first > 0 && hover_row - first < MAX_SPAN && joins(first - 1) {
        first -= 1;
    }
    let mut last = hover_row;
    while last + 1 < rows.len() && last - hover_row < MAX_SPAN && joins(last) {
        last += 1;
    }

    let mut spans = vec![];
    for top in first..=hover_row {
        for bottom in hover_row..=last {
            if top != bottom {
                spans.push((top, bottom));
            }
        }
    }
    spans.sort_by_key(|&(top, bottom)| std::cmp::Reverse(bottom - top));

    let mut found: Vec<Rejoined> = vec![];
    for (top, bottom) in spans {
        // (row, start in joined, start in row text, length) for each piece
        let mut pieces = vec![];
        let mut joined = String::new();
        let mut hovered = None;
        for idx in top..=bottom {
            let mut piece = rows[idx].text;
            let mut lead = 0;
            if idx > top && !rows[idx - 1].soft_wrapped {
                let trimmed = piece.trim_start();
                lead = piece.len() - trimmed.len();
                piece = trimmed;
            }
            if idx < bottom && !rows[idx].soft_wrapped {
                piece = piece.trim_end();
            }
            let start = joined.len();
            if idx == hover_row {
                if hover_byte < lead || hover_byte - lead >= piece.len() {
                    break;
                }
                hovered = Some((start + hover_byte - lead, start..start + piece.len()));
            }
            pieces.push((idx, start, lead, piece.len()));
            joined.push_str(piece);
        }
        let Some((offset, hovered_piece)) = hovered else {
            continue;
        };
        for m in Rule::match_hyperlinks(&joined, rules) {
            let crosses = m.range.start < hovered_piece.start || m.range.end > hovered_piece.end;
            if !crosses
                || !m.range.contains(&offset)
                || found.iter().any(|r| r.link.uri() == m.link.uri())
            {
                continue;
            }
            let segments = pieces
                .iter()
                .filter_map(|&(row, start, lead, len)| {
                    let from = m.range.start.max(start);
                    let to = m.range.end.min(start + len);
                    (from < to).then(|| (row, lead + from - start..lead + to - start))
                })
                .collect();
            found.push(Rejoined {
                link: m.link,
                segments,
            });
        }
    }
    found
}

/// Whether every hard break that `rejoined` crosses has the link running
/// into the last column. A URL has no existence check, so a TUI breaking a
/// token that does not fit is the only accepted shape.
pub fn breaks_at_full_width(rows: &[LinkRow], rejoined: &Rejoined) -> bool {
    let crossed = rejoined.segments.len().saturating_sub(1);
    rejoined.segments.iter().take(crossed).all(|(row, bytes)| {
        let row = &rows[*row];
        row.soft_wrapped || (row.fills_width && bytes.end == row.text.trim_end().len())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PATH: &str =
        "/private/tmp/claude-501/-Users-tw93-www-yado/737fb7ad/scratchpad/shots/final-sheet.png";

    fn rules() -> Vec<Rule> {
        config::default_hyperlink_rules()
    }

    fn row(text: &str) -> LinkRow<'_> {
        LinkRow {
            text,
            soft_wrapped: false,
            reaches_edge: true,
            fills_width: false,
        }
    }

    fn uris(found: Vec<Rejoined>) -> Vec<String> {
        found.iter().map(|r| r.link.uri().to_string()).collect()
    }

    #[test]
    fn claude_code_wrapped_path_rejoins_from_either_fragment() {
        // The shape Claude Code printed: a CJK sentence, a path broken
        // mid-word, and the continuation indented by two columns with the
        // sentence running straight on after it.
        let (head, tail) = PATH.split_at(PATH.find("-www-ya").unwrap() + "-www-ya".len());
        let first = format!("  截图拼在一张图里：{head}");
        let second = format!("  {tail}，五张分别是照片删除");
        let rows = [row(&first), row(&second)];
        let expected = vec![format!("file://{PATH}")];

        let in_head = first.find("/private").unwrap() + 3;
        assert_eq!(
            uris(rejoined_links(&rows, 0, in_head, &rules(), false)),
            expected
        );
        let in_tail = second.find("do/").unwrap() + 1;
        let found = rejoined_links(&rows, 1, in_tail, &rules(), false);
        assert_eq!(
            uris(rejoined_links(&rows, 1, in_tail, &rules(), false)),
            expected
        );
        assert_eq!(
            found[0].segments,
            vec![
                (0, first.find("/private").unwrap()..first.len()),
                (1, 2..2 + tail.len()),
            ],
            "segments cover each fragment without the indent or the trailing sentence"
        );
    }

    #[test]
    fn a_tail_without_a_slash_still_rejoins() {
        // `sheet.png` alone is not a link, so hovering it must still find
        // the path that ends on the row above.
        let rows = [row("  see /tmp/kaku-wrap/"), row("  sheet.png here")];
        assert_eq!(
            uris(rejoined_links(&rows, 1, 3, &rules(), false)),
            vec!["file:///tmp/kaku-wrap/sheet.png".to_string()]
        );
    }

    #[test]
    fn path_over_three_rows_rejoins_from_the_middle() {
        let rows = [
            row("  /private/tmp/claude-501/-Users"),
            row("  -tw93-www-yado/737fb7ad/scratchpad"),
            row("  /shots/final-sheet.png"),
        ];
        assert_eq!(
            uris(rejoined_links(&rows, 1, 6, &rules(), false))[0],
            format!("file://{PATH}")
        );
    }

    #[test]
    fn blank_row_ends_the_join() {
        let rows = [
            row("  /private/tmp/claude-501/-Users-tw93-www-ya"),
            row(""),
            row("  do/x.png"),
        ];
        assert!(rejoined_links(&rows, 0, 5, &rules(), false).is_empty());
    }

    #[test]
    fn edge_requirement_rejects_a_short_row() {
        // `cd /Users/tw93/www/` followed by `ls` output: the first row is a
        // complete directory that stops well short of the edge.
        let rows = [
            LinkRow {
                reaches_edge: false,
                ..row("cd /Users/tw93/www/")
            },
            row("kaku  mole  pake"),
        ];
        assert!(rejoined_links(&rows, 0, 5, &rules(), true).is_empty());
        assert_eq!(
            uris(rejoined_links(&rows, 0, 5, &rules(), false)),
            vec!["file:///Users/tw93/www/kaku".to_string()],
            "without the edge requirement the caller's existence check decides"
        );
    }

    #[test]
    fn hovers_in_the_indent_or_outside_a_crossing_link_find_nothing() {
        let rows = [row("  see /tmp/a.png here"), row("  and more")];
        assert!(rejoined_links(&rows, 1, 0, &rules(), false).is_empty());
        assert!(rejoined_links(&rows, 0, 8, &rules(), false).is_empty());
    }

    #[test]
    fn soft_wrapped_rows_keep_their_leading_spaces() {
        let rows = [
            LinkRow {
                soft_wrapped: true,
                ..row("/tmp/a b")
            },
            row(" /c.png"),
        ];
        assert!(rejoined_links(&rows, 0, 2, &rules(), false).is_empty());
    }

    fn urls(rows: &[LinkRow], hover_row: usize, hover_byte: usize) -> Vec<Rejoined> {
        rejoined_links(rows, hover_row, hover_byte, &rules(), true)
            .into_iter()
            .filter(|r| !r.is_file() && breaks_at_full_width(rows, r))
            .collect()
    }

    #[test]
    fn url_broken_at_full_width_rejoins() {
        // Claude Code 2.1.310 at 60 columns, replayed through a terminal
        // emulator: the URL fills the first row to column 60 and continues
        // on the next one. Without the fix the second row's fragment was
        // detected as the relative path `termwindow/render/...`.
        let rows = [
            LinkRow {
                fills_width: true,
                ..row("⏺ Bare: https://github.com/tw93/Kaku/blob/main/kaku-gui/src/")
            },
            row("  termwindow/render/screen_line.rs#L120-L180"),
        ];
        let expected = "https://github.com/tw93/Kaku/blob/main/kaku-gui/src/termwindow/render/screen_line.rs#L120-L180";
        assert_eq!(urls(&rows, 1, 5)[0].link.uri(), expected);
        assert_eq!(urls(&rows, 0, 20)[0].link.uri(), expected);
    }

    #[test]
    fn complete_url_before_a_word_wrap_is_not_glued_to_the_next_word() {
        // A URL that fit, then a word that did not: the row ends short of
        // the last column, so the next word must not join it.
        let rows = [
            row("  see https://example.com/docs/page"),
            row("  carefully please"),
        ];
        assert!(
            !rejoined_links(&rows, 0, 10, &rules(), true).is_empty(),
            "the shape alone would glue the next word"
        );
        assert!(urls(&rows, 0, 10).is_empty());
    }

    #[test]
    fn url_ending_mid_row_above_a_full_row_is_not_joined() {
        // Only the row the link leaves from counts: a full row below does
        // not make the break above it a token break.
        let rows = [
            row("  see https://example.com/docs/page"),
            LinkRow {
                fills_width: true,
                ..row("  next")
            },
        ];
        assert!(urls(&rows, 0, 10).is_empty());
    }
}
