//! Regenerate the binhost `Packages` index from `PKGDIR`'s containers:
//! `em maint binhost` (and `em maint binpkg prune`, which deletes stale
//! containers first and then calls this to bring the index back in sync).
//!
//! Walks `PKGDIR` for `*.gpkg.tar`, reads each container's VDB-style metadata
//! via [`crate::read_metadata`], and writes the `Packages` index
//! portage/emerge consume over `PORTAGE_BINHOST` / `--getbinpkg`, in the shape
//! of portage's `PackageIndex.write`:
//!
//! - a **header** block: the caller's [`IndexConfig::header`] plus `VERSION`,
//!   `PACKAGES`, `TIMESTAMP` and `REPO_REVISIONS`, sorted, then
//! - one block per package (sorted by CPV, keys sorted by their untranslated
//!   name, so `MTIME` and `REPO` come last), blank line between entries.
//!
//! An entry leaves out what a reader fills in again: `SLOT: 0`, `EAPI: 0`, and
//! a `CHOST`/`REPO` equal to the header's. It also carries the build's
//! `CFLAGS`-family flags and `BUILD_ENV_KEY`, which em's reuse check reads and
//! portage's reader ignores.

use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use camino::Utf8Path;

use crate::error::Result;
use crate::scan::{ContainerFacts, checksum, find_gpkg_containers};

/// What [`index_pkgdir`] writes besides the entries
#[derive(Debug, Clone, Default)]
pub struct IndexConfig {
    /// Header fields (`CHOST`, `USE`, `PROFILE`, ...); the writer sets `VERSION`,
    /// `PACKAGES`, `TIMESTAMP` and `REPO_REVISIONS` itself
    pub header: BTreeMap<String, String>,
    /// `FEATURES=compress-index`: also write `Packages.gz`
    pub compress: bool,
}

