//! `em sync` / `em maint sync` — update ebuild repositories from `repos.conf`
//!
//! ## Backends
//!
//! | `sync-type` | Default backend | Optional |
//! |-------------|-----------------|----------|
//! | `git` | shell `git` ([`git_cmd`]) | pure gix via feature **`sync-gix`** |
//! | `rsync` | shell `rsync` ([`rsync_cmd`]) | — |
//! | other | skipped with a message | — |
//!
//! gix is **not** the default: it pulls a large dep graph (build time) and we
//! have not yet proven it faster than `/usr/bin/git` for Portage sync. Enable
//! with `--features sync-gix` to dogfood the pure-Rust path.
//!
//! Selection (Portage / emaint):
//! - named repos: those names, regardless of `auto-sync`
//! - no names: every repo with `auto-sync = yes|true` (default yes) that is
//!   syncable (`sync-type` + `sync-uri` + on-disk `location`)

#[cfg(not(feature = "sync-gix"))]
mod git_cmd;
#[cfg(feature = "sync-gix")]
mod git_gix;
mod rsync_cmd;

use std::collections::HashSet;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, bail};
use camino::{Utf8Path, Utf8PathBuf};
use portage_repo::{RepoEntry, ReposConf};

use crate::cli::Cli;
use crate::settings::Key;
use crate::style::{C_BOLD, C_ERROR, C_WARN};

/// Run repository sync for `em sync` and `em maint sync`
pub async fn run(repos: &[String], globals: &Cli) -> Result<()> {
    let conf = globals.roots().repos_conf().context("reading repos.conf")?;
    let selected = select_repos(&conf, repos)?;
    if selected.is_empty() {
        if globals.quiet() {
            return Ok(());
        }
        info_line("Nothing to sync.");
        return Ok(());
    }

    let roots = globals.roots();
    let config_root = roots.config().unwrap_or(Utf8Path::new("/"));
    let portage_dir = config_root.join("etc/portage");
    let configroot_env = format!("{}/", config_root.as_str().trim_end_matches('/'));
    let repo_hooks = postsync_hooks(&portage_dir.join("repo.postsync.d"));
    let mut hooked: HashSet<&str> = HashSet::new();
    let mut failed = 0usize;
    for entry in &selected {
        let changed = match sync_one(entry, globals).await {
            Ok(SyncOutcome::Skipped(why)) => {
                if !globals.quiet() {
                    info_repo(&entry.name, &format!("Skipping — {why}"));
                }
                continue;
            }
            Ok(SyncOutcome::Pretend(msg)) => {
                info_repo(&entry.name, &format!("Would sync — {msg}"));
                continue;
            }
            Ok(SyncOutcome::Synced { changed }) => {
                if !globals.quiet() {
                    if changed {
                        info_repo(&entry.name, "Synced (updated)");
                    } else {
                        info_repo(&entry.name, "Synced (already up to date)");
                    }
                }
                changed
            }
            Err(e) => {
                error_repo(&entry.name, &format!("{e:#}"));
                failed += 1;
                false
            }
        };
        if !runs_repo_hooks(entry, changed, &hooked) {
            continue;
        }
        hooked.insert(entry.name.as_str());
        let uri = entry.sync_uri.as_deref().unwrap_or("").trim();
        let location = entry.location.as_path().map(|p| p.as_os_str());
        for hook in &repo_hooks {
            let mut cmd = std::process::Command::new(hook);
            cmd.arg(entry.name.as_str())
                .arg(uri)
                .arg(location.unwrap_or_default())
                .env("PORTAGE_CONFIGROOT", &configroot_env);
            run_hook(hook, cmd);
        }
    }

    if !hooked.is_empty() {
        for hook in postsync_hooks(&portage_dir.join("postsync.d")) {
            let mut cmd = std::process::Command::new(&hook);
            cmd.env("PORTAGE_CONFIGROOT", &configroot_env);
            run_hook(&hook, cmd);
        }
    }

    if failed > 0 {
        bail!("{failed} of {} repo(s) failed to sync", selected.len());
    }
    Ok(())
}

