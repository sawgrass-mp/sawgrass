//! Asset tables: which game asset each protocol [`AssetId`] stands for, for one game build.
//!
//! The protocol names models, animations and other assets by an opaque `u32` (ADR 0006), so the same id
//! means the same thing on every client whatever the build. What it means in the game can change with a patch, so
//! this table is kept per build like the function table, outside this repository:
//!
//! ```text
//! build = victory/1.0.0/0123abcd
//! 17 = 0x1234      # asset id = the game's value for it, decimal or 0x hex
//! ped = 0x2000      # the ped model for a ped the server names no model for (asset id 0)
//! ```
//!
//! Players carry no model in the protocol yet (`AssetId::NONE`), so the `ped` line is what the first milestone, two
//! players who see each other, needs.

use std::collections::HashMap;

use crate::runtime::GameBuild;
use crate::table::TableError;

use crate::game::GameAsset;

pub use sawgrass_protocol::entity::AssetId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssetTable {
    pub build: String,
    to_game: HashMap<AssetId, GameAsset>,
    from_game: HashMap<GameAsset, AssetId>,
    default_ped: Option<GameAsset>,
}

fn number(text: &str) -> Option<u64> {
    match text.strip_prefix("0x") {
        Some(hex) => u64::from_str_radix(hex, 16).ok(),
        None => text.parse().ok(),
    }
}

impl AssetTable {
    pub fn parse(text: &str) -> Result<Self, TableError> {
        let mut build = None;
        let mut to_game = HashMap::new();
        let mut from_game = HashMap::new();
        let mut default_ped = None;
        for (index, raw) in text.lines().enumerate() {
            let line = index + 1;
            let content = raw.split('#').next().unwrap_or("").trim();
            if content.is_empty() {
                continue;
            }
            let syntax = |reason: String| TableError::Syntax { line, reason };
            let (key, value) = content
                .split_once('=')
                .ok_or_else(|| syntax("expected `id = value`".into()))?;
            let (key, value) = (key.trim(), value.trim());
            if key == "build" {
                build = Some(value.to_owned());
                continue;
            }
            if key == "ped" {
                let asset = number(value).ok_or_else(|| syntax(format!("bad game value `{value}`")))?;
                default_ped = Some(GameAsset(asset));
                continue;
            }
            let id = key
                .parse()
                .map(AssetId)
                .map_err(|_| syntax(format!("bad asset id `{key}`")))?;
            if id == AssetId::NONE {
                return Err(syntax("asset id 0 means \"none\" in the protocol".into()));
            }
            let asset = number(value)
                .map(GameAsset)
                .ok_or_else(|| syntax(format!("bad game value `{value}`")))?;
            if to_game.insert(id, asset).is_some() || from_game.insert(asset, id).is_some() {
                return Err(TableError::Duplicate {
                    line,
                    name: content.to_owned(),
                });
            }
        }
        Ok(Self {
            build: build.ok_or(TableError::MissingBuild)?,
            to_game,
            from_game,
            default_ped,
        })
    }

    pub fn is_for(&self, build: &GameBuild) -> bool {
        self.build == build.key()
    }

    pub fn game_asset(&self, id: AssetId) -> Option<GameAsset> {
        self.to_game.get(&id).copied()
    }

    /// The ped model for a ped the server names no model for, from the table's `ped` line.
    pub fn default_ped(&self) -> Option<GameAsset> {
        self.default_ped
    }

    /// The protocol id of an asset the game used by itself, if the server knows it.
    pub fn asset_id(&self, asset: GameAsset) -> Option<AssetId> {
        self.from_game.get(&asset).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::RuntimeKind;

    #[test]
    fn maps_both_ways_for_its_build() {
        let table =
            AssetTable::parse("build = victory/1.0.0/feed\n17 = 0x1F # a car\n18 = 42\n").expect("valid");
        assert_eq!(table.game_asset(AssetId(17)), Some(GameAsset(0x1F)));
        assert_eq!(table.asset_id(GameAsset(42)), Some(AssetId(18)));
        assert_eq!(table.game_asset(AssetId(19)), None);
        assert_eq!(table.default_ped(), None);
        let with_ped = AssetTable::parse("build = x\nped = 0x2000\n").expect("valid");
        assert_eq!(with_ped.default_ped(), Some(GameAsset(0x2000)));
        let build = |digest: &str| GameBuild {
            runtime: RuntimeKind::Victory,
            title_version: "1.0.0".into(),
            image_digest: digest.into(),
        };
        assert!(table.is_for(&build("feed")));
        assert!(!table.is_for(&build("beef")));
    }

    #[test]
    fn refuses_malformed_tables() {
        assert_eq!(AssetTable::parse("17 = 1"), Err(TableError::MissingBuild));
        for text in [
            "build = x\ncar = 1",
            "build = x\n0 = 1",
            "build = x\n17 = 0xZZ",
            "build = x\n17",
        ] {
            assert!(
                matches!(AssetTable::parse(text), Err(TableError::Syntax { line: 2, .. })),
                "{text:?}"
            );
        }
        assert!(matches!(
            AssetTable::parse("build = x\n1 = 5\n1 = 6"),
            Err(TableError::Duplicate { line: 3, .. })
        ));
        assert!(matches!(
            AssetTable::parse("build = x\n1 = 5\n2 = 5"),
            Err(TableError::Duplicate { line: 3, .. })
        ));
    }
}
