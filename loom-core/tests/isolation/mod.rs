//! Linked into every Core test binary: model policy (cost ceiling, connections) comes
//! from a private directory, never the user's own `~/.loom`, so a changed personal
//! setting cannot change test results.
#[ctor::ctor]
fn isolate_model_state() {
    let dir = std::env::temp_dir().join(format!("loom-test-model-state-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    // SAFETY: runs before main, while the test binary is still single-threaded.
    unsafe { std::env::set_var("LOOM_MODEL_STATE_DIR", dir) };
}