/// Whether `entry`'s repo.postsync.d hooks run after a sync attempt, failed ones included
///
/// `sync-hooks-only-on-change` limits them to a changed repo, or one whose master's hooks ran.
fn runs_repo_hooks(entry: &RepoEntry, changed: bool, hooked: &HashSet<&str>) -> bool {
    let only_on_change = entry
        .sync_options
        .get("sync-hooks-only-on-change")
        .is_some_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "yes" | "true"));
    !only_on_change
        || changed
        || entry
            .masters
            .iter()
            .flatten()
            .any(|m| hooked.contains(m.as_str()))
}

/// The executable files under a `postsync.d`-style directory, in Portage's walk order
fn postsync_hooks(dir: &Utf8Path) -> Vec<Utf8PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let files = portage_repo::ini::collect_conf_files(dir.as_std_path()).unwrap_or_else(|e| {
        warn_line(&format!("reading {dir}: {e}"));
        Vec::new()
    });
    files
        .into_iter()
        .filter_map(|f| Utf8PathBuf::try_from(f).ok())
        .filter(|f| {
            let exec = std::fs::metadata(f).is_ok_and(|m| m.permissions().mode() & 0o111 != 0);
            if !exec {
                tracing::debug!("{dir} hook: '{f}' is not executable");
            }
            exec
        })
        .collect()
}

/// A failed hook is reported but does not fail the sync
fn run_hook(hook: &Utf8Path, mut cmd: std::process::Command) {
    tracing::debug!("Spawning post_sync hook: {hook}");
    match cmd.status() {
        Ok(st) if st.success() => {}
        Ok(st) => warn_line(&format!("post_sync hook {hook} failed ({st})")),
        Err(e) => warn_line(&format!("Spawn failed for: {hook}: {e}")),
    }
}

fn select_repos<'a>(conf: &'a ReposConf, names: &[String]) -> Result<Vec<&'a RepoEntry>> {
    if names.is_empty() {
        return Ok(conf
            .repos()
            .iter()
            .filter(|e| e.auto_sync && e.is_syncable())
            .collect());
    }

    let mut out = Vec::with_capacity(names.len());
    let mut missing = Vec::new();
    for name in names {
        match conf.find(name) {
            Some(e) => out.push(e),
            None => missing.push(name.clone()),
        }
    }
    if !missing.is_empty() {
        bail!(
            "unknown repo(s): {} (not in repos.conf)",
            missing.join(", ")
        );
    }
    Ok(out)
}

#[derive(Debug)]
enum SyncOutcome {
    Synced { changed: bool },
    Pretend(String),
    Skipped(String),
}

#[derive(Debug, Clone, Copy)]
enum SyncKind {
    Git,
    Rsync,
}

async fn sync_one(entry: &RepoEntry, globals: &Cli) -> Result<SyncOutcome> {
    let Some(path) = entry.location.as_path() else {
        return Ok(SyncOutcome::Skipped("virtual/alias repo".into()));
    };
    let path = Utf8PathBuf::try_from(path.to_path_buf())
        .map_err(|_| anyhow::anyhow!("repo path is not valid UTF-8: {}", path.display()))?;

    let sync_type = entry
        .sync_type
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty());
    let sync_uri = entry
        .sync_uri
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty());

    let (Some(sync_type), Some(sync_uri)) = (sync_type, sync_uri) else {
        if entry.sync_type.is_none() && entry.sync_uri.is_none() {
            return Ok(SyncOutcome::Skipped(
                "no sync-type/sync-uri in repos.conf".into(),
            ));
        }
        bail!("missing sync-type or sync-uri in repos.conf");
    };

    let kind = match sync_type.to_ascii_lowercase().as_str() {
        "git" => SyncKind::Git,
        "rsync" => SyncKind::Rsync,
        other => {
            return Ok(SyncOutcome::Skipped(format!(
                "sync-type={other} not supported (git, rsync)"
            )));
        }
    };

    let volatile = resolve_volatile(entry, path.as_std_path());
    let rsync_opts = match kind {
        SyncKind::Git => Vec::new(),
        SyncKind::Rsync => rsync_options(entry, globals).await?,
    };

    if globals.pretend() {
        let action = match kind {
            SyncKind::Git => git_pretend(&path, sync_uri, volatile),
            SyncKind::Rsync => rsync_cmd::pretend(&path, sync_uri, &rsync_opts),
        };
        return Ok(SyncOutcome::Pretend(action));
    }

    if !globals.quiet() {
        let mut out = anstream::stdout();
        let _ = writeln!(
            out,
            ">>> Syncing repository '{C_BOLD}{}{C_BOLD:#}' into '{path}'...",
            entry.name
        );
        if globals.verbose() > 0 {
            let _ = writeln!(
                out,
                ">>> sync-type={sync_type} sync-uri={sync_uri} backend={}",
                backend_label(kind)
            );
        }
        let _ = out.flush();
    }

    let quiet = globals.quiet();
    let path_c = path.clone();
    let uri = sync_uri.to_string();
    let verify = Verify::requested(entry, kind);
    let changed = tokio::task::spawn_blocking(move || match kind {
        SyncKind::Git => git_sync(&path_c, &uri, volatile, quiet, verify.as_ref()),
        SyncKind::Rsync => {
            let changed = rsync_cmd::sync(&path_c, &uri, &rsync_opts, quiet)?;
            if let Some(verify) = &verify {
                rsync_cmd::verify(&path_c, verify, quiet)?;
            }
            Ok(changed)
        }
    })
    .await
    .context("sync task join")??;

    Ok(SyncOutcome::Synced { changed })
}