/// Walk `pkgdir` for `*.gpkg.tar`, build per-package entries, and write the
/// `Packages` index (and `Packages.gz` with [`IndexConfig::compress`]).
///
/// Returns `(indexed, skipped)`.
pub fn index_pkgdir(pkgdir: &Utf8Path, config: &IndexConfig) -> Result<(usize, usize)> {
    let mut files: Vec<(String, PathBuf)> = Vec::new();
    find_gpkg_containers(pkgdir.as_std_path(), pkgdir.as_std_path(), &mut files)?;
    files.sort();

    let mut entries: Vec<(String, BTreeMap<String, String>)> = Vec::new();
    let mut skipped = 0usize;
    for (rel, full) in &files {
        match build_entry(rel, full) {
            Ok((cpv, fields)) => entries.push((cpv, fields)),
            Err(e) => {
                tracing::warn!("skipping {}: {e:#}", full.display());
                skipped += 1;
            }
        }
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let mut header = config.header.clone();
    header.insert("VERSION".to_string(), "0".to_string());
    match header_repo_revisions(&entries) {
        Some(revisions) => header.insert("REPO_REVISIONS".to_string(), revisions),
        None => header.remove("REPO_REVISIONS"),
    };

    write_index(pkgdir, header, &entries, config.compress)?;
    Ok((entries.len(), skipped))
}

/// Build one index entry from a container: metadata fields + file checksums
///
/// Returns `(cpv, fields)` where `fields` keys are already in their
/// index-translated form (`MTIME`, `REPO`).
fn build_entry(rel: &str, full: &Path) -> Result<(String, BTreeMap<String, String>)> {
    let meta = crate::read_metadata(full)?;
    let facts = ContainerFacts::from_metadata(&meta, rel)?;
    let cpv = facts.cpv;

    let (md5, sha1, size, mtime) = checksum(full)?;

    let mut f = BTreeMap::new();
    f.insert("CPV".to_string(), cpv.clone());
    f.insert("MD5".to_string(), md5);
    f.insert("SHA1".to_string(), sha1);
    f.insert("SIZE".to_string(), size.to_string());
    f.insert("MTIME".to_string(), mtime.to_string());
    f.insert("PATH".to_string(), rel.to_string());

    if let Some(bid) = facts.build_id {
        f.insert("BUILD_ID".to_string(), bid.to_string());
    }

    for key in [
        "BUILD_TIME",
        "SLOT",
        "EAPI",
        "USE",
        "IUSE",
        "KEYWORDS",
        "LICENSE",
        "RESTRICT",
        "PROPERTIES",
        "DEFINED_PHASES",
        "DEPEND",
        "RDEPEND",
        "BDEPEND",
        "PDEPEND",
        "IDEPEND",
        "CHOST",
        "PROVIDES",
        "REQUIRES",
        "REPO_REVISIONS",
        // em's reuse check reads these; portage's reader drops them.
        "CFLAGS",
        "CXXFLAGS",
        "LDFLAGS",
        "RUSTFLAGS",
        crate::index::BUILD_ENV_KEY_FIELD,
    ] {
        copy_field(&meta, &mut f, key);
    }
    if let Some(v) = meta.get("repository").filter(|s| !s.is_empty()) {
        f.insert("REPO".to_string(), v.clone());
    }

    Ok((cpv, f))
}

fn copy_field(meta: &BTreeMap<String, String>, out: &mut BTreeMap<String, String>, key: &str) {
    if let Some(v) = meta.get(key).filter(|s| !s.is_empty()) {
        out.insert(key.to_string(), v.clone());
    }
}

/// The header's `REPO_REVISIONS`: per repository, the revision of its newest build by
/// `BUILD_TIME`, as JSON with sorted keys
///
/// Portage takes only revisions found in its sync history; em keeps none.
fn header_repo_revisions(entries: &[(String, BTreeMap<String, String>)]) -> Option<String> {
    let mut newest: BTreeMap<String, (u64, String)> = BTreeMap::new();
    for (_, fields) in entries {
        let Some(revisions) = fields.get("REPO_REVISIONS") else {
            continue;
        };
        let Ok(revisions) = serde_json::from_str::<BTreeMap<String, String>>(revisions) else {
            continue;
        };
        let built: u64 = fields
            .get("BUILD_TIME")
            .and_then(|t| t.parse().ok())
            .unwrap_or(0);
        for (repo, revision) in revisions {
            match newest.get(&repo) {
                Some((time, _)) if *time > built => {}
                _ => {
                    newest.insert(repo, (built, revision));
                }
            }
        }
    }
    if newest.is_empty() {
        return None;
    }
    let quote = |s: &str| serde_json::to_string(s).unwrap_or_default();
    let pairs: Vec<String> = newest
        .iter()
        .map(|(repo, (_, revision))| format!("{}: {}", quote(repo), quote(revision)))
        .collect();
    Some(format!("{{{}}}", pairs.join(", ")))
}

/// Portage's key order: entries sort by the untranslated key name
fn sort_name(key: &str) -> &str {
    match key {
        "MTIME" => "_mtime_",
        "REPO" => "repository",
        key => key,
    }
}

/// Append `KEY: VALUE` lines, one per non-empty value
///
/// A value spanning lines (an ebuild's DEPEND) has to be folded onto one, since a blank line
/// ends a block.
fn push_block<'a>(text: &mut String, fields: impl Iterator<Item = (&'a str, &'a str)>) {
    for (k, v) in fields {
        let v = v.split_whitespace().collect::<Vec<_>>().join(" ");
        if v.is_empty() {
            continue;
        }
        text.push_str(k);
        text.push_str(": ");
        text.push_str(&v);
        text.push('\n');
    }
    text.push('\n');
}

/// Write the `Packages` index: header block, then one block per entry
fn write_index(
    pkgdir: &Utf8Path,
    mut header: BTreeMap<String, String>,
    entries: &[(String, BTreeMap<String, String>)],
    compress: bool,
) -> Result<()> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    header.insert("PACKAGES".to_string(), entries.len().to_string());
    header.insert("TIMESTAMP".to_string(), now.to_string());

    let mut text = String::new();
    push_block(
        &mut text,
        header.iter().map(|(k, v)| (k.as_str(), v.as_str())),
    );
    for (_cpv, fields) in entries {
        let mut keys: Vec<&str> = fields
            .iter()
            .filter(|&(k, v)| match k.as_str() {
                "SLOT" | "EAPI" => v != "0",
                "CHOST" | "REPO" => header.get(k) != Some(v),
                _ => true,
            })
            .map(|(k, _)| k.as_str())
            .collect();
        keys.sort_by_key(|k| sort_name(k));
        push_block(&mut text, keys.into_iter().map(|k| (k, fields[k].as_str())));
    }

    let mtime = UNIX_EPOCH + Duration::from_secs(now);
    let group = std::fs::metadata(pkgdir.as_std_path()).ok();
    let index = pkgdir.join("Packages");
    let gz = pkgdir.join("Packages.gz");
    write_atomic(index.as_std_path(), text.as_bytes(), mtime, group.as_ref())?;
    if compress {
        let mut encoder = flate2::GzBuilder::new()
            .mtime(now as u32)
            .write(Vec::new(), flate2::Compression::default());
        encoder.write_all(text.as_bytes())?;
        write_atomic(gz.as_std_path(), &encoder.finish()?, mtime, group.as_ref())?;
    } else {
        match std::fs::remove_file(gz.as_std_path()) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
    }
    Ok(())
}

