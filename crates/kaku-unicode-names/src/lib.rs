//! Unicode character names for the character selector (Unicode 16.0.0).
//!
//! The table is stored as zstd-compressed `NAME\tHEX` lines sorted by name
//! and decoded once, the first time it is asked for. As a Rust `const` slice
//! it compiled into ~6.8 MB per architecture; `regenerate.sh` rebuilds the
//! data file from the Unicode Character Database.

use std::sync::OnceLock;

static NAMES_ZST: &[u8] = include_bytes!("names.tsv.zst");

/// Every named character as `(name, codepoint)`, sorted by name.
pub fn names() -> &'static [(&'static str, u32)] {
    static NAMES: OnceLock<Vec<(&'static str, u32)>> = OnceLock::new();
    NAMES.get_or_init(|| {
        // The data file is compiled in and covered by the tests below, so a
        // decode or parse failure can only come from a broken regeneration.
        let text = zstd::stream::decode_all(NAMES_ZST).expect("decode names.tsv.zst");
        let text: &'static str =
            Box::leak(String::from_utf8(text).expect("names.tsv is UTF-8").into_boxed_str());
        text.lines()
            .map(|line| {
                let (name, hex) = line.split_once('\t').expect("NAME\\tHEX line");
                (name, u32::from_str_radix(hex, 16).expect("hex codepoint"))
            })
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::names;

    #[test]
    fn decodes_the_full_sorted_table() {
        let names = names();
        assert_eq!(names.len(), 137_428);
        assert_eq!(names.first(), Some(&("ABACUS", 129_518)));
        assert_eq!(names.last(), Some(&("ZWSP", 8_203)));
        assert!(names.contains(&("ADLAM CAPITAL LETTER KPO", 125_216)));
        assert!(names.windows(2).all(|w| w[0].0 <= w[1].0));
        // The character selector unwraps `char::from_u32` on every entry.
        assert!(names.iter().all(|(_, v)| char::from_u32(*v).is_some()));
    }
}