/// OpenPGP verification of a synced tree, from the repo's `sync-openpgp-*` keys
pub(super) struct Verify {
    /// `sync-openpgp-key-path`: the only keys trusted; the user's own keyring when unset
    pub key: Option<Utf8PathBuf>,
    /// `sync-openpgp-keyserver`, for refreshing `key`
    pub keyserver: Option<String>,
    /// `sync-rsync-verify-jobs`; `None` lets the verifier pick
    pub jobs: Option<u32>,
}

impl Verify {
    /// Some when the repo turns on `sync-rsync-verify-metamanifest` or
    /// `sync-git-verify-commit-signature` for its sync-type
    fn requested(entry: &RepoEntry, kind: SyncKind) -> Option<Self> {
        let switch = match kind {
            SyncKind::Git => "sync-git-verify-commit-signature",
            SyncKind::Rsync => "sync-rsync-verify-metamanifest",
        };
        let opt = |name| {
            entry
                .sync_options
                .get(name)
                .map(|v| v.trim())
                .filter(|v| !v.is_empty())
        };
        opt(switch)
            .is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "yes" | "true"))
            .then(|| Self {
                key: opt("sync-openpgp-key-path").map(Utf8PathBuf::from),
                keyserver: opt("sync-openpgp-keyserver").map(String::from),
                jobs: opt("sync-rsync-verify-jobs").and_then(|v| match v.parse() {
                    Ok(0) => None,
                    Ok(n) => Some(n),
                    Err(_) => {
                        warn_line("sync-rsync-verify-jobs not a positive integer");
                        None
                    }
                }),
            })
    }
}

async fn rsync_options(entry: &RepoEntry, globals: &Cli) -> Result<Vec<String>> {
    // A first sync has no main repo to evaluate make.conf in; Portage's defaults apply then.
    let settings = match crate::crossdev::main_repo(globals) {
        Err(e) => {
            tracing::debug!("no main repo for rsync options: {e:#}");
            None
        }
        Ok(repo) => crate::settings::cached(Key::of(&globals.roots()), &repo)
            .await
            .unwrap_or_else(|e| {
                warn_line(&format!("ignoring make.conf rsync options: {e:#}"));
                None
            }),
    };
    let var = |name| settings.as_ref().and_then(|s| s.get(name)).unwrap_or("");
    let extra = entry
        .sync_options
        .get("sync-rsync-extra-opts")
        .map_or_else(|| var("PORTAGE_RSYNC_EXTRA_OPTS"), String::as_str);
    rsync_cmd::options(
        entry.sync_uri.as_deref().unwrap_or("").trim(),
        var("PORTAGE_RSYNC_OPTS"),
        extra,
        globals.quiet(),
        globals.verbose() > 0,
    )
}

fn backend_label(kind: SyncKind) -> &'static str {
    match kind {
        SyncKind::Git => {
            #[cfg(feature = "sync-gix")]
            {
                "gix (pure Rust)"
            }
            #[cfg(not(feature = "sync-gix"))]
            {
                "git (command)"
            }
        }
        SyncKind::Rsync => "rsync (command)",
    }
}

