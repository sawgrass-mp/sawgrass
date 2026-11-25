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
//! ```
//!
//! `offset=N` moves the result N bytes from the start of the match (N may be negative). `rel32=D,E` then follows a
//! relative reference: it reads the little-endian `i32` at D bytes past that point and adds it to the address E bytes
//! past it, as a `call rel32` does with D = 1, E = 5.
//!
//! [`FunctionTable::resolve`] scans the code image for every function the module requires, every time the module
//! starts. A game patch or a runtime update changes the build, the table no longer matches, and the module refuses
//! to start until a table for the new build exists; scanning again with the old table would risk hooking the wrong
//! code.

use std::collections::BTreeMap;
use std::fmt;

use crate::pattern::{Pattern, PatternError, ScanError};
use crate::runtime::{CodeImage, GameBuild};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub pattern: Pattern,
    pub offset: i64,
    /// `(displacement offset, instruction end)` of a relative reference to follow.
    pub rel32: Option<(u32, u32)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionTable {
    /// The [`GameBuild::key`] the table was written for.
    pub build: String,
    pub entries: BTreeMap<String, Entry>,
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

/// Addresses of the required functions, valid only for the build they were found in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedFunctions {
    pub build: GameBuild,
    pub addresses: BTreeMap<String, u64>,
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
        other => return Err(format!("unknown modifier `{other}`")),
    }
    Ok(())
}

impl FunctionTable {
    pub fn parse(text: &str) -> Result<Self, TableError> {
        let mut build = None;
        let mut entries = BTreeMap::new();
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
            if name.is_empty()
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
            {
                return Err(syntax(format!(
                    "bad function name `{name}` (lowercase, digits and `_` only)"
                )));
            }
            let mut parts = rest.split(';');
            let pattern = Pattern::parse(parts.next().unwrap_or(""))
                .map_err(|error| TableError::Pattern { line, error })?;
            let mut entry = Entry {
                pattern,
                offset: 0,
                rel32: None,
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
        })
    }

    /// Locates every function in `required` in `image`, for the running `build`.
    pub fn resolve(
        &self,
        build: &GameBuild,
        image: CodeImage<'_>,
        required: &[&str],
    ) -> Result<ResolvedFunctions, ResolveError> {
        let running = build.key();
        if self.build != running {
            return Err(ResolveError::WrongBuild {
                table: self.build.clone(),
                running,
            });
        }
        let mut addresses = BTreeMap::new();
        let mut failed = Vec::new();
        for &name in required {
            let located = match self.entries.get(name) {
                None => Err(LookupError::NotInTable),
                Some(entry) => locate(entry, image.bytes),
            };
            match located {
                Ok(offset) => {
                    addresses.insert(name.to_owned(), image.base + offset as u64);
                }
                Err(error) => failed.push((name.to_owned(), error)),
            }
        }
        if failed.is_empty() {
            Ok(ResolvedFunctions {
                build: build.clone(),
                addresses,
            })
        } else {
            Err(ResolveError::Functions(failed))
        }
    }
}

/// The offset of one function inside `bytes`.
fn locate(entry: &Entry, bytes: &[u8]) -> Result<usize, LookupError> {
    let found = entry.pattern.find_unique(bytes).map_err(LookupError::Scan)?;
    let at = i64::try_from(found)
        .ok()
        .and_then(|f| f.checked_add(entry.offset));
    let at = at
        .and_then(|a| usize::try_from(a).ok())
        .ok_or(LookupError::OutOfImage)?;
    let target = match entry.rel32 {
        None => at,
        Some((disp, end)) => {
            let disp_at = at.checked_add(disp as usize).ok_or(LookupError::OutOfImage)?;
            let raw = bytes.get(disp_at..disp_at + 4).ok_or(LookupError::OutOfImage)?;
            let displacement = i32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]);
            let next = at.checked_add(end as usize).ok_or(LookupError::OutOfImage)?;
            next.checked_add_signed(displacement as isize)
                .ok_or(LookupError::OutOfImage)?
        }
    };
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
