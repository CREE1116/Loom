use loom_core::instructions::{self, Instructions};
use std::fs;
#[test]
fn project_instructions_are_hierarchical_scoped_and_loaded_once() {
    let d = tempfile::tempdir().unwrap();
    fs::write(d.path().join("AGENTS.md"), "root rule").unwrap();
    fs::create_dir(d.path().join("src")).unwrap();
    fs::write(d.path().join("src/AGENTS.md"), "src rule").unwrap();
    let mut guidance = Instructions::open(d.path()).unwrap();
    assert!(guidance.root_guidance().unwrap().contains("root rule"));
    assert!(
        !guidance
            .for_path(std::path::Path::new("README.md"))
            .unwrap()
            .contains("src rule")
    );
    assert!(
        guidance
            .for_path(std::path::Path::new("src/lib.rs"))
            .unwrap()
            .contains("src rule")
    );
    assert!(
        guidance
            .for_path(std::path::Path::new("src/other.rs"))
            .unwrap()
            .is_empty()
    );
    assert!(guidance.for_path(std::path::Path::new("../other")).is_err());
}
#[test]
fn selected_and_named_skills_load_actual_instructions() {
    let d = tempfile::tempdir().unwrap();
    fs::create_dir_all(d.path().join(".agents/skills/testskill")).unwrap();
    fs::write(
        d.path().join(".agents/skills/testskill/SKILL.md"),
        "---\nname: testskill\ndescription: sample\n---\nrequired behavior\n",
    )
    .unwrap();
    let out = instructions::initial(d.path(), "use $testskill", &[]).unwrap();
    assert!(out.contains("required behavior"));
    let skills = instructions::discover(d.path()).unwrap();
    assert!(skills.iter().any(|s| s.name == "testskill"));
}
