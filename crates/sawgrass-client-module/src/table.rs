//! Function tables: where each game function the module needs is, for one game build.
//!
//! A table is a text file kept outside this repository (ADR 0004: no bytes from the executable here). It names the
//! build it was written for and one pattern per function:
//!
//! ```text
//! # comments and blank lines are ignored
//! build = victory/1.0.0/0123abcd
//! ped_create = 0A ?? 0C 0D
//! vehicle_create = 1A 1B ?? ?? ?? ?? ; offset=2 ; rel32=1,5
//! local_player = @0x2A41F80 ; hash=0x0123456789ABCDEF
//! value script_tls_context = 0x40
//! ```
//!
//! An entry is a byte pattern, or `@` and an offset from the image base found with a debugger. An offset holds only
//! for the exact build the table names, which the `build` line already guarantees; a pattern usually survives a
//! small patch and is the better entry once it is known. An entry may name data (where the local player's ped is
//! kept) as well as a function.
//!
//! `offset=N` moves the result N bytes from the start of the match (N may be negative). `rel32=D,E` then follows a
//! relative reference: it reads the little-endian `i32` at D bytes past that point and adds it to the address E bytes
//! past it, as a `call rel32` does with D = 1, E = 5. `rel8=D,E` does the same with a signed byte, as a short jump.
//!
//! `hash=H` marks the entry as a script native's handler, with the native's hash: the adapter checks the handler's
//! code against it before calling it, and calls it as a native. A `value` line holds a number the adapter needs that
//! is not an address, such as an offset inside a structure of the game.
//!
//! [`FunctionTable::resolve`] scans the code image for every function the module uses, every time the module
//! starts. A game patch or a runtime update changes the build, the table no longer matches, and the module refuses
//! to start until a table for the new build exists; scanning again with the old table would risk hooking the wrong
//! code.

use std::collections::BTreeMap;
use std::fmt;

use crate::pattern::{Pattern, PatternError, ScanError};
use crate::runtime::{CodeImage, GameBuild};

/// How an entry is found in the image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Locator {
    /// Scanned for; it must match exactly once.
    Pattern(Pattern),
    /// At this offset from the image base.
    Offset(u64),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub locator: Locator,
    pub offset: i64,
    /// `(displacement offset, instruction end)` of a relative reference to follow.
    pub rel32: Option<(u32, u32)>,
    /// The same for a signed byte displacement.
    pub rel8: Option<(u32, u32)>,
    /// For a script native's handler, the native's hash.
    pub hash: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionTable {
    /// The [`GameBuild::key`] the table was written for.
    pub build: String,
    pub entries: BTreeMap<String, Entry>,
    /// Numbers that are not addresses, from `value` lines.
    pub values: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TableError {
    MissingBuild,
    Syntax { line: usize, reason: String },
    Pattern { line: usize, error: PatternError },
    Duplicate { line: usize, name: String },
}

impl fmt::Display for TableError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingBuild => write!(f, "function table has no `build = ...` line"),
            Self::Syntax { line, reason } => write!(f, "line {line}: {reason}"),
            Self::Pattern { line, error } => write!(f, "line {line}: {error}"),
            Self::Duplicate { line, name } => write!(f, "line {line}: `{name}` is listed twice"),
        }
    }
}

impl std::error::Error for TableError {}

/// Why one function could not be located.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LookupError {
    NotInTable,
    Scan(ScanError),
    /// The offset or the relative reference points outside the code image.
    OutOfImage,
}

