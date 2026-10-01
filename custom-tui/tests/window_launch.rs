//! Manual OS integration test: intentionally opens one macOS Terminal window.
#[test]
#[ignore = "opens a desktop Terminal window; run explicitly"]
fn opens_the_same_agent_in_a_new_terminal() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap();
    custom_tui::window::open_agent_with_demo(
        std::path::Path::new(env!("CARGO_BIN_EXE_custom-tui")),
        root,
        "",
        "demo-agent-a",
        true,
    )
    .unwrap();
}
