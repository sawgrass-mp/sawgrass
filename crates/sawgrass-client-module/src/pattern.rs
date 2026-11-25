//! Byte patterns with wildcards, and a scanner that finds them in a code image.
//!
//! A pattern is written as hex bytes separated by spaces, with `?` or `??` for a byte that may be anything:
//! `"0A ?? 0C"`. This repository never stores patterns taken from the game (ADR 0004): they live in a function table
//! outside it (see [`crate::table`]), and the tests here use made-up bytes.

use std::fmt;

/// A parsed pattern: `Some(byte)` must match exactly, `None` matches any byte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pattern {
    bytes: Vec<Option<u8>>,
    /// Index of the first exact byte, used to skip quickly through the image.
    anchor: usize,
}

/// Why a pattern string could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatternError {
    Empty,
    /// Every byte is a wildcard, so the pattern would match everywhere.
    OnlyWildcards,
    /// A token that is neither two hex digits nor a wildcard.
    BadToken(String),
}

impl fmt::Display for PatternError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "empty pattern"),
            Self::OnlyWildcards => write!(f, "pattern has only wildcards"),
            Self::BadToken(t) => write!(
                f,
                "bad pattern token `{t}` (expected two hex digits, `?` or `??`)"
            ),
        }
    }
}

impl std::error::Error for PatternError {}

/// Why a scan did not give exactly one match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanError {
    NotFound,
    /// More than one match: the pattern is too short for this build and must be made longer.
    Ambiguous {
        matches: usize,
    },
}

impl fmt::Display for ScanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => write!(f, "no match"),
            Self::Ambiguous { matches } => write!(f, "{matches} matches, expected one"),
        }
    }
}

impl std::error::Error for ScanError {}

impl Pattern {
    pub fn parse(text: &str) -> Result<Self, PatternError> {
        let bytes = text
            .split_ascii_whitespace()
            .map(|token| match token {
                "?" | "??" => Ok(None),
                t if t.len() == 2 => u8::from_str_radix(t, 16)
                    .map(Some)
                    .map_err(|_| PatternError::BadToken(t.to_owned())),
                t => Err(PatternError::BadToken(t.to_owned())),
            })
            .collect::<Result<Vec<_>, _>>()?;
        if bytes.is_empty() {
            return Err(PatternError::Empty);
        }
        let anchor = bytes
            .iter()
            .position(Option::is_some)
            .ok_or(PatternError::OnlyWildcards)?;
        Ok(Self { bytes, anchor })
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Whether the pattern matches `window`, which must be exactly as long as the pattern.
    fn matches_at(&self, window: &[u8]) -> bool {
        self.bytes
            .iter()
            .zip(window)
            .all(|(p, b)| p.is_none_or(|p| p == *b))
    }

    /// Offsets of every match in `haystack`, in increasing order. Matches may overlap.
    pub fn find_all<'a>(&'a self, haystack: &'a [u8]) -> impl Iterator<Item = usize> + 'a {
        let anchor_byte = self.bytes[self.anchor];
        let last_start = haystack.len().checked_sub(self.len());
        let mut next = 0usize;
        std::iter::from_fn(move || {
            let last_start = last_start?;
            while next <= last_start {
                // Jump to the next place where the anchor byte fits, then check the whole window.
                let search_from = next + self.anchor;
                let search_to = last_start + self.anchor;
                let found = haystack[search_from..=search_to]
                    .iter()
                    .position(|b| Some(*b) == anchor_byte)?;
                let start = next + found;
                next = start + 1;
                if self.matches_at(&haystack[start..start + self.len()]) {
                    return Some(start);
                }
            }
            None
        })
    }

    /// The offset of the only match in `haystack`.
    pub fn find_unique(&self, haystack: &[u8]) -> Result<usize, ScanError> {
        let mut found = self.find_all(haystack);
        let first = found.next().ok_or(ScanError::NotFound)?;
        match found.count() {
            0 => Ok(first),
            more => Err(ScanError::Ambiguous { matches: more + 1 }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(text: &str) -> Pattern {
        Pattern::parse(text).expect("valid pattern")
    }

    #[test]
    fn parses_bytes_and_wildcards() {
        let pattern = p("0a ?? 0C ?");
        assert_eq!(pattern.bytes, vec![Some(0x0A), None, Some(0x0C), None]);
        assert_eq!(pattern.anchor, 0);
        assert_eq!(p("?? ?? 7F").anchor, 2);
    }

    #[test]
    fn refuses_bad_patterns() {
        assert_eq!(Pattern::parse("  "), Err(PatternError::Empty));
        assert_eq!(Pattern::parse("?? ?"), Err(PatternError::OnlyWildcards));
        assert_eq!(Pattern::parse("0A G1"), Err(PatternError::BadToken("G1".into())));
        assert_eq!(Pattern::parse("0A0B"), Err(PatternError::BadToken("0A0B".into())));
    }

    #[test]
    fn finds_every_match_including_overlaps() {
        let hay = [0x01, 0x0A, 0x0A, 0x0A, 0x02, 0x0A, 0x0A];
        assert_eq!(p("0A 0A").find_all(&hay).collect::<Vec<_>>(), vec![1, 2, 5]);
        assert_eq!(p("0A ?? 0A").find_all(&hay).collect::<Vec<_>>(), vec![1, 3]);
    }

    #[test]
    fn leading_wildcards_and_edges() {
        let hay = [0x7F, 0x01, 0x02, 0x7F];
        // A leading wildcard cannot match before the start of the image.
        assert_eq!(p("?? 7F").find_all(&hay).collect::<Vec<_>>(), vec![2]);
        // A match that ends on the last byte is found; one that would run past the end is not.
        assert_eq!(p("02 7F").find_all(&hay).collect::<Vec<_>>(), vec![2]);
        assert_eq!(p("7F ??").find_all(&hay).collect::<Vec<_>>(), vec![0]);
        assert_eq!(p("01 02 7F 00 00").find_all(&hay).count(), 0);
        assert_eq!(p("01").find_all(&[]).count(), 0);
    }

    #[test]
    fn unique_match_or_a_reason() {
        let hay = [0xAA, 0xBB, 0xCC, 0xAA, 0xBB, 0xDD];
        assert_eq!(p("AA BB CC").find_unique(&hay), Ok(0));
        assert_eq!(
            p("AA BB ??").find_unique(&hay),
            Err(ScanError::Ambiguous { matches: 2 })
        );
        assert_eq!(p("EE").find_unique(&hay), Err(ScanError::NotFound));
    }

    #[test]
    fn agrees_with_a_naive_scan_on_pseudo_random_data() {
        // A small xorshift so the test needs no dependency and is the same on every run.
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        // Four byte values only, so short patterns match often.
        let hay: Vec<u8> = (0..4096).map(|_| (next() % 4) as u8).collect();
        for _ in 0..200 {
            let len = 1 + (next() % 6) as usize;
            let mut tokens: Vec<String> = (0..len)
                .map(|_| {
                    if next() % 3 == 0 {
                        "??".into()
                    } else {
                        format!("{:02X}", next() % 4)
                    }
                })
                .collect();
            if tokens.iter().all(|t| t == "??") {
                tokens[0] = "01".into();
            }
            let pattern = p(&tokens.join(" "));
            let naive: Vec<usize> = (0..=hay.len() - len)
                .filter(|&i| pattern.matches_at(&hay[i..i + len]))
                .collect();
            assert_eq!(
                pattern.find_all(&hay).collect::<Vec<_>>(),
                naive,
                "pattern {tokens:?}"
            );
        }
    }
}
