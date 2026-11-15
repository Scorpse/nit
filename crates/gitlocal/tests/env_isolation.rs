// Lives in its own test binary (own process) because it mutates the process environment.
use gitlocal::{discover_clones, Git};
use std::path::Path;

fn init(path: &Path, url: &str) {
    std::fs::create_dir_all(path).unwrap();
    let git = Git::locate().unwrap();
    git.run(path, &["init", "-q"], None).unwrap();
    git.run(path, &["remote", "add", "origin", url], None)
        .unwrap();
}

#[test]
fn inherited_git_env_does_not_redirect_queries() {
    let root = tempfile::tempdir().unwrap();
    let a = root.path().join("a");
    let decoy = root.path().join("decoy");
    init(&a, "https://bitbucket.org/ws/real");
    init(&decoy, "https://bitbucket.org/ws/decoy");
    std::env::set_var("GIT_DIR", decoy.join(".git"));
    std::env::set_var("GIT_WORK_TREE", &decoy);
    std::env::set_var("GIT_CONFIG_COUNT", "1");
    std::env::set_var("GIT_CONFIG_KEY_0", "remote.evil.url");
    std::env::set_var("GIT_CONFIG_VALUE_0", "https://bitbucket.org/ws/evil");
    let git = Git::locate().unwrap();
    let scan = root.path().join("scan");
    std::fs::create_dir_all(&scan).unwrap();
    std::fs::rename(&a, scan.join("a")).unwrap();
    let (clones, skipped) = discover_clones(&git, &scan, 3);
    assert!(skipped.is_empty(), "{skipped:?}");
    assert_eq!(clones.len(), 1, "{clones:?}");
    assert_eq!(clones[0].remote_key, "bitbucket.org/ws/real");
}
