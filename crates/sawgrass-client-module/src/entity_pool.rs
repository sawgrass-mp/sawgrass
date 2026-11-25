//! Which game entity stands for which server entity.

use std::collections::HashMap;

use crate::game::{EntityKind, GameHandle};

/// The server's id for an entity, as in the protocol's `EntityState::id`.
pub type NetId = u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindError {
    NetIdTaken(NetId),
    HandleTaken(GameHandle),
}

/// A one-to-one map between server ids and game handles.
#[derive(Debug, Default)]
pub struct EntityPool {
    by_net: HashMap<NetId, (GameHandle, EntityKind)>,
    by_handle: HashMap<GameHandle, NetId>,
}

impl EntityPool {
    pub fn bind(&mut self, net: NetId, handle: GameHandle, kind: EntityKind) -> Result<(), BindError> {
        if self.by_net.contains_key(&net) {
            return Err(BindError::NetIdTaken(net));
        }
        if self.by_handle.contains_key(&handle) {
            return Err(BindError::HandleTaken(handle));
        }
        self.by_net.insert(net, (handle, kind));
        self.by_handle.insert(handle, net);
        Ok(())
    }

    pub fn unbind_net(&mut self, net: NetId) -> Option<(GameHandle, EntityKind)> {
        let bound = self.by_net.remove(&net)?;
        self.by_handle.remove(&bound.0);
        Some(bound)
    }

    pub fn unbind_handle(&mut self, handle: GameHandle) -> Option<NetId> {
        let net = self.by_handle.remove(&handle)?;
        self.by_net.remove(&net);
        Some(net)
    }

    pub fn handle(&self, net: NetId) -> Option<GameHandle> {
        self.by_net.get(&net).map(|(handle, _)| *handle)
    }

    pub fn net_id(&self, handle: GameHandle) -> Option<NetId> {
        self.by_handle.get(&handle).copied()
    }

    pub fn len(&self) -> usize {
        self.by_net.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_net.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binds_one_to_one_and_unbinds_from_either_side() {
        let mut pool = EntityPool::default();
        pool.bind(7, GameHandle(70), EntityKind::Ped).expect("bound");
        pool.bind(8, GameHandle(80), EntityKind::Vehicle).expect("bound");
        assert_eq!(
            pool.bind(7, GameHandle(71), EntityKind::Ped),
            Err(BindError::NetIdTaken(7))
        );
        assert_eq!(
            pool.bind(9, GameHandle(80), EntityKind::Ped),
            Err(BindError::HandleTaken(GameHandle(80)))
        );
        assert_eq!(pool.handle(7), Some(GameHandle(70)));
        assert_eq!(pool.net_id(GameHandle(80)), Some(8));
        assert_eq!(pool.unbind_handle(GameHandle(70)), Some(7));
        assert_eq!(pool.handle(7), None);
        assert_eq!(pool.unbind_net(8), Some((GameHandle(80), EntityKind::Vehicle)));
        assert_eq!(pool.net_id(GameHandle(80)), None);
        assert!(pool.is_empty());
    }
}
