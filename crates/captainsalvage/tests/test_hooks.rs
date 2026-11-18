//! The hooks that drive the window in tests exist only in a build made with the `test-hooks`
//! feature. These tests read this very test program, which links the engine built with the
//! same features, and look for the hooks' names in it. The names are kept scrambled here so
//! that this file never puts one into the program itself.

use captainsalvage::engine::{Engine, AUTHOR_CLAUDE};
use captainsalvage::mcp::tool_definitions;
use captainsalvage::session::Session;
use captainsalvage::source::MemorySource;
use captainsalvage::workspace::Workspace;
use fs_exfat::fixtures;
use serde_json::json;
use std::path::Path;
use std::sync::Arc;

/// Every name a test hook reads or answers to, each with 0x2a folded into its bytes.
const HOOK_NAMES: &[&[u8]] = &[
    // the environment variables the window tests set
    &[
        0x79, 0x6b, 0x66, 0x7c, 0x6b, 0x6d, 0x6f, 0x75, 0x7e, 0x6f, 0x79, 0x7e, 0x75, 0x7f, 0x63,
    ],
    &[
        0x79, 0x6b, 0x66, 0x7c, 0x6b, 0x6d, 0x6f, 0x75, 0x7e, 0x6f, 0x79, 0x7e, 0x75, 0x62, 0x63,
        0x6e, 0x6e, 0x6f, 0x64,
    ],
    &[
        0x79, 0x6b, 0x66, 0x7c, 0x6b, 0x6d, 0x6f, 0x75, 0x7e, 0x6f, 0x79, 0x7e, 0x75, 0x6e, 0x6b,
        0x78, 0x61,
    ],
    &[
        0x79, 0x6b, 0x66, 0x7c, 0x6b, 0x6d, 0x6f, 0x75, 0x7e, 0x6f, 0x79, 0x7e, 0x75, 0x69, 0x65,
        0x7a, 0x73, 0x75, 0x78, 0x65, 0x65, 0x7e,
    ],
    &[
        0x79, 0x6b, 0x66, 0x7c, 0x6b, 0x6d, 0x6f, 0x75, 0x7e, 0x6f, 0x79, 0x7e, 0x75, 0x79, 0x7e,
        0x6b, 0x78, 0x7e, 0x7f, 0x7a, 0x75, 0x6e, 0x63, 0x79, 0x61, 0x79,
    ],
    // the variable that names another agent command
    &[
        0x79, 0x6b, 0x66, 0x7c, 0x6b, 0x6d, 0x6f, 0x75, 0x6b, 0x6d, 0x6f, 0x64, 0x7e, 0x75,
    ],
    // the tool that runs script in the window, the event it sends, the tool that unplugs a disk
    &[0x5f, 0x43, 0x75, 0x4f, 0x5c, 0x4b, 0x46],
    &[0x5e, 0x4f, 0x59, 0x5e, 0x07, 0x4f, 0x5c, 0x4b, 0x46],
    &[
        0x5e, 0x4f, 0x59, 0x5e, 0x75, 0x4e, 0x43, 0x59, 0x49, 0x45, 0x44, 0x44, 0x4f, 0x49, 0x5e,
    ],
];

fn name(index: usize) -> String {
    HOOK_NAMES[index]
        .iter()
        .map(|byte| (byte ^ 0x2a) as char)
        .collect()
}

fn found_in_this_program(needle: &str) -> bool {
    let program = std::fs::read(std::env::current_exe().expect("this program")).expect("read it");
    program
        .windows(needle.len())
        .any(|window| window == needle.as_bytes())
}

fn engine_with_volume() -> (Arc<Engine>, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("salvage-hooks-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let source = Arc::new(MemorySource::new(fixtures::build_volume()));
    let session = Session::with_source(
        Path::new("/test/image"),
        source,
        Workspace::at(&dir).expect("workspace"),
    )
    .expect("session");
    let engine = Engine::new();
    engine.set_session(session);
    (engine, dir)
}

#[cfg(not(feature = "test-hooks"))]
#[test]
fn the_normal_build_holds_no_test_hook() {
    for index in 0..HOOK_NAMES.len() {
        let hook = name(index);
        assert!(
            !found_in_this_program(&hook),
            "{hook} is in a build without the test-hooks feature"
        );
    }
    let (engine, dir) = engine_with_volume();
    for tool in [name(6), name(8)] {
        let error = engine
            .call(&tool, &json!({"code": "1"}), AUTHOR_CLAUDE)
            .expect_err("a normal build has no such tool");
        assert!(error.to_string().contains("unknown tool"), "{error}");
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(feature = "test-hooks")]
#[test]
fn the_test_hooks_build_holds_the_hooks() {
    // Three are not in this program, though they are in the build: the window's own two
    // (hidden and dark) belong to the app shell, and the agent command is only linked in when
    // a chat is. `scripts/check-test-hooks.sh` finds all of them in the built app.
    for index in (0..HOOK_NAMES.len()).filter(|index| ![1, 2, 5].contains(index)) {
        let hook = name(index);
        assert!(
            found_in_this_program(&hook),
            "{hook} is missing from a build with the test-hooks feature"
        );
    }
}

#[test]
fn no_build_offers_a_test_hook_to_an_agent() {
    let advertised = serde_json::Value::Array(tool_definitions()).to_string();
    assert!(!advertised.contains(&name(6)));
    assert!(!advertised.contains(&name(8)));
}

#[cfg(feature = "test-hooks")]
#[test]
fn a_disconnect_is_refused_unless_the_test_environment_is_set() {
    if std::env::var_os(name(0)).is_some() {
        return;
    }
    let (engine, dir) = engine_with_volume();
    assert!(engine.call(&name(8), &json!({}), AUTHOR_CLAUDE).is_err());
    let _ = std::fs::remove_dir_all(dir);
}