/// Everything that stopped a table from resolving, reported at once so that a new build is fixed in one pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    WrongBuild { table: String, running: String },
    Functions(Vec<(String, LookupError)>),
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongBuild { table, running } => {
                write!(f, "function table is for build {table}, but {running} is running")
            }
            Self::Functions(failed) => {
                write!(f, "{} functions not located:", failed.len())?;
                for (name, error) in failed {
                    write!(f, " {name} ({error:?})")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for ResolveError {}

/// Addresses of the functions located, valid only for the build they were found in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedFunctions {
    pub build: GameBuild,
    pub addresses: BTreeMap<String, u64>,
    /// Optional functions that were not located, and why. What they serve is switched off.
    pub missing: Vec<(String, LookupError)>,
}

impl ResolvedFunctions {
    pub fn address(&self, name: &str) -> Option<u64> {
        self.addresses.get(name).copied()
    }

    /// Whether these addresses may be used with the build that runs now.
    pub fn is_for(&self, build: &GameBuild) -> bool {
        self.build == *build
    }
}

/// A hexadecimal offset, with or without `0x`.
fn parse_offset(text: &str) -> Option<u64> {
    let text = text.trim();
    let digits = text
        .strip_prefix("0x")
        .or_else(|| text.strip_prefix("0X"))
        .unwrap_or(text);
    u64::from_str_radix(digits, 16).ok()
}

fn parse_modifier(entry: &mut Entry, text: &str) -> Result<(), String> {
    let (key, value) = text
        .split_once('=')
        .ok_or_else(|| format!("expected `key=value`, got `{text}`"))?;
    match key.trim() {
        "offset" => {
            entry.offset = value
                .trim()
                .parse()
                .map_err(|_| format!("bad offset `{}`", value.trim()))?
        }
        "rel32" => {
            let (d, e) = value.split_once(',').ok_or("rel32 needs `displacement,end`")?;
            let d = d
                .trim()
                .parse()
                .map_err(|_| format!("bad rel32 displacement `{}`", d.trim()))?;
            let e = e
                .trim()
                .parse()
                .map_err(|_| format!("bad rel32 end `{}`", e.trim()))?;
            entry.rel32 = Some((d, e));
        }
        "rel8" => {
            let (d, e) = value.split_once(',').ok_or("rel8 needs `displacement,end`")?;
            let d = d
                .trim()
                .parse()
                .map_err(|_| format!("bad rel8 displacement `{}`", d.trim()))?;
            let e = e
                .trim()
                .parse()
                .map_err(|_| format!("bad rel8 end `{}`", e.trim()))?;
            entry.rel8 = Some((d, e));
        }
        "hash" => {
            entry.hash = Some(
                parse_offset(value).ok_or_else(|| format!("bad hash `{}` (hexadecimal)", value.trim()))?,
            )
        }
        other => return Err(format!("unknown modifier `{other}`")),
    }
    Ok(())
}

impl FunctionTable {
    pub fn parse(text: &str) -> Result<Self, TableError> {
        let mut build = None;
        let mut entries = BTreeMap::new();
        let mut values = BTreeMap::new();
        for (index, raw) in text.lines().enumerate() {
            let line = index + 1;
            let content = raw.split('#').next().unwrap_or("").trim();
            if content.is_empty() {
                continue;
            }
            let syntax = |reason: String| TableError::Syntax { line, reason };
            let (name, rest) = content
                .split_once('=')
                .ok_or_else(|| syntax("expected `name = pattern`".into()))?;
            let name = name.trim();
            if name == "build" {
                build = Some(rest.trim().to_owned());
                continue;
            }
            let (name, is_value) = match name.strip_prefix("value ") {
                Some(value_name) => (value_name.trim(), true),
                None => (name, false),
            };
            if name.is_empty()
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
            {
                return Err(syntax(format!(
                    "bad function name `{name}` (lowercase, digits and `_` only)"
                )));
            }
            if is_value {
                let value = parse_offset(rest)
                    .ok_or_else(|| syntax(format!("bad value `{}` (hexadecimal)", rest.trim())))?;
                if values.insert(name.to_owned(), value).is_some() {
                    return Err(TableError::Duplicate {
                        line,
                        name: name.to_owned(),
                    });
                }
                continue;
            }
            let mut parts = rest.split(';');
            let first = parts.next().unwrap_or("").trim();
            let locator =
                match first.strip_prefix('@') {
                    Some(offset) => Locator::Offset(parse_offset(offset).ok_or_else(|| {
                        syntax(format!("bad offset `{offset}` (hexadecimal, as `@0x1A2B`)"))
                    })?),
                    None => Locator::Pattern(
                        Pattern::parse(first).map_err(|error| TableError::Pattern { line, error })?,
                    ),
                };
            let mut entry = Entry {
                locator,
                offset: 0,
                rel32: None,
                rel8: None,
                hash: None,
            };
            for modifier in parts {
                parse_modifier(&mut entry, modifier).map_err(syntax)?;
            }
            if entries.insert(name.to_owned(), entry).is_some() {
                return Err(TableError::Duplicate {
                    line,
                    name: name.to_owned(),
                });
            }
        }
        Ok(Self {
            build: build.ok_or(TableError::MissingBuild)?,
            entries,
            values,
        })
    }

    /// Locates every function in `required` and `optional` in `image`, for the running `build`. Any required one
    /// missing is an error; optional ones missing are listed in [`ResolvedFunctions::missing`].
    pub fn resolve(
        &self,
        build: &GameBuild,
        image: CodeImage<'_>,
        required: &[&str],
        optional: &[&str],
    ) -> Result<ResolvedFunctions, ResolveError> {
        let running = build.key();
        if self.build != running {
            return Err(ResolveError::WrongBuild {
                table: self.build.clone(),
                running,
            });
        }
        let mut addresses = BTreeMap::new();
        let (mut failed, mut missing) = (Vec::new(), Vec::new());
        for (names, failures) in [(required, &mut failed), (optional, &mut missing)] {
            for &name in names {
                let located = match self.entries.get(name) {
                    None => Err(LookupError::NotInTable),
                    Some(entry) => locate(entry, image.bytes),
                };
                match located {
                    Ok(offset) => {
                        addresses.insert(name.to_owned(), image.base + offset as u64);
                    }
                    Err(error) => failures.push((name.to_owned(), error)),
                }
            }
        }
        if failed.is_empty() {
            Ok(ResolvedFunctions {
                build: build.clone(),
                addresses,
                missing,
            })
        } else {
            Err(ResolveError::Functions(failed))
        }
    }
}

/// Where the entry's relative reference at `at` in `bytes` leads, or `at` itself without one. The result may lie
/// past `bytes`; a displacement that does not fit in them is `None`.
pub(crate) fn follow(entry: &Entry, bytes: &[u8], at: usize) -> Option<usize> {
    let (disp, end, size) = match (entry.rel32, entry.rel8) {
        (Some((disp, end)), _) => (disp, end, 4),
        (None, Some((disp, end))) => (disp, end, 1),
        (None, None) => return Some(at),
    };
    let disp_at = at.checked_add(disp as usize)?;
    let raw = bytes.get(disp_at..disp_at.checked_add(size)?)?;
    let displacement = match raw {
        [a, b, c, d] => i32::from_le_bytes([*a, *b, *c, *d]) as isize,
        [a] => isize::from(*a as i8),
        _ => return None,
    };
    at.checked_add(end as usize)?.checked_add_signed(displacement)
}

/// The offset of one function inside `bytes`.
fn locate(entry: &Entry, bytes: &[u8]) -> Result<usize, LookupError> {
    let found = match &entry.locator {
        Locator::Pattern(pattern) => pattern.find_unique(bytes).map_err(LookupError::Scan)?,
        Locator::Offset(offset) => usize::try_from(*offset).map_err(|_| LookupError::OutOfImage)?,
    };
    let at = i64::try_from(found)
        .ok()
        .and_then(|f| f.checked_add(entry.offset));
    let at = at
        .and_then(|a| usize::try_from(a).ok())
        .ok_or(LookupError::OutOfImage)?;
    let target = follow(entry, bytes, at).ok_or(LookupError::OutOfImage)?;
    if target < bytes.len() {
        Ok(target)
    } else {
        Err(LookupError::OutOfImage)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::RuntimeKind;

    fn build(digest: &str) -> GameBuild {
        GameBuild {
            runtime: RuntimeKind::Victory,
            title_version: "1.0.0".into(),
            image_digest: digest.into(),
        }
    }

    /// A made-up image: filler, a marker at 0x10, a "call" at 0x20 whose displacement points to 0x40.
    fn image() -> Vec<u8> {
        let mut bytes = vec![0u8; 0x60];
        bytes[0x10..0x14].copy_from_slice(&[0xA1, 0xA2, 0xA3, 0xA4]);
        bytes[0x20] = 0xB1;
        bytes[0x21..0x25].copy_from_slice(&(0x40i32 - 0x25).to_le_bytes());
        bytes[0x25] = 0xB2;
        bytes
    }

    const TABLE: &str = "\
# made-up patterns for a made-up build
build = victory/1.0.0/feed

marker = A1 A2 ?? A4       # comment after an entry
marker_end = A1 A2 ; offset=3
call_target = B1 ?? ?? ?? ?? B2 ; rel32=1,5
";

    fn resolve(table: &FunctionTable, required: &[&str]) -> Result<ResolvedFunctions, ResolveError> {
        let bytes = image();
        table.resolve(
            &build("feed"),
            CodeImage {
                base: 0x1000,
                bytes: &bytes,
            },
            required,
            &[],
        )
    }

    #[test]
    fn resolves_patterns_offsets_and_relative_references() {
        let table = FunctionTable::parse(TABLE).expect("valid table");
        let resolved = resolve(&table, &["marker", "marker_end", "call_target"]).expect("resolves");
        assert_eq!(resolved.address("marker"), Some(0x1010));
        assert_eq!(resolved.address("marker_end"), Some(0x1013));
        assert_eq!(resolved.address("call_target"), Some(0x1040));
        assert!(resolved.is_for(&build("feed")));
        assert!(!resolved.is_for(&build("beef")));
    }

    #[test]
    fn refuses_a_table_for_another_build() {
        let table = FunctionTable::parse(TABLE).expect("valid table");
        let bytes = image();
        let error = table.resolve(
            &build("beef"),
            CodeImage {
                base: 0,
                bytes: &bytes,
            },
            &["marker"],
            &[],
        );
        assert_eq!(
            error,
            Err(ResolveError::WrongBuild {
                table: "victory/1.0.0/feed".into(),
                running: "victory/1.0.0/beef".into()
            })
        );
    }

    #[test]
    fn reports_every_failure_at_once() {
        let text = format!(
            "{TABLE}zeros = 00 00\nlost = EE EE\nbefore = A1 ; offset=-0x20\nfar = A1 ; offset=-17\n"
        );
        assert!(matches!(
            FunctionTable::parse(&text),
            Err(TableError::Syntax { line: 9, .. })
        ));
        let text =
            format!("{TABLE}zeros = 00 00\nlost = EE EE\nbefore = A1 ; offset=-17\nfar = A1 ; offset=80\n");
        let table = FunctionTable::parse(&text).expect("valid table");
        let error = resolve(&table, &["marker", "zeros", "lost", "before", "far", "absent"]);
        let Err(ResolveError::Functions(failed)) = error else {
            panic!("expected failures, got {error:?}")
        };
        let failed: BTreeMap<_, _> = failed.into_iter().collect();
        assert_eq!(failed.len(), 5);
        assert!(matches!(
            failed["zeros"],
            LookupError::Scan(ScanError::Ambiguous { .. })
        ));
        assert_eq!(failed["lost"], LookupError::Scan(ScanError::NotFound));
        assert_eq!(failed["before"], LookupError::OutOfImage);
        assert_eq!(failed["far"], LookupError::OutOfImage);
        assert_eq!(failed["absent"], LookupError::NotInTable);
    }

    #[test]
    fn natives_carry_their_hash_and_values_are_kept_apart() {
        let text = format!(
            "{TABLE}local_player = @0x30 ; hash=0xA86D5F069399F44D\nvalue script_tls_context = 0x1478\nvalue state = 9\n"
        );
        let table = FunctionTable::parse(&text).expect("valid table");
        assert_eq!(table.entries["local_player"].hash, Some(0xA86D_5F06_9399_F44D));
        assert_eq!(table.values["script_tls_context"], 0x1478);
        assert_eq!(table.values["state"], 9);
        assert!(!table.entries.contains_key("script_tls_context"));
        for bad in ["value x = zz", "value Bad = 1", "p = @0x1 ; hash=nope"] {
            assert!(FunctionTable::parse(&format!("{TABLE}{bad}\n")).is_err(), "{bad}");
        }
        let twice = FunctionTable::parse(&format!("{TABLE}value a = 1\nvalue a = 2\n"));
        assert!(matches!(twice, Err(TableError::Duplicate { .. })));
    }

    #[test]
    fn offsets_locate_entries_and_optional_misses_are_listed() {
        let text = format!("{TABLE}player = @0x30\nnear_end = @3F ; offset=-1\nfar = @0x1000\n");
        let table = FunctionTable::parse(&text).expect("valid table");
        let bytes = image();
        let image = CodeImage {
            base: 0x1000,
            bytes: &bytes,
        };
        let resolved = table
            .resolve(
                &build("feed"),
                image,
                &["marker", "player"],
                &["near_end", "far", "absent"],
            )
            .expect("the required entries resolve");
        assert_eq!(resolved.address("player"), Some(0x1030));
        assert_eq!(resolved.address("near_end"), Some(0x103E));
        let missing: BTreeMap<_, _> = resolved.missing.into_iter().collect();
        assert_eq!(missing.len(), 2);
        assert_eq!(missing["far"], LookupError::OutOfImage);
        assert_eq!(missing["absent"], LookupError::NotInTable);

        // A required entry missing still stops the start, whatever the optional ones do.
        let error = table.resolve(&build("feed"), image, &["absent"], &["marker"]);
        assert_eq!(
            error,
            Err(ResolveError::Functions(vec![(
                "absent".into(),
                LookupError::NotInTable
            )]))
        );
        assert!(matches!(
            FunctionTable::parse("build = x\nplayer = @0xZZ"),
            Err(TableError::Syntax { line: 2, .. })
        ));
    }

    #[test]
    fn refuses_malformed_tables() {
        assert_eq!(FunctionTable::parse("marker = A1"), Err(TableError::MissingBuild));
        let cases = [
            ("build = x\nmarker A1", 2),
            ("build = x\nMarker = A1", 2),
            ("build = x\nmarker = A1 ; offset=two", 2),
            ("build = x\nmarker = A1 ; rel32=1", 2),
            ("build = x\nmarker = A1 ; speed=3", 2),
        ];
        for (text, line) in cases {
            assert!(
                matches!(FunctionTable::parse(text), Err(TableError::Syntax { line: l, .. }) if l == line),
                "{text:?}"
            );
        }
        assert!(matches!(
            FunctionTable::parse("build = x\nmarker = ZZ"),
            Err(TableError::Pattern { line: 2, .. })
        ));
        assert_eq!(
            FunctionTable::parse("build = x\nmarker = A1\nmarker = A2"),
            Err(TableError::Duplicate {
                line: 3,
                name: "marker".into()
            })
        );
    }
}