/// Write `path` by renaming a finished sibling into place, so a reader never sees half a file
///
/// As Portage does, the file takes `TIMESTAMP` as its times and PKGDIR's group and group bits.
fn write_atomic(
    path: &Path,
    data: &[u8],
    mtime: SystemTime,
    pkgdir: Option<&std::fs::Metadata>,
) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let mut partial = path.as_os_str().to_owned();
    partial.push(format!(".part-{}", std::process::id()));
    let partial = PathBuf::from(partial);
    let file = std::fs::File::create(&partial)?;
    let written = (|| -> Result<()> {
        (&file).write_all(data)?;
        if let Some(dir) = pkgdir {
            let _ = std::os::unix::fs::fchown(&file, None, Some(dir.gid()));
            let mode = file.metadata()?.permissions().mode() | (dir.mode() & 0o060);
            file.set_permissions(std::fs::Permissions::from_mode(mode))?;
        }
        file.set_times(
            std::fs::FileTimes::new()
                .set_accessed(mtime)
                .set_modified(mtime),
        )?;
        std::fs::rename(&partial, path)?;
        Ok(())
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&partial);
    }
    written
}

/// An [`IndexConfig`] whose header holds only `CHOST`, for tests
#[cfg(test)]
pub(crate) fn chost_config(chost: &str) -> IndexConfig {
    IndexConfig {
        header: BTreeMap::from([("CHOST".to_string(), chost.to_string())]),
        compress: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::write_gpkg;

    // Seed a PKGDIR with a gpkg via the writer, then build the index and
    // verify the `Packages` file has the expected header + one sorted entry
    // with checksums and the translated REPO key.
    #[test]
    fn index_roundtrips_a_written_gpkg() {
        let tmp = tempfile::tempdir().unwrap();
        let pkgdir = camino::Utf8Path::from_path(tmp.path()).unwrap();

        let image = tmp.path().join("image");
        std::fs::create_dir_all(image.join("usr/bin")).unwrap();
        std::fs::write(image.join("usr/bin/hello"), b"hi\n").unwrap();

        let meta = tmp.path().join("vdb/foo-1.0");
        std::fs::create_dir_all(&meta).unwrap();
        for (k, v) in [
            ("PF", "foo-1.0"),
            ("CATEGORY", "app-test"),
            ("SLOT", "0"),
            ("EAPI", "8"),
            ("DESCRIPTION", "a test package"),
            ("repository", "gentoo"),
            ("BUILD_TIME", "1700000000"),
            ("SIZE", "3"),
            ("CFLAGS", "-O2 -pipe -mcpu=ampere1a"),
            ("CXXFLAGS", "-O2 -pipe -mcpu=ampere1a"),
            ("RUSTFLAGS", "-C target-cpu=neoverse-n1"),
            (
                "DEPEND",
                "\n\t\n\n\tvirtual/zlib:=\n\treadline? ( sys-libs/readline:= )",
            ),
        ] {
            std::fs::write(meta.join(k), format!("{v}\n")).unwrap();
        }

        let container = pkgdir.join("app-test/foo-1.0-1.gpkg.tar");
        std::fs::create_dir_all(container.parent().unwrap()).unwrap();
        write_gpkg(
            &crate::GpkgInput {
                image_dir: &image,
                metadata_dir: &meta,
                basename: "foo-1.0",
                signing: None,
                build_id: None,
            },
            container.as_std_path(),
        )
        .unwrap();

        let (count, skipped) = index_pkgdir(
            pkgdir,
            &crate::regen::chost_config("aarch64-unknown-linux-gnu"),
        )
        .unwrap();
        assert_eq!(count, 1);
        assert_eq!(skipped, 0);

        let idx = std::fs::read_to_string(pkgdir.join("Packages")).unwrap();
        assert!(idx.contains("VERSION: 0"));
        assert!(idx.contains("CHOST: aarch64-unknown-linux-gnu"));
        assert!(idx.contains("PACKAGES: 1"));
        // Header sorts before entries; CHOST < VERSION alphabetically, so the
        // file leads with CHOST (portage sorts header keys the same way).
        assert!(idx.starts_with("CHOST: aarch64-unknown-linux-gnu\n"));

        assert!(idx.contains("CPV: app-test/foo-1.0"));
        assert!(idx.contains("PATH: app-test/foo-1.0-1.gpkg.tar"));
        assert!(idx.contains("BUILD_ID: 1"));
        assert!(idx.contains("REPO: gentoo"));
        assert!(idx.contains("CFLAGS: -O2 -pipe -mcpu=ampere1a"));
        assert!(idx.contains("CXXFLAGS: -O2 -pipe -mcpu=ampere1a"));
        assert!(idx.contains("RUSTFLAGS: -C target-cpu=neoverse-n1"));
        assert!(idx.contains("MD5: "));
        assert!(idx.contains("SHA1: "));
        assert!(idx.contains("SIZE: "));
        assert!(idx.contains("MTIME: "));
        assert!(idx.contains("DEPEND: virtual/zlib:= readline? ( sys-libs/readline:= )\n"));

        // Parser surface: provenance available without affecting reuse.
        let parsed = crate::BinpkgIndex::open(pkgdir.as_std_path()).unwrap();
        let e = parsed.get("app-test/foo-1.0").unwrap();
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].cflags, "-O2 -pipe -mcpu=ampere1a");
        assert_eq!(e[0].cxxflags, "-O2 -pipe -mcpu=ampere1a");
        assert_eq!(e[0].rustflags, "-C target-cpu=neoverse-n1");
        // Proves the key now differs from a no-RUSTFLAGS build of the same
        // CFLAGS/CXXFLAGS (the consumer-side hash math itself is already
        // covered by `build_env_key_*` in index.rs).
        assert!(!e[0].build_env_key.is_empty());
        assert_ne!(
            e[0].build_env_key,
            crate::index::build_env_key(
                "-O2 -pipe -mcpu=ampere1a",
                "-O2 -pipe -mcpu=ampere1a",
                "",
                ""
            )
        );
    }

    // The `Packages` writer and the no-index scan path resolve a container's
    // `BUILD_ID` the same way, so a package whose GPKG metadata predates the
    // field — carrying its instance only in the filename — reports one build
    // id, not two. They used to disagree here, and the scan's answer (always
    // `0`) made `--usepkg` pick the oldest instance of a multi-instance cpv.
    #[test]
    fn written_index_and_direct_scan_agree_on_a_filename_only_build_id() {
        let tmp = tempfile::tempdir().unwrap();
        let pkgdir = camino::Utf8Path::from_path(tmp.path()).unwrap();

        let image = tmp.path().join("image");
        std::fs::create_dir_all(image.join("usr/bin")).unwrap();
        std::fs::write(image.join("usr/bin/hello"), b"hi\n").unwrap();

        let meta = tmp.path().join("vdb/foo-1.0");
        std::fs::create_dir_all(&meta).unwrap();
        for (k, v) in [("PF", "foo-1.0"), ("CATEGORY", "app-test"), ("SLOT", "0")] {
            std::fs::write(meta.join(k), format!("{v}\n")).unwrap();
        }

        for build_id in [1, 2] {
            let container = pkgdir.join(format!("app-test/foo-1.0-{build_id}.gpkg.tar"));
            std::fs::create_dir_all(container.parent().unwrap()).unwrap();
            write_gpkg(
                &crate::GpkgInput {
                    image_dir: &image,
                    metadata_dir: &meta,
                    basename: "foo-1.0",
                    signing: None,
                    build_id: None,
                },
                container.as_std_path(),
            )
            .unwrap();
        }

        index_pkgdir(pkgdir, &crate::regen::chost_config("x86_64-pc-linux-gnu")).unwrap();
        let written = crate::BinpkgIndex::open(pkgdir.as_std_path()).unwrap();

        std::fs::remove_file(pkgdir.join("Packages")).unwrap();
        let scanned = crate::BinpkgIndex::open(pkgdir.as_std_path()).unwrap();

        let ids = |idx: &crate::BinpkgIndex| -> Vec<u32> {
            let mut ids: Vec<u32> = idx
                .get("app-test/foo-1.0")
                .unwrap()
                .iter()
                .map(|e| e.build_id)
                .collect();
            ids.sort_unstable();
            ids
        };
        assert_eq!(ids(&written), vec![1, 2]);
        assert_eq!(ids(&scanned), ids(&written));
    }

    fn seed(pkgdir: &Utf8Path, pf: &str, meta: &[(&str, &str)]) {
        let work = tempfile::tempdir().unwrap();
        let image = work.path().join("image");
        std::fs::create_dir_all(&image).unwrap();
        let dir = work.path().join("meta");
        std::fs::create_dir_all(&dir).unwrap();
        for (k, v) in [("PF", pf), ("CATEGORY", "app-test")].iter().chain(meta) {
            std::fs::write(dir.join(k), format!("{v}\n")).unwrap();
        }
        let container = pkgdir.join(format!("app-test/{pf}-1.gpkg.tar"));
        std::fs::create_dir_all(container.parent().unwrap()).unwrap();
        write_gpkg(
            &crate::GpkgInput {
                image_dir: &image,
                metadata_dir: &dir,
                basename: pf,
                signing: None,
                build_id: None,
            },
            container.as_std_path(),
        )
        .unwrap();
    }

    fn blocks(text: &str) -> Vec<Vec<&str>> {
        text.split("\n\n")
            .filter(|b| !b.is_empty())
            .map(|b| b.lines().collect())
            .collect()
    }

    #[test]
    fn the_index_omits_what_portage_omits_and_carries_repo_revisions() {
        let tmp = tempfile::tempdir().unwrap();
        let pkgdir = Utf8Path::from_path(tmp.path()).unwrap();
        let chost = "x86_64-pc-linux-gnu";
        seed(
            pkgdir,
            "bar-1.0",
            &[
                ("SLOT", "0"),
                ("EAPI", "0"),
                ("CHOST", chost),
                ("repository", "gentoo"),
                ("BUILD_TIME", "200"),
                ("REPO_REVISIONS", r#"{"gentoo": "new"}"#),
            ],
        );
        seed(
            pkgdir,
            "foo-1.0",
            &[
                ("SLOT", "2"),
                ("EAPI", "8"),
                ("CHOST", "aarch64-unknown-linux-gnu"),
                ("BUILD_TIME", "100"),
                ("REPO_REVISIONS", r#"{"gentoo": "old", "guru": "g1"}"#),
            ],
        );
        let mut config = chost_config(chost);
        config.header.insert("ARCH".into(), "amd64".into());
        index_pkgdir(pkgdir, &config).unwrap();

        let text = std::fs::read_to_string(pkgdir.join("Packages")).unwrap();
        let blocks = blocks(&text);
        let header = &blocks[0];
        let keys: Vec<&str> = header
            .iter()
            .map(|l| l.split(": ").next().unwrap())
            .collect();
        assert_eq!(
            keys,
            [
                "ARCH",
                "CHOST",
                "PACKAGES",
                "REPO_REVISIONS",
                "TIMESTAMP",
                "VERSION"
            ]
        );
        assert!(header.contains(&r#"REPO_REVISIONS: {"gentoo": "new", "guru": "g1"}"#));

        let bar = &blocks[1];
        assert!(bar[0].starts_with("BUILD_ID: "));
        for gone in ["SLOT:", "EAPI:", "CHOST:"] {
            assert!(
                !bar.iter().any(|l| l.starts_with(gone)),
                "{gone} in {bar:?}"
            );
        }
        assert!(bar.contains(&r#"REPO_REVISIONS: {"gentoo": "new"}"#));
        assert!(bar[bar.len() - 2].starts_with("MTIME: "));
        assert_eq!(bar[bar.len() - 1], "REPO: gentoo");

        let foo = &blocks[2];
        assert!(foo.contains(&"SLOT: 2"));
        assert!(foo.contains(&"EAPI: 8"));
        assert!(foo.contains(&"CHOST: aarch64-unknown-linux-gnu"));

        // Read back: the omitted CHOST comes from the header.
        let idx = crate::BinpkgIndex::open(pkgdir.as_std_path()).unwrap();
        assert_eq!(idx.get("app-test/bar-1.0").unwrap()[0].chost, chost);

        let timestamp: u64 = header
            .iter()
            .find_map(|l| l.strip_prefix("TIMESTAMP: "))
            .unwrap()
            .parse()
            .unwrap();
        let mtime = std::fs::metadata(pkgdir.join("Packages"))
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(mtime, UNIX_EPOCH + Duration::from_secs(timestamp));
    }

    #[test]
    fn compress_index_writes_packages_gz_and_its_absence_removes_it() {
        use std::io::Read;
        let tmp = tempfile::tempdir().unwrap();
        let pkgdir = Utf8Path::from_path(tmp.path()).unwrap();
        seed(pkgdir, "foo-1.0", &[("SLOT", "0")]);
        let mut config = chost_config("x86_64-pc-linux-gnu");
        config.compress = true;
        index_pkgdir(pkgdir, &config).unwrap();

        let plain = std::fs::read_to_string(pkgdir.join("Packages")).unwrap();
        let gz = std::fs::File::open(pkgdir.join("Packages.gz")).unwrap();
        let mut unpacked = String::new();
        flate2::read::GzDecoder::new(gz)
            .read_to_string(&mut unpacked)
            .unwrap();
        assert_eq!(unpacked, plain);

        config.compress = false;
        index_pkgdir(pkgdir, &config).unwrap();
        assert!(!pkgdir.join("Packages.gz").exists());
    }
}
