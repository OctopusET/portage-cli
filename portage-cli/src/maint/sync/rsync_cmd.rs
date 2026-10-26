//! rsync sync backend: shell out to `rsync` with Portage's `PORTAGE_RSYNC_OPTS`

use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use camino::Utf8Path;

use super::{Verify, info_line, warn_line};

/// Portage's built-in options when `PORTAGE_RSYNC_OPTS` is empty or unset
const DEFAULT_OPTS: &[&str] = &[
    "--recursive",
    "--links",
    "--safe-links",
    "--perms",
    "--times",
    "--omit-dir-times",
    "--compress",
    "--force",
    "--whole-file",
    "--delete",
    "--stats",
    "--human-readable",
    "--timeout=180",
    "--exclude=/distfiles",
    "--exclude=/local",
    "--exclude=/packages",
    "--exclude=/.git",
];

/// Added to a user's `PORTAGE_RSYNC_OPTS` when missing; the excludes keep `--delete` off local data
const REQUIRED_OPTS: &[&str] = &[
    "--recursive",
    "--times",
    "--exclude=/distfiles",
    "--exclude=/local",
    "--exclude=/packages",
];

/// rsync's options before source and destination: `PORTAGE_RSYNC_OPTS`, the verbosity, then
/// the repo's `sync-rsync-extra-opts` (or `PORTAGE_RSYNC_EXTRA_OPTS`)
pub(super) fn options(
    uri: &str,
    rsync_opts: &str,
    extra_opts: &str,
    quiet: bool,
    verbose: bool,
) -> Result<Vec<String>> {
    let mut opts = if rsync_opts.trim().is_empty() {
        DEFAULT_OPTS.iter().map(|o| o.to_string()).collect()
    } else {
        let mut opts = split(rsync_opts, "PORTAGE_RSYNC_OPTS")?;
        for &req in REQUIRED_OPTS {
            if !opts.iter().any(|o| o == req) {
                warn_line(&format!(
                    "adding required option {req} not included in PORTAGE_RSYNC_OPTS"
                ));
                opts.push(req.to_string());
            }
        }
        if uri
            .trim_end_matches('/')
            .ends_with(".gentoo.org/gentoo-portage")
        {
            if !opts.iter().any(|o| o.starts_with("--timeout=")) {
                opts.push("--timeout=180".to_string());
            }
            for req in ["--compress", "--whole-file"] {
                if !opts.iter().any(|o| o == req) {
                    warn_line(&format!(
                        "adding required option {req} not included in PORTAGE_RSYNC_OPTS"
                    ));
                    opts.push(req.to_string());
                }
            }
        }
        opts
    };
    opts.push(if quiet { "--quiet" } else { "--verbose" }.to_string());
    if verbose {
        opts.push("--progress".to_string());
    }
    opts.extend(split(extra_opts, "sync-rsync-extra-opts")?);
    Ok(opts)
}

fn split(value: &str, name: &str) -> Result<Vec<String>> {
    shlex::split(value).with_context(|| format!("unbalanced quoting in {name}: {value}"))
}

pub(super) fn pretend(path: &Utf8Path, uri: &str, opts: &[String]) -> String {
    format!("rsync {} {uri}/ → {path}/", opts.join(" "))
}

/// Returns whether the tree may have changed (rsync exit 0; we cannot cheaply
/// detect no-op without parsing itemize-changes, so `true` on success).
pub(super) fn sync(path: &Utf8Path, uri: &str, opts: &[String], quiet: bool) -> Result<bool> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("creating parent of {path}"))?;
    }
    std::fs::create_dir_all(path).with_context(|| format!("creating {path}"))?;

    // Trailing slashes: copy *contents* of uri into path (Portage style).
    let src = ensure_trailing_slash(uri);
    let dst = ensure_trailing_slash(path.as_str());

    if !quiet {
        info_line(&format!("rsync {} {src} → {dst}", opts.join(" ")));
    }

    let mut cmd = Command::new("rsync");
    cmd.args(opts);
    if quiet {
        cmd.stdout(Stdio::null()).stderr(Stdio::null());
    } else {
        cmd.stdout(Stdio::inherit()).stderr(Stdio::inherit());
    }
    cmd.arg(&src).arg(&dst);

    let status = cmd.status().context("spawning rsync")?;
    if !status.success() {
        bail!("rsync failed with {status}");
    }
    // Without --itemize-changes we cannot know no-op; treat success as maybe-changed.
    Ok(true)
}

