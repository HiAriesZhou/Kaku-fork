//! Rejoin a file path that a TUI hard-wrapped across rows.
//!
//! Claude Code and Codex wrap long tokens themselves and emit a real newline
//! plus an indent, so the terminal never sets the wrap attribute and each
//! fragment is scanned as its own path (#547). Joining rows by shape alone
//! was reverted twice because unrelated output can fabricate a target, so
//! this module only proposes candidates: the caller keeps one only when the
//! joined path exists on disk.

use std::sync::Arc;
use termwiz::hyperlink::{Hyperlink, Rule};
use termwiz::surface::Line;

/// Rows above and below the clicked row that a rejoined path may span.
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
        }
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

/// File links that cover byte `click_byte` of `rows[click_row]` once the
/// row is joined with its hard-wrapped neighbours, longest join first.
/// Only links that cross a row boundary are returned, since the clicked
/// fragment on its own is what failed. With `require_edge`, a hard break
/// counts only when the row above it reaches the right edge.
pub fn rejoined_file_links(
    rows: &[LinkRow],
    click_row: usize,
    click_byte: usize,
    rules: &[Rule],
    require_edge: bool,
) -> Vec<Arc<Hyperlink>> {
    if click_row >= rows.len() {
        return vec![];
    }
    let joins = |upper: usize| -> bool {
        let (above, below) = (&rows[upper], &rows[upper + 1]);
        above.soft_wrapped
            || (!above.text.trim().is_empty()
                && !below.text.trim().is_empty()
                && (!require_edge || above.reaches_edge))
    };
    let mut first = click_row;
    while first > 0 && click_row - first < MAX_SPAN && joins(first - 1) {
        first -= 1;
    }
    let mut last = click_row;
    while last + 1 < rows.len() && last - click_row < MAX_SPAN && joins(last) {
        last += 1;
    }

    let mut spans = vec![];
    for top in first..=click_row {
        for bottom in click_row..=last {
            if top != bottom {
                spans.push((top, bottom));
            }
        }
    }
    spans.sort_by_key(|&(top, bottom)| std::cmp::Reverse(bottom - top));

    let mut found: Vec<Arc<Hyperlink>> = vec![];
    for (top, bottom) in spans {
        let mut joined = String::new();
        let mut clicked = None;
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
            if idx == click_row {
                if click_byte < lead || click_byte - lead >= piece.len() {
                    break;
                }
                let start = joined.len();
                clicked = Some((start + click_byte - lead, start..start + piece.len()));
            }
            joined.push_str(piece);
        }
        let Some((offset, clicked_row)) = clicked else {
            continue;
        };
        for m in Rule::match_hyperlinks(&joined, rules) {
            let crosses = m.range.start < clicked_row.start || m.range.end > clicked_row.end;
            if crosses
                && m.range.contains(&offset)
                && m.link.uri().starts_with("file://")
                && !found.iter().any(|link| link.uri() == m.link.uri())
            {
                found.push(m.link);
            }
        }
    }
    found
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
        }
    }

    fn uris(links: Vec<Arc<Hyperlink>>) -> Vec<String> {
        links.iter().map(|link| link.uri().to_string()).collect()
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
            uris(rejoined_file_links(&rows, 0, in_head, &rules(), false)),
            expected
        );
        let in_tail = second.find("do/").unwrap() + 1;
        assert_eq!(
            uris(rejoined_file_links(&rows, 1, in_tail, &rules(), false)),
            expected
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
            uris(rejoined_file_links(&rows, 1, 6, &rules(), false))[0],
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
        assert!(rejoined_file_links(&rows, 0, 5, &rules(), false).is_empty());
    }

    #[test]
    fn edge_requirement_rejects_a_short_row() {
        // `cd /Users/tw93/www/` followed by `ls` output: the first row is a
        // complete directory that stops well short of the edge.
        let rows = [
            LinkRow {
                text: "cd /Users/tw93/www/",
                soft_wrapped: false,
                reaches_edge: false,
            },
            row("kaku  mole  pake"),
        ];
        assert!(rejoined_file_links(&rows, 0, 5, &rules(), true).is_empty());
        assert_eq!(
            uris(rejoined_file_links(&rows, 0, 5, &rules(), false)),
            vec!["file:///Users/tw93/www/kaku".to_string()],
            "without the edge requirement the caller's existence check decides"
        );
    }

    #[test]
    fn clicks_in_the_indent_or_outside_a_crossing_link_find_nothing() {
        let rows = [row("  see /tmp/a.png here"), row("  and more")];
        assert!(rejoined_file_links(&rows, 1, 0, &rules(), false).is_empty());
        assert!(rejoined_file_links(&rows, 0, 8, &rules(), false).is_empty());
    }

    #[test]
    fn soft_wrapped_rows_keep_their_leading_spaces() {
        let rows = [
            LinkRow {
                text: "/tmp/a b",
                soft_wrapped: true,
                reaches_edge: true,
            },
            row(" /c.png"),
        ];
        assert!(rejoined_file_links(&rows, 0, 2, &rules(), false).is_empty());
    }
}