fn git_pretend(path: &Utf8Path, uri: &str, volatile: bool) -> String {
    #[cfg(feature = "sync-gix")]
    {
        git_gix::GixBackend.pretend(path, uri, volatile)
    }
    #[cfg(not(feature = "sync-gix"))]
    {
        git_cmd::GitCmdBackend.pretend(path, uri, volatile)
    }
}

fn git_sync(
    path: &Utf8Path,
    uri: &str,
    volatile: bool,
    quiet: bool,
    verify: Option<&Verify>,
) -> Result<bool> {
    #[cfg(feature = "sync-gix")]
    {
        git_gix::GixBackend.sync(path, uri, volatile, quiet, verify)
    }
    #[cfg(not(feature = "sync-gix"))]
    {
        git_cmd::GitCmdBackend.sync(path, uri, volatile, quiet, verify)
    }
}

/// Portage `volatile`: explicit conf wins; else volatile if not root/portage-owned
/// `pub(crate)`: also reused by `em --info` to show the same `volatile:
/// True/False` real emerge infers from filesystem ownership when a repo
/// doesn't set it explicitly in `repos.conf`.
pub(crate) fn resolve_volatile(entry: &RepoEntry, path: &Path) -> bool {
    if let Some(v) = entry.volatile {
        return v;
    }
    match std::fs::metadata(path).ok().map(|m| {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            m.uid()
        }
        #[cfg(not(unix))]
        {
            let _ = m;
            0u32
        }
    }) {
        Some(0) => false,
        Some(uid) if is_portage_uid(uid) => false,
        None => false,
        Some(_) => true,
    }
}

/// Resolve the `portage` user's uid from the host's `/etc/passwd` by name
/// Falls back to the conventional Gentoo uid (250) when there is no entry
/// (e.g. a minimal chroot with no passwd db yet) — same fallback shape as
/// `portage-repo`'s `resolve_owner`.
fn portage_uid() -> Option<u32> {
    let content = std::fs::read_to_string("/etc/passwd").ok()?;
    parse_portage_uid(&content)
}

fn parse_portage_uid(passwd: &str) -> Option<u32> {
    portage_repo::userdb::id_in(passwd, "portage")
}

fn is_portage_uid(uid: u32) -> bool {
    portage_uid().unwrap_or(250) == uid
}

// ── colour helpers (shared by backends) ──────────────────────────────────

pub(super) fn info_line(msg: &str) {
    let mut out = anstream::stdout();
    let _ = writeln!(out, ">>> {msg}");
    let _ = out.flush();
}

fn info_repo(name: &str, msg: &str) {
    let mut out = anstream::stdout();
    let _ = writeln!(out, ">>> {msg}: '{C_BOLD}{name}{C_BOLD:#}'");
    let _ = out.flush();
}

pub(super) fn warn_line(msg: &str) {
    let mut out = anstream::stderr();
    let _ = writeln!(out, "{C_WARN}!!!{C_WARN:#} {msg}");
    let _ = out.flush();
}

fn error_repo(name: &str, msg: &str) {
    let mut out = anstream::stderr();
    let _ = writeln!(
        out,
        "{C_ERROR}!!!{C_ERROR:#} Failed to sync '{C_BOLD}{name}{C_BOLD:#}': {msg}"
    );
    let _ = out.flush();
}

/// Shared trait for git backends (command vs gix)
pub(super) trait GitBackend {
    fn pretend(&self, path: &Utf8Path, uri: &str, volatile: bool) -> String;
    /// Sync, then when `verify` is set reject an unsigned or badly signed tip
    fn sync(
        &self,
        path: &Utf8Path,
        uri: &str,
        volatile: bool,
        quiet: bool,
        verify: Option<&Verify>,
    ) -> Result<bool>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::Path;

