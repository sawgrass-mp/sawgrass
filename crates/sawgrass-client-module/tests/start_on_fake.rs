//! The start-up path a real runtime will take, on the fake: read the build, resolve the table, install hooks.

use sawgrass_client_module::fake::FakeRuntime;
use sawgrass_client_module::runtime::{GameBuild, HookRequest, Runtime, RuntimeKind};
use sawgrass_client_module::table::{FunctionTable, ResolveError};

fn build(version: &str) -> GameBuild {
    GameBuild {
        runtime: RuntimeKind::Victory,
        title_version: version.into(),
        image_digest: "00ff".into(),
    }
}

fn runtime(version: &str) -> FakeRuntime {
    // Made-up bytes; the two "functions" sit at 0x08 and 0x18.
    let mut image = vec![0x90u8; 0x40];
    image[0x08..0x0B].copy_from_slice(&[0xC1, 0xC2, 0xC3]);
    image[0x18..0x1B].copy_from_slice(&[0xD1, 0xD2, 0xD3]);
    FakeRuntime::new(build(version), 0x7000_0000, image)
}

const TABLE: &str = "build = victory/1.0.0/00ff\nframe = C1 C2 C3\nped_create = D1 ?? D3\n";

#[test]
fn resolves_and_hooks_on_the_build_the_table_was_written_for() {
    let mut rt = runtime("1.0.0");
    let table = FunctionTable::parse(TABLE).expect("valid table");
    let required = ["frame", "ped_create"];
    let resolved = {
        let build = rt.build().expect("build");
        let image = rt.code_image().expect("image");
        table.resolve(&build, image, &required).expect("resolves")
    };
    for name in required {
        let address = resolved.address(name).expect("resolved");
        rt.install_hook(HookRequest {
            function: name.into(),
            address,
        })
        .expect("hooked");
    }
    let hooked: Vec<_> = rt
        .hooks
        .iter()
        .map(|(_, r)| (r.function.as_str(), r.address))
        .collect();
    assert_eq!(hooked, vec![("frame", 0x7000_0008), ("ped_create", 0x7000_0018)]);
    let (first, _) = rt.hooks[0].clone();
    rt.remove_hook(first).expect("removed");
    assert!(rt.remove_hook(first).is_err());
    assert_eq!(rt.hooks.len(), 1);
}

#[test]
fn a_game_patch_stops_start_up_until_a_new_table_exists() {
    let rt = runtime("1.0.1");
    let table = FunctionTable::parse(TABLE).expect("valid table");
    let build = rt.build().expect("build");
    let result = table.resolve(&build, rt.code_image().expect("image"), &["frame"]);
    assert!(matches!(result, Err(ResolveError::WrongBuild { .. })));
}
