//! The adapter for [VIctory](https://github.com/Marvisak/VIctory), the first runtime (ADR 0004).
//!
//! VIctory has no API for loading an outside module or registering hooks yet. ADR 0004 decides that we propose one
//! upstream (load a listed library after the title starts, let it register hooks by build identity) instead of
//! forking VIctory or writing an injector, so every method here reports [`RuntimeError::Unavailable`] until that API
//! exists. The module never loads itself, patches the runtime or works around any protection: it runs only when
//! VIctory loads it.

use crate::runtime::{CodeImage, GameBuild, HookHandle, HookRequest, Runtime, RuntimeError, RuntimeKind};

const NO_API: &str = "VIctory has no module-loading and hook API yet (proposed upstream, ADR 0004)";

/// The VIctory adapter. It cannot be created until VIctory offers the API above.
#[derive(Debug)]
pub struct VictoryRuntime {
    _private: (),
}

impl VictoryRuntime {
    /// Binds to the VIctory instance that loaded this module.
    ///
    /// TODO(upstream API): obtain the host interface VIctory passes to a loaded module, then read the build identity
    /// and the code image from it.
    pub fn connect() -> Result<Self, RuntimeError> {
        Err(RuntimeError::Unavailable(NO_API))
    }
}

impl Runtime for VictoryRuntime {
    fn kind(&self) -> RuntimeKind {
        RuntimeKind::Victory
    }

    fn build(&self) -> Result<GameBuild, RuntimeError> {
        Err(RuntimeError::Unavailable(NO_API))
    }

    fn code_image(&self) -> Result<CodeImage<'_>, RuntimeError> {
        Err(RuntimeError::Unavailable(NO_API))
    }

    fn install_hook(&mut self, _request: HookRequest) -> Result<HookHandle, RuntimeError> {
        Err(RuntimeError::Unavailable(NO_API))
    }

    fn remove_hook(&mut self, _handle: HookHandle) -> Result<(), RuntimeError> {
        Err(RuntimeError::Unavailable(NO_API))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_the_missing_api() {
        assert_eq!(
            VictoryRuntime::connect().err(),
            Some(RuntimeError::Unavailable(NO_API))
        );
    }
}