    fn write(path: &Path, body: &str) {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).unwrap();
        }
        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
    }

    #[test]
    fn select_auto_sync_only_when_no_names() {
        let dir = tempfile::tempdir().unwrap();
        let conf = dir.path().join("repos.conf");
        write(
            &conf,
            r#"
[gentoo]
location = /var/db/repos/gentoo
sync-type = git
sync-uri = https://example/gentoo.git

[local]
location = /var/db/repos/local
auto-sync = no
sync-type = git
sync-uri = https://example/local.git

[nosync]
location = /var/db/repos/nosync
"#,
        );
        let rc = ReposConf::load_from(&[&conf]).unwrap();
        let selected = select_repos(&rc, &[]).unwrap();
        let names: Vec<_> = selected.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["gentoo"]);
    }

    #[test]
    fn verify_follows_the_sync_types_own_switch() {
        let dir = tempfile::tempdir().unwrap();
        let conf = dir.path().join("repos.conf");
        write(
            &conf,
            "[gentoo]\nlocation = /a\nsync-rsync-verify-metamanifest = yes\n\
             sync-openpgp-key-path = /k.asc\nsync-rsync-verify-jobs = 0\n",
        );
        let rc = ReposConf::load_from(&[&conf]).unwrap();
        let entry = &rc.repos()[0];
        assert!(Verify::requested(entry, SyncKind::Git).is_none());
        let v = Verify::requested(entry, SyncKind::Rsync).unwrap();
        assert_eq!(v.key.as_deref(), Some(Utf8Path::new("/k.asc")));
        assert_eq!(v.jobs, None);
    }

    #[test]
    fn select_named_ignores_auto_sync() {
        let dir = tempfile::tempdir().unwrap();
        let conf = dir.path().join("repos.conf");
        write(
            &conf,
            r#"
[local]
location = /var/db/repos/local
auto-sync = no
sync-type = git
sync-uri = https://example/local.git
"#,
        );
        let rc = ReposConf::load_from(&[&conf]).unwrap();
        let selected = select_repos(&rc, &["local".into()]).unwrap();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].name, "local");
    }

    #[test]
    fn portage_uid_resolves_by_name_not_hardcoded_id() {
        assert_eq!(
            parse_portage_uid(
                "root:x:0:0:root:/root:/bin/bash\nportage:x:999:999:portage:/var/tmp/portage:/sbin/nologin\n"
            ),
            Some(999)
        );
    }

    #[test]
    fn portage_uid_is_none_without_a_matching_entry() {
        assert_eq!(parse_portage_uid("root:x:0:0:root:/root:/bin/bash\n"), None);
    }

    #[test]
    fn repo_hooks_honour_sync_hooks_only_on_change() {
        let dir = tempfile::tempdir().unwrap();
        let conf = dir.path().join("repos.conf");
        write(
            &conf,
            "[gentoo]\nlocation = /a\n\n[ov]\nlocation = /b\nmasters = gentoo\n\
             sync-hooks-only-on-change = yes\n",
        );
        let rc = ReposConf::load_from(&[&conf]).unwrap();
        let (gentoo, ov) = (rc.find("gentoo").unwrap(), rc.find("ov").unwrap());
        let none = HashSet::new();
        assert!(runs_repo_hooks(gentoo, false, &none));
        assert!(!runs_repo_hooks(ov, false, &none));
        assert!(runs_repo_hooks(ov, true, &none));
        assert!(runs_repo_hooks(ov, false, &HashSet::from(["gentoo"])));
    }

    #[test]
    fn postsync_hooks_are_the_executable_entries_in_order() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(dir.path()).unwrap();
        for (name, mode) in [
            ("20-b", 0o755),
            ("10-a", 0o755),
            ("15-data", 0o644),
            (".hidden", 0o755),
        ] {
            let p = root.join(name);
            write(p.as_std_path(), "#!/bin/sh\n");
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
        }
        let hooks = postsync_hooks(root);
        let names: Vec<_> = hooks.iter().map(|h| h.file_name().unwrap()).collect();
        assert_eq!(names, ["10-a", "20-b"]);
        assert!(postsync_hooks(&root.join("missing")).is_empty());
    }

    #[test]
    fn select_unknown_name_errors() {
        let dir = tempfile::tempdir().unwrap();
        let conf = dir.path().join("repos.conf");
        write(&conf, "[gentoo]\nlocation = /a\n");
        let rc = ReposConf::load_from(&[&conf]).unwrap();
        let err = select_repos(&rc, &["nope".into()]).unwrap_err();
        assert!(err.to_string().contains("unknown repo"));
    }
}
