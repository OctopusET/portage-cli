//! `PORTAGE_REPOSITORIES` mutates the process environment, so it gets a test binary of its own.

use camino::{Utf8Path, Utf8PathBuf};
use portage_resolve::Roots;

fn write(path: &Utf8Path, body: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

fn names(roots: &Roots) -> Vec<String> {
    let conf = roots.repos_conf().unwrap();
    let mut names: Vec<String> = conf.repos().iter().map(|r| r.name.to_string()).collect();
    names.sort();
    names
}

#[test]
fn portage_repositories_replaces_repos_conf_and_portdir_overlay() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = Utf8PathBuf::from_path_buf(dir.path().to_owned()).unwrap();
    let overlay = cfg.join("overlay");
    write(&overlay.join("profiles/repo_name"), "legacy\n");
    write(
        &cfg.join("etc/portage/repos.conf"),
        "[user]\nlocation = /var/db/repos/user\n",
    );
    write(
        &cfg.join("etc/portage/make.conf"),
        &format!("PORTDIR_OVERLAY=\"{overlay}\"\n"),
    );
    let roots = Roots::default().with_config(Some(cfg.clone()));
    let without = names(&roots);
    assert!(
        ["legacy", "user"]
            .iter()
            .all(|n| without.iter().any(|w| w == n))
    );

    // SAFETY: this binary's only test, so no other thread reads the environment.
    unsafe { std::env::set_var("PORTAGE_REPOSITORIES", "") };
    assert!(names(&roots).is_empty());

    unsafe { std::env::set_var("PORTAGE_REPOSITORIES", "[inline]\nlocation = /x\n") };
    assert_eq!(names(&roots), ["inline"]);
}
