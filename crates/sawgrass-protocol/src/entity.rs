//! Entity kinds, opaque game asset ids and the per-kind block fields of
//! [ADR 0006](../../../docs/adr/0006-entity-sync-before-the-game.md).
//!
//! An entity's kind and model are fixed for the life of its id and sent once, when the entity is added to a
//! client's view. Its block is a short list of `u32` fields whose meaning depends on the kind (the ped and vehicle
//! blocks are defined by later tasks); a field that changes is sent in the delta snapshot's `blocks` list
//! ([`crate::delta`]).

use crate::{DecodeError, EncodeError};

/// A game asset (model, animation, weapon, weather type, door) named by an opaque number that only the client
/// module interprets. `0` is "none" everywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, PartialOrd, Ord)]
pub struct AssetId(pub u32);

impl AssetId {
    /// No asset: unarmed, no action, no model.
    pub const NONE: Self = Self(0);
}

/// What an entity is. The values are part of the wire format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum EntityKind {
    /// A player's ped.
    Player = 1,
    /// A ped the server spawned and an owner simulates.
    Npc = 2,
    /// A vehicle.
    Vehicle = 3,
    /// An object the server spawned.
    Object = 4,
}

impl TryFrom<u8> for EntityKind {
    type Error = DecodeError;

    fn try_from(value: u8) -> Result<Self, DecodeError> {
        match value {
            1 => Ok(Self::Player),
            2 => Ok(Self::Npc),
            3 => Ok(Self::Vehicle),
            4 => Ok(Self::Object),
            _ => Err(DecodeError::OutOfRange),
        }
    }
}

/// Most fields in one entity's block.
pub const MAX_FIELDS: usize = 16;

/// What a snapshot carries about an entity besides its kinematics: kind, model and block fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Info {
    /// What the entity is.
    pub kind: EntityKind,
    /// Its model.
    pub model: AssetId,
    len: u8,
    fields: [u32; MAX_FIELDS],
}

impl Default for Info {
    fn default() -> Self {
        Self::PLAYER
    }
}

impl Info {
    /// A player with no model and no fields: what servers send before the ped block exists.
    pub const PLAYER: Self = Self::new(EntityKind::Player, AssetId::NONE);

    /// An entity with no block fields.
    #[must_use]
    pub const fn new(kind: EntityKind, model: AssetId) -> Self {
        Self {
            kind,
            model,
            len: 0,
            fields: [0; MAX_FIELDS],
        }
    }

    /// An entity with these block fields. Fails with [`EncodeError::OutOfRange`] for more than [`MAX_FIELDS`].
    pub fn with_fields(kind: EntityKind, model: AssetId, fields: &[u32]) -> Result<Self, EncodeError> {
        let mut info = Self::new(kind, model);
        info.fields
            .get_mut(..fields.len())
            .ok_or(EncodeError::OutOfRange)?
            .copy_from_slice(fields);
        info.len = fields.len() as u8;
        Ok(info)
    }

    /// The block fields. Their number is fixed when the entity is added.
    #[must_use]
    pub fn fields(&self) -> &[u32] {
        &self.fields[..usize::from(self.len)]
    }

    /// Sets field `index`. Fails with [`EncodeError::OutOfRange`] past the entity's fields.
    pub fn set_field(&mut self, index: usize, value: u32) -> Result<(), EncodeError> {
        *self.fields[..usize::from(self.len)]
            .get_mut(index)
            .ok_or(EncodeError::OutOfRange)? = value;
        Ok(())
    }

    /// The mask of fields that differ from `old`, bit `i` for field `i`; `None` if the two have different numbers
    /// of fields, which no block change can express.
    #[must_use]
    pub fn changed_from(&self, old: &Self) -> Option<u16> {
        if self.len != old.len {
            return None;
        }
        let mut mask = 0u16;
        for (i, (a, b)) in old.fields().iter().zip(self.fields()).enumerate() {
            if a != b {
                mask |= 1 << i;
            }
        }
        Some(mask)
    }
}
