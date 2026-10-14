use std::path::Path;
use std::process::Command;

use conductor_state::{Error, repo_id_for_root, resolve_repo};

fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.com",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn same_id_from_main_checkout_and_worktree() {
    let tmp = tempfile::tempdir().unwrap();
    let main = tmp.path().join("my-repo");
    std::fs::create_dir_all(main.join("src/deep")).unwrap();
    git(&main, &["init", "-q"]);
    git(&main, &["commit", "-q", "--allow-empty", "-m", "init"]);
    // A worktree inside the repo, the way worktrunk lays them out, and one outside it.
    git(
        &main,
        &["worktree", "add", "-q", ".worktrees/task", "-b", "task"],
    );
    let outside = tmp.path().join("elsewhere");
    git(
        &main,
        &[
            "worktree",
            "add",
            "-q",
            outside.to_str().unwrap(),
            "-b",
            "other",
        ],
    );

    let from_main = resolve_repo(&main).unwrap();
    assert_eq!(from_main.root, main.canonicalize().unwrap());
    assert_eq!(from_main.id, repo_id_for_root(&from_main.root));
    assert!(from_main.id.starts_with("my-repo-"), "{}", from_main.id);

    for dir in [main.join("src/deep"), main.join(".worktrees/task"), outside] {
        assert_eq!(resolve_repo(&dir).unwrap(), from_main, "{}", dir.display());
    }
}

#[test]
fn not_a_repo() {
    let tmp = tempfile::tempdir().unwrap();
    let err = resolve_repo(tmp.path()).unwrap_err();
    assert!(matches!(err, Error::Git { .. }), "{err}");
}
