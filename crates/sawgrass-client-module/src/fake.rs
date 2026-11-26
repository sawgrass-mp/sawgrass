//! A runtime with a made-up code image, for tests and for the module's code above the [`Runtime`] boundary.

use crate::runtime::{CodeImage, GameBuild, HookHandle, HookRequest, Runtime, RuntimeError, RuntimeKind};

#[derive(Debug)]
pub struct FakeRuntime {
    pub build: GameBuild,
    pub base: u64,
    pub image: Vec<u8>,
    /// Hooks currently installed, in the order they were installed.
    pub hooks: Vec<(HookHandle, HookRequest)>,
    /// Function names whose hooks the fake refuses, to test failure paths.
    pub refuse: Vec<String>,
    /// Function names whose hooks the fake refuses to remove.
    pub refuse_removal: Vec<String>,
    /// Whether the fake calls the module's frame itself, as VIctory does.
    pub drives_frames: bool,
    next_handle: u64,
}

impl FakeRuntime {
    pub fn new(build: GameBuild, base: u64, image: Vec<u8>) -> Self {
        Self {
            build,
            base,
            image,
            hooks: Vec::new(),
            refuse_removal: Vec::new(),
            refuse: Vec::new(),
            drives_frames: false,
            next_handle: 1,
        }
    }
}

impl Runtime for FakeRuntime {
    fn kind(&self) -> RuntimeKind {
        self.build.runtime
    }

    fn build(&self) -> Result<GameBuild, RuntimeError> {
        Ok(self.build.clone())
    }

    fn code_image(&self) -> Result<CodeImage<'_>, RuntimeError> {
        Ok(CodeImage {
            base: self.base,
            bytes: &self.image,
        })
    }

    fn install_hook(&mut self, request: HookRequest) -> Result<HookHandle, RuntimeError> {
        if self.refuse.contains(&request.function) {
            return Err(RuntimeError::HookRefused {
                function: request.function,
                reason: "refused by the fake".into(),
            });
        }
        let handle = HookHandle(self.next_handle);
        self.next_handle += 1;
        self.hooks.push((handle, request));
        Ok(handle)
    }

    fn remove_hook(&mut self, handle: HookHandle) -> Result<(), RuntimeError> {
        let index = self
            .hooks
            .iter()
            .position(|(h, _)| *h == handle)
            .ok_or(RuntimeError::UnknownHook(handle))?;
        let function = &self.hooks[index].1.function;
        if self.refuse_removal.contains(function) {
            return Err(RuntimeError::HookRefused {
                function: function.clone(),
                reason: "kept by the fake".into(),
            });
        }
        self.hooks.remove(index);
        Ok(())
    }

    fn drives_frames(&self) -> bool {
        self.drives_frames
    }
}