/// Verify the tree against its OpenPGP-signed Manifests (GLEP 74) with gemato
pub(super) fn verify(path: &Utf8Path, verify: &Verify, quiet: bool) -> Result<()> {
    let mut cmd = Command::new("gemato");
    cmd.args(["verify", "--require-signed-manifest"]);
    if let Some(key) = &verify.key {
        if !quiet {
            info_line(&format!("Using keys from {key}"));
        }
        cmd.arg("--openpgp-key").arg(key);
    }
    if let Some(keyserver) = &verify.keyserver {
        cmd.arg("--keyserver").arg(keyserver);
    }
    if let Some(jobs) = verify.jobs {
        cmd.arg(format!("--jobs={jobs}"));
    }
    if quiet {
        cmd.arg("--quiet");
    } else {
        info_line(&format!("Verifying {path}"));
    }
    cmd.arg(path);
    match cmd.status() {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            bail!("unable to verify: gemato is required for sync-rsync-verify-metamanifest")
        }
        Err(e) => Err(e).context("spawning gemato"),
        Ok(st) if !st.success() => bail!("Manifest verification failed ({st})"),
        Ok(_) => Ok(()),
    }
}

fn ensure_trailing_slash(s: &str) -> String {
    if s.ends_with('/') {
        s.to_string()
    } else {
        format!("{s}/")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GENTOO: &str = "rsync://rsync.gentoo.org/gentoo-portage/";
    const OTHER: &str = "rsync://example.org/repo";

    #[test]
    fn empty_rsync_opts_use_defaults() {
        let opts = options(OTHER, "", "", false, false).unwrap();
        assert_eq!(opts[..DEFAULT_OPTS.len()], *DEFAULT_OPTS);
        assert_eq!(opts.last().unwrap(), "--verbose");
        assert!(opts.iter().any(|o| o == "--exclude=/.git"));
    }

    #[test]
    fn gentoo_mirror_forces_timeout_compress_whole_file() {
        let opts = options(GENTOO, "-a", "", true, false).unwrap();
        for req in ["--timeout=180", "--compress", "--whole-file"] {
            assert!(opts.iter().any(|o| o == req), "{req} missing: {opts:?}");
        }
        let opts = options(OTHER, "-a", "", true, false).unwrap();
        assert!(
            !opts
                .iter()
                .any(|o| o.starts_with("--timeout") || o == "--compress")
        );
    }

    #[test]
    fn gentoo_mirror_keeps_user_timeout() {
        let opts = options(GENTOO, "--timeout=30", "", true, false).unwrap();
        assert_eq!(
            opts.iter().filter(|o| o.starts_with("--timeout=")).count(),
            1
        );
    }

    #[test]
    fn user_rsync_opts_keep_required_excludes() {
        let opts = options(
            OTHER,
            "-a --delete",
            "--bwlimit=100 '--exclude=/a b'",
            true,
            false,
        )
        .unwrap();
        assert_eq!(
            opts,
            [
                "-a",
                "--delete",
                "--recursive",
                "--times",
                "--exclude=/distfiles",
                "--exclude=/local",
                "--exclude=/packages",
                "--quiet",
                "--bwlimit=100",
                "--exclude=/a b",
            ]
        );
    }

    #[test]
    fn present_required_opts_are_not_duplicated() {
        let opts = options(
            OTHER,
            "--recursive --times --exclude=/distfiles --exclude=/local --exclude=/packages",
            "",
            false,
            true,
        )
        .unwrap();
        assert_eq!(opts.len(), REQUIRED_OPTS.len() + 2);
    }

    #[test]
    fn unbalanced_quote_is_an_error() {
        assert!(options(OTHER, "--exclude='x", "", false, false).is_err());
    }
}
