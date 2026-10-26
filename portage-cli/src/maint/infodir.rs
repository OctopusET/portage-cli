//! GNU info `dir` index regeneration after a merge, as emerge does at the end of a run
//!
//! An info directory is reindexed when its mtime differs from the one recorded under `info` in
//! `${EROOT}/var/cache/edb/mtimedb`, the file emerge keeps the same record in.

use std::os::unix::fs::MetadataExt;

use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};

/// Compressed forms of the index that a regeneration replaces
const DIR_SUFFIXES: &[&str] = &["", ".gz", ".bz2", ".xz", ".lz", ".lz4", ".zst", ".lzma"];

/// Reindex every changed directory of `INFOPATH`/`INFODIR` (from `root`'s env.d) under `root`
pub fn update_info_dirs(root: &Utf8Path) -> Result<()> {
    let env = super::env::merged_env_d(root);
    let dirs: Vec<&str> = ["INFOPATH", "INFODIR"]
        .iter()
        .filter_map(|k| env.get(*k))
        .flat_map(|v| v.split(':'))
        .filter(|d| !d.is_empty())
        .collect();
    let have_install_info = || {
        std::process::Command::new("install-info")
            .arg("--version")
            .output()
            .is_ok()
    };
    if dirs.is_empty() || !have_install_info() {
        return Ok(());
    }

    let mtimedb_path = root.join("var/cache/edb/mtimedb");
    let mut mtimedb: serde_json::Map<String, serde_json::Value> =
        std::fs::read_to_string(&mtimedb_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
    let before = mtimedb.clone();
    let info = mtimedb
        .entry("info")
        .or_insert_with(|| serde_json::Value::Object(Default::default()));
    let Some(info) = info.as_object_mut() else {
        return Ok(());
    };

    let mut errors = Vec::new();
    for dir in dirs {
        let full = root.join(dir.trim_start_matches('/'));
        let Ok(meta) = std::fs::metadata(&full) else {
            continue;
        };
        if !meta.is_dir() || has_keep_marker(&full) {
            continue;
        }
        if info.get(full.as_str()).and_then(|v| v.as_i64()) == Some(meta.mtime()) {
            continue;
        }
        errors.extend(reindex(&full)?);
        let mtime = std::fs::metadata(&full)?.mtime();
        info.insert(full.into_string(), mtime.into());
    }
    for e in &errors {
        crate::style::warn_line!("install-info: {e}");
    }

    if mtimedb != before {
        let json = serde_json::to_string_pretty(&mtimedb)?;
        crate::util::write_atomic(&mtimedb_path, json)?;
        let perms = std::os::unix::fs::PermissionsExt::from_mode(0o644);
        let _ = std::fs::set_permissions(&mtimedb_path, perms);
    }
    Ok(())
}

fn has_keep_marker(dir: &Utf8Path) -> bool {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .any(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|n| n.starts_with(".keepinfodir"))
        })
}

fn is_index_name(name: &str) -> bool {
    DIR_SUFFIXES.iter().any(|s| {
        name.strip_prefix("dir")
            .and_then(|r| r.strip_prefix(s))
            .is_some_and(|r| r.is_empty() || r == ".old")
    })
}

/// Rebuild `dir/dir` from every info page in `dir`; returns install-info's real errors
fn reindex(dir: &Utf8Path) -> Result<Vec<String>> {
    let mut pages: Vec<Utf8PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {dir}"))?
        .flatten()
        .filter_map(|e| Utf8PathBuf::from_path_buf(e.path()).ok())
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| !n.starts_with('.') && !is_index_name(n))
                && !p.is_dir()
        })
        .collect();
    pages.sort();
    if pages.is_empty() {
        return Ok(Vec::new());
    }

    let index = dir.join("dir");
    let old = |s: &str| Utf8PathBuf::from(format!("{index}{s}.old"));
    for s in DIR_SUFFIXES {
        let _ = std::fs::rename(format!("{index}{s}"), old(s));
    }
    let mut errors = Vec::new();
    for page in &pages {
        let out = std::process::Command::new("install-info")
            .arg(format!("--dir-file={index}"))
            .arg(page.as_std_path())
            .env("LANG", "C")
            .env("LANGUAGE", "C")
            .output()
            .context("running install-info")?;
        let text = String::from_utf8_lossy(&out.stdout).into_owned()
            + &String::from_utf8_lossy(&out.stderr);
        let text = text.trim();
        // Pages without a dir entry, or already listed, are not errors.
        if !text.is_empty()
            && !text.contains("already exists, for file `")
            && !text.starts_with("install-info: warning: no info dir entry in ")
        {
            errors.push(text.to_owned());
        }
    }
    let restore = !index.exists();
    for s in DIR_SUFFIXES {
        if restore {
            let _ = std::fs::rename(old(s), format!("{index}{s}"));
        }
        let _ = std::fs::remove_file(old(s));
    }
    Ok(errors)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_names_cover_every_compressed_and_old_form() {
        for n in ["dir", "dir.gz", "dir.bz2.old", "dir.old", "dir.zst"] {
            assert!(is_index_name(n), "{n}");
        }
        for n in ["dirent.info", "dir.info", "bash.info.bz2", "dir.gz.bak"] {
            assert!(!is_index_name(n), "{n}");
        }
    }
}
