use std::io::Read;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use sha2::Digest;

/// A single entry in a `Manifest` file (GLEP 74)
///
/// See [GLEP 74](https://www.gentoo.org/glep/glep-0074.html).
#[derive(Debug, Clone, PartialEq)]
pub enum ManifestEntry {
    /// `DIST` — a distfile (downloaded source archive)
    Dist {
        /// Basename of the distfile
        filename: String,
        /// Expected file size in bytes
        size: u64,
        /// `(algorithm, hex-digest)` pairs (e.g. `BLAKE2B`, `SHA512`)
        hashes: Vec<(String, String)>,
    },
    /// `DATA` / `EBUILD` / `MISC` / `AUX` — a tracked repo file
    Data {
        /// Which tag the line carried
        kind: DataKind,
        /// Path to the file, relative to the package directory (`files/` for `AUX`)
        path: String,
        /// Expected file size in bytes
        size: u64,
        /// `(algorithm, hex-digest)` pairs (e.g. `BLAKE2B`, `SHA512`)
        hashes: Vec<(String, String)>,
    },
    /// `MANIFEST` — a sub-manifest reference
    SubManifest {
        /// Repository-relative path to the nested `Manifest` file
        path: String,
        /// Expected file size in bytes
        size: u64,
        /// `(algorithm, hex-digest)` pairs (e.g. `BLAKE2B`, `SHA512`)
        hashes: Vec<(String, String)>,
    },
    /// `IGNORE` — path excluded from manifest checks
    Ignore {
        /// Repository-relative path prefix or file to skip
        path: String,
    },
    /// `TIMESTAMP` — last-updated RFC 3339 timestamp
    Timestamp {
        /// RFC 3339 timestamp string from the manifest line
        value: String,
    },
}

/// The tag of a [`ManifestEntry::Data`] line
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DataKind {
    /// `AUX`: a file under the package's `files/`
    Aux,
    /// `DATA`: a GLEP 74 tree file
    Data,
    /// `EBUILD`: an ebuild in the package directory
    Ebuild,
    /// `MISC`: any other package file, such as `metadata.xml`
    Misc,
}

impl DataKind {
    /// The Manifest tag
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Aux => "AUX",
            Self::Data => "DATA",
            Self::Ebuild => "EBUILD",
            Self::Misc => "MISC",
        }
    }
}

impl ManifestEntry {
    /// A `DIST` entry for the distfile at `path`, with the size and every hash in `algos`
    pub fn dist_from_file(filename: &str, path: &Path, algos: &[String]) -> Result<Self> {
        let (size, hashes) = hash_file(path, algos)?;
        Ok(Self::Dist {
            filename: filename.to_owned(),
            size,
            hashes,
        })
    }

    /// The line tag and the name the entry is keyed by
    fn key(&self) -> (&str, &str) {
        match self {
            Self::Dist { filename, .. } => ("DIST", filename),
            Self::Data { kind, path, .. } => (kind.as_str(), path),
            Self::SubManifest { path, .. } => ("MANIFEST", path),
            Self::Ignore { path } => ("IGNORE", path),
            Self::Timestamp { value } => ("TIMESTAMP", value),
        }
    }

    /// Verify that `path` matches this Manifest entry's recorded size and hashes
    ///
    /// `Ignore` and `Timestamp` entries always return `Ok(())`.
    /// For all other variants the file is read once and checked.
    pub fn verify_file(&self, path: &Path) -> Result<()> {
        match self {
            ManifestEntry::Ignore { .. } | ManifestEntry::Timestamp { .. } => Ok(()),
            ManifestEntry::Dist { size, hashes, .. }
            | ManifestEntry::Data { size, hashes, .. }
            | ManifestEntry::SubManifest { size, hashes, .. } => verify_hashes(path, *size, hashes),
        }
    }

    /// Whether this entry lists a hash [`Self::verify_file`] can check
    ///
    /// Always `true` for `Ignore` and `Timestamp`, which need none.
    pub fn has_supported_hash(&self) -> bool {
        match self {
            ManifestEntry::Ignore { .. } | ManifestEntry::Timestamp { .. } => true,
            ManifestEntry::Dist { hashes, .. }
            | ManifestEntry::Data { hashes, .. }
            | ManifestEntry::SubManifest { hashes, .. } => hashes
                .iter()
                .any(|(a, _)| SUPPORTED_HASHES.contains(&a.as_str())),
        }
    }
}

impl std::fmt::Display for ManifestEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (tag, name) = self.key();
        write!(f, "{tag} {name}")?;
        match self {
            Self::Dist { size, hashes, .. }
            | Self::Data { size, hashes, .. }
            | Self::SubManifest { size, hashes, .. } => {
                write!(f, " {size}")?;
                let mut hashes: Vec<_> = hashes.iter().collect();
                hashes.sort();
                for (algo, hex) in hashes {
                    write!(f, " {algo} {hex}")?;
                }
                Ok(())
            }
            Self::Ignore { .. } | Self::Timestamp { .. } => Ok(()),
        }
    }
}

const SUPPORTED_HASHES: [&str; 3] = ["BLAKE2B", "SHA512", "SHA256"];

/// Size and `algos` digests of `path`, refusing an algorithm this crate cannot compute
fn hash_file(path: &Path, algos: &[String]) -> Result<(u64, Vec<(String, String)>)> {
    if let Some(algo) = algos
        .iter()
        .find(|a| !SUPPORTED_HASHES.contains(&a.as_str()))
    {
        return Err(Error::InvalidManifest(format!(
            "unsupported manifest hash {algo}"
        )));
    }
    let size = std::fs::metadata(path)
        .map_err(|e| Error::Io {
            path: path.to_path_buf(),
            source: e,
        })?
        .len();
    let wanted: Vec<&str> = algos.iter().map(String::as_str).collect();
    Ok((size, digest_file(path, &wanted)?))
}

/// Hex digests of `path` for each algorithm in `algos` this crate supports, in that order, in one read pass
fn digest_file(path: &Path, algos: &[&str]) -> Result<Vec<(String, String)>> {
    let mut blake2b_h: Option<blake2::Blake2b512> = algos.contains(&"BLAKE2B").then(Digest::new);
    let mut sha512_h: Option<sha2::Sha512> = algos.contains(&"SHA512").then(Digest::new);
    let mut sha256_h: Option<sha2::Sha256> = algos.contains(&"SHA256").then(Digest::new);

    let file = std::fs::File::open(path).map_err(|e| Error::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    let mut reader = std::io::BufReader::new(file);
    let mut buf = vec![0u8; 65536];
    loop {
        let n = reader.read(&mut buf).map_err(|e| Error::Io {
            path: path.to_path_buf(),
            source: e,
        })?;
        if n == 0 {
            break;
        }
        let chunk = &buf[..n];
        if let Some(h) = blake2b_h.as_mut() {
            h.update(chunk);
        }
        if let Some(h) = sha512_h.as_mut() {
            h.update(chunk);
        }
        if let Some(h) = sha256_h.as_mut() {
            h.update(chunk);
        }
    }

    let mut blake2b_hex = blake2b_h.map(|h| hex::encode(h.finalize()));
    let mut sha512_hex = sha512_h.map(|h| hex::encode(h.finalize()));
    let mut sha256_hex = sha256_h.map(|h| hex::encode(h.finalize()));
    Ok(algos
        .iter()
        .filter_map(|&algo| {
            let hex = match algo {
                "BLAKE2B" => blake2b_hex.take(),
                "SHA512" => sha512_hex.take(),
                "SHA256" => sha256_hex.take(),
                _ => None,
            }?;
            Some((algo.to_owned(), hex))
        })
        .collect())
}

/// Single-pass size + multi-hash verification
fn verify_hashes(path: &Path, expected_size: u64, hashes: &[(String, String)]) -> Result<()> {
    // --- size check (cheap, no I/O beyond stat) ---
    let actual_size = std::fs::metadata(path)
        .map_err(|e| Error::Io {
            path: path.to_path_buf(),
            source: e,
        })?
        .len();
    if actual_size != expected_size {
        return Err(Error::ManifestVerifyFailed {
            path: path.to_path_buf(),
            reason: format!(
                "size mismatch: expected {} bytes, got {}",
                expected_size, actual_size
            ),
        });
    }

    let wanted: Vec<&str> = hashes
        .iter()
        .map(|(a, _)| a.as_str())
        .filter(|a| SUPPORTED_HASHES.contains(a))
        .collect();
    if wanted.is_empty() {
        return Err(Error::ManifestVerifyFailed {
            path: path.to_path_buf(),
            reason: "insufficient data: no supported hash".to_owned(),
        });
    }
    let digests = digest_file(path, &wanted)?;

    for (algo, expected_hex) in hashes {
        let actual_opt = digests
            .iter()
            .find(|(a, _)| a == algo)
            .map(|(_, hex)| hex.as_str());
        if let Some(actual) = actual_opt
            && actual != expected_hex.to_lowercase()
        {
            return Err(Error::ManifestVerifyFailed {
                path: path.to_path_buf(),
                reason: format!(
                    "{} mismatch: expected {}, got {}",
                    algo, expected_hex, actual
                ),
            });
        }
    }

    Ok(())
}

/// Parsed representation of a `Manifest` file (GLEP 74)
///
/// In practice, the Gentoo repository uses `thin-manifests = true`, so only
/// `DIST` entries appear.
#[derive(Debug, Clone)]
pub struct Manifest {
    /// Parsed manifest lines in file order
    pub entries: Vec<ManifestEntry>,
}

impl Manifest {
    /// Parse a `Manifest` from its text content
    ///
    /// Blank lines and lines starting with `#` are silently skipped.
    /// Unknown type tags are also silently skipped for forward compatibility.
    pub fn parse(input: &str) -> Result<Self> {
        let mut entries = Vec::new();

        for (lineno, raw) in input.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            let mut tokens = line.split_ascii_whitespace();
            let Some(tag) = tokens.next() else {
                continue;
            };

            let entry = match tag {
                "DIST" => {
                    let (path, size, hashes) = parse_path_size_hashes(tag, line, lineno)?;
                    ManifestEntry::Dist {
                        filename: path,
                        size,
                        hashes,
                    }
                }
                "DATA" | "EBUILD" | "MISC" | "AUX" => {
                    let kind = match tag {
                        "AUX" => DataKind::Aux,
                        "EBUILD" => DataKind::Ebuild,
                        "MISC" => DataKind::Misc,
                        _ => DataKind::Data,
                    };
                    let (path, size, hashes) = parse_path_size_hashes(tag, line, lineno)?;
                    ManifestEntry::Data {
                        kind,
                        path,
                        size,
                        hashes,
                    }
                }
                "MANIFEST" => {
                    let (path, size, hashes) = parse_path_size_hashes(tag, line, lineno)?;
                    ManifestEntry::SubManifest { path, size, hashes }
                }
                "IGNORE" => {
                    let path = tokens.next().ok_or_else(|| {
                        Error::InvalidManifest(format!(
                            "line {}: IGNORE requires a path",
                            lineno + 1
                        ))
                    })?;
                    ManifestEntry::Ignore {
                        path: path.to_string(),
                    }
                }
                "TIMESTAMP" => {
                    let value = tokens.next().ok_or_else(|| {
                        Error::InvalidManifest(format!(
                            "line {}: TIMESTAMP requires a value",
                            lineno + 1
                        ))
                    })?;
                    ManifestEntry::Timestamp {
                        value: value.to_string(),
                    }
                }
                _ => {
                    // Unknown type tag — skip for forward compatibility.
                    continue;
                }
            };

            entries.push(entry);
        }

        Ok(Manifest { entries })
    }

    /// Iterate over `DIST` entries only
    pub fn dist_entries(&self) -> impl Iterator<Item = &ManifestEntry> {
        self.entries
            .iter()
            .filter(|e| matches!(e, ManifestEntry::Dist { .. }))
    }

    /// Add `entry`, replacing any entry with the same tag and name
    pub fn upsert(&mut self, entry: ManifestEntry) {
        let key = entry.key();
        match self.entries.iter().position(|e| e.key() == key) {
            Some(i) => self.entries[i] = entry,
            None => self.entries.push(entry),
        }
    }

    /// The Manifest text, one line per entry sorted by tag then name, as portage writes it
    pub fn render(&self) -> String {
        let mut entries: Vec<&ManifestEntry> = self.entries.iter().collect();
        entries.sort_by(|a, b| a.key().cmp(&b.key()));
        entries.iter().map(|e| format!("{e}\n")).collect()
    }

    /// Replace every `EBUILD`/`AUX`/`MISC` entry with ones hashed from `pkg_dir` (a thick Manifest)
    pub fn regenerate_package_files(&mut self, pkg_dir: &Path, algos: &[String]) -> Result<()> {
        self.entries.retain(|e| {
            !matches!(
                e,
                ManifestEntry::Data {
                    kind: DataKind::Ebuild | DataKind::Aux | DataKind::Misc,
                    ..
                }
            )
        });
        for (kind, path) in package_files(pkg_dir)? {
            let (size, hashes) = hash_file(&package_file_path(pkg_dir, kind, &path), algos)?;
            self.entries.push(ManifestEntry::Data {
                kind,
                path,
                size,
                hashes,
            });
        }
        Ok(())
    }

    /// Verify the `EBUILD`/`AUX`/`MISC` entries against `pkg_dir`; a listed `MISC` file may be absent
    pub fn verify_package_dir(&self, pkg_dir: &Path) -> Result<()> {
        for entry in &self.entries {
            let ManifestEntry::Data { kind, path, .. } = entry else {
                continue;
            };
            if *kind == DataKind::Data {
                continue;
            }
            let file = package_file_path(pkg_dir, *kind, path);
            if *kind == DataKind::Misc && !file.exists() {
                continue;
            }
            entry.verify_file(&file)?;
        }
        Ok(())
    }

    /// Ebuilds and `files/` entries in `pkg_dir` that the Manifest does not list
    pub fn unlisted_package_files(&self, pkg_dir: &Path) -> Result<Vec<PathBuf>> {
        let listed = |kind: DataKind, path: &str| {
            self.entries.iter().any(|e| {
                matches!(e, ManifestEntry::Data { kind: k, path: p, .. } if *k == kind && p == path)
            })
        };
        Ok(package_files(pkg_dir)?
            .into_iter()
            .filter(|(kind, path)| *kind != DataKind::Misc && !listed(*kind, path))
            .map(|(kind, path)| package_file_path(pkg_dir, kind, &path))
            .collect())
    }
}

fn package_file_path(pkg_dir: &Path, kind: DataKind, path: &str) -> PathBuf {
    match kind {
        DataKind::Aux => pkg_dir.join("files").join(path),
        _ => pkg_dir.join(path),
    }
}

/// The files a thick Manifest covers in `pkg_dir`: ebuilds, other top-level files, and `files/`
///
/// Hidden names, `CVS` and `files/digest-*` are left out, as portage does.
fn package_files(pkg_dir: &Path) -> Result<Vec<(DataKind, String)>> {
    let io = |path: &Path| {
        let path = path.to_path_buf();
        move |source| Error::Io { path, source }
    };
    let mut out = Vec::new();
    for entry in std::fs::read_dir(pkg_dir).map_err(io(pkg_dir))? {
        let entry = entry.map_err(io(pkg_dir))?;
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if name.starts_with('.') || name == "Manifest" || !entry.path().is_file() {
            continue;
        }
        let kind = if name.ends_with(".ebuild") {
            DataKind::Ebuild
        } else {
            DataKind::Misc
        };
        out.push((kind, name));
    }
    let mut dirs = vec![PathBuf::new()];
    let files = pkg_dir.join("files");
    while let Some(rel) = dirs.pop() {
        let dir = files.join(&rel);
        let Ok(read) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in read {
            let entry = entry.map_err(io(&dir))?;
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            if name.starts_with('.') || name == "CVS" {
                continue;
            }
            let rel = rel.join(&name);
            if entry.path().is_dir() {
                dirs.push(rel);
            } else if let Some(rel) = rel.to_str()
                && !rel.starts_with("digest-")
            {
                out.push((DataKind::Aux, rel.to_owned()));
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Parse `tag path size [algo hex ...]` from a whitespace-split line
#[allow(clippy::type_complexity)]
fn parse_path_size_hashes(
    tag: &str,
    line: &str,
    lineno: usize,
) -> Result<(String, u64, Vec<(String, String)>)> {
    let mut tokens = line.split_ascii_whitespace();
    tokens.next(); // skip the tag itself

    let path = tokens.next().ok_or_else(|| {
        Error::InvalidManifest(format!("line {}: {} requires a path", lineno + 1, tag))
    })?;

    let size_str = tokens.next().ok_or_else(|| {
        Error::InvalidManifest(format!("line {}: {} requires a size", lineno + 1, tag))
    })?;

    let size: u64 = size_str.parse().map_err(|_| {
        Error::InvalidManifest(format!(
            "line {}: {} size {:?} is not a valid integer",
            lineno + 1,
            tag,
            size_str
        ))
    })?;

    let mut hashes = Vec::new();
    loop {
        match (tokens.next(), tokens.next()) {
            (Some(algo), Some(hex)) => hashes.push((algo.to_string(), hex.to_string())),
            (None, _) => break,
            (Some(algo), None) => {
                return Err(Error::InvalidManifest(format!(
                    "line {}: hash algo {:?} has no corresponding hex value",
                    lineno + 1,
                    algo
                )));
            }
        }
    }

    Ok((path.to_string(), size, hashes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_dist_entry() {
        let input = "DIST foo-1.0.tar.gz 12345 SHA256 abcd1234 SHA512 ef567890\n";
        let manifest = Manifest::parse(input).unwrap();
        assert_eq!(manifest.entries.len(), 1);
        match &manifest.entries[0] {
            ManifestEntry::Dist {
                filename,
                size,
                hashes,
            } => {
                assert_eq!(filename, "foo-1.0.tar.gz");
                assert_eq!(*size, 12345);
                assert_eq!(
                    hashes,
                    &[
                        ("SHA256".into(), "abcd1234".into()),
                        ("SHA512".into(), "ef567890".into())
                    ]
                );
            }
            _ => panic!("expected Dist"),
        }
    }

    #[test]
    fn parse_ignore_entry() {
        let input = "IGNORE some/path\n";
        let manifest = Manifest::parse(input).unwrap();
        assert_eq!(manifest.entries.len(), 1);
        match &manifest.entries[0] {
            ManifestEntry::Ignore { path } => assert_eq!(path, "some/path"),
            _ => panic!("expected Ignore"),
        }
    }

    #[test]
    fn parse_timestamp_entry() {
        let input = "TIMESTAMP 2024-01-15T12:00:00Z\n";
        let manifest = Manifest::parse(input).unwrap();
        assert_eq!(manifest.entries.len(), 1);
        match &manifest.entries[0] {
            ManifestEntry::Timestamp { value } => assert_eq!(value, "2024-01-15T12:00:00Z"),
            _ => panic!("expected Timestamp"),
        }
    }

    #[test]
    fn parse_data_aliases() {
        for tag in &["DATA", "EBUILD", "MISC", "AUX"] {
            let input = format!("{} files/foo.patch 42 SHA256 deadbeef\n", tag);
            let manifest = Manifest::parse(&input).unwrap();
            assert_eq!(manifest.entries.len(), 1);
            assert!(matches!(manifest.entries[0], ManifestEntry::Data { .. }));
        }
    }

    #[test]
    fn render_round_trips_parse_and_sorts_like_portage() {
        let input = "\
EBUILD foo-1.ebuild 3 SHA512 bb BLAKE2B aa
DIST b.tar.gz 2 BLAKE2B cc SHA512 dd
AUX fix.patch 1 BLAKE2B ee SHA512 ff
DIST a.tar.gz 4 BLAKE2B 11 SHA512 22
";
        let manifest = Manifest::parse(input).unwrap();
        let rendered = manifest.render();
        assert_eq!(
            rendered,
            "\
AUX fix.patch 1 BLAKE2B ee SHA512 ff
DIST a.tar.gz 4 BLAKE2B 11 SHA512 22
DIST b.tar.gz 2 BLAKE2B cc SHA512 dd
EBUILD foo-1.ebuild 3 BLAKE2B aa SHA512 bb
"
        );
        assert_eq!(Manifest::parse(&rendered).unwrap().entries.len(), 4);
    }

    #[test]
    fn dist_from_file_hashes_with_the_requested_algorithms() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut f, b"hello").unwrap();
        let algos = ["BLAKE2B".to_owned(), "SHA512".to_owned()];
        let entry = ManifestEntry::dist_from_file("hello.tar", f.path(), &algos).unwrap();
        assert_eq!(
            entry,
            ManifestEntry::Dist {
                filename: "hello.tar".into(),
                size: 5,
                hashes: hello_hashes(),
            }
        );
        assert!(ManifestEntry::dist_from_file("hello.tar", f.path(), &["MD5".to_owned()]).is_err());
    }

    fn package_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("files/sub")).unwrap();
        std::fs::write(dir.path().join("foo-1.ebuild"), "EAPI=8\n").unwrap();
        std::fs::write(dir.path().join("metadata.xml"), "<pkgmetadata/>\n").unwrap();
        std::fs::write(dir.path().join("files/sub/fix.patch"), "patch\n").unwrap();
        dir
    }

    #[test]
    fn a_regenerated_thick_manifest_verifies_with_aux_paths_under_files() {
        let dir = package_dir();
        let mut manifest = Manifest { entries: vec![] };
        let algos = ["BLAKE2B".to_owned(), "SHA512".to_owned()];
        manifest
            .regenerate_package_files(dir.path(), &algos)
            .unwrap();
        let rendered = manifest.render();
        assert!(rendered.starts_with("AUX sub/fix.patch 6 BLAKE2B "));
        assert!(rendered.contains("\nEBUILD foo-1.ebuild 7 "));
        assert!(rendered.contains("\nMISC metadata.xml 15 "));
        let manifest = Manifest::parse(&rendered).unwrap();
        manifest.verify_package_dir(dir.path()).unwrap();
        assert!(
            manifest
                .unlisted_package_files(dir.path())
                .unwrap()
                .is_empty()
        );

        std::fs::write(dir.path().join("foo-1.ebuild"), "EAPI=7\n").unwrap();
        assert!(manifest.verify_package_dir(dir.path()).is_err());
    }

    #[test]
    fn unlisted_ebuilds_and_aux_files_are_reported() {
        let dir = package_dir();
        let manifest = Manifest { entries: vec![] };
        std::fs::write(dir.path().join("files/.hidden"), "").unwrap();
        std::fs::write(dir.path().join("files/digest-foo-1"), "").unwrap();
        assert_eq!(
            manifest.unlisted_package_files(dir.path()).unwrap(),
            [
                dir.path().join("files/sub/fix.patch"),
                dir.path().join("foo-1.ebuild"),
            ]
        );
    }

    #[test]
    fn parse_sub_manifest() {
        let input = "MANIFEST sub/Manifest 99 SHA256 cafebabe\n";
        let manifest = Manifest::parse(input).unwrap();
        assert_eq!(manifest.entries.len(), 1);
        assert!(matches!(
            manifest.entries[0],
            ManifestEntry::SubManifest { .. }
        ));
    }

    #[test]
    fn parse_empty_file() {
        let manifest = Manifest::parse("").unwrap();
        assert!(manifest.entries.is_empty());
    }

    #[test]
    fn parse_blank_lines_and_comments() {
        let input = "# This is a comment\n\nDIST bar-2.0.tar.xz 99 SHA256 ff00\n\n# end\n";
        let manifest = Manifest::parse(input).unwrap();
        assert_eq!(manifest.entries.len(), 1);
    }

    #[test]
    fn parse_unknown_type_skipped() {
        let input = "FUTURE somepath 0 SHA256 aabbcc\nDIST foo.tar.gz 1 SHA256 001122\n";
        let manifest = Manifest::parse(input).unwrap();
        assert_eq!(manifest.entries.len(), 1);
        assert!(matches!(manifest.entries[0], ManifestEntry::Dist { .. }));
    }

    #[test]
    fn parse_multiple_entries() {
        let input = "\
DIST a.tar.gz 100 SHA256 aa
DIST b.tar.bz2 200 SHA256 bb
IGNORE obsolete/
TIMESTAMP 2024-06-01T00:00:00Z
";
        let manifest = Manifest::parse(input).unwrap();
        assert_eq!(manifest.entries.len(), 4);
        assert_eq!(manifest.dist_entries().count(), 2);
    }

    #[test]
    fn parse_dist_no_hashes() {
        // DIST with just path and size, no hash pairs — valid edge case.
        let input = "DIST foo.tar.gz 42\n";
        let manifest = Manifest::parse(input).unwrap();
        match &manifest.entries[0] {
            ManifestEntry::Dist { hashes, .. } => assert!(hashes.is_empty()),
            _ => panic!("expected Dist"),
        }
    }

    #[test]
    fn parse_error_dist_missing_size() {
        let input = "DIST foo.tar.gz\n";
        assert!(Manifest::parse(input).is_err());
    }

    #[test]
    fn parse_error_dist_bad_size() {
        let input = "DIST foo.tar.gz notanumber SHA256 aabb\n";
        assert!(Manifest::parse(input).is_err());
    }

    #[test]
    fn parse_error_orphan_hash_algo() {
        let input = "DIST foo.tar.gz 42 SHA256\n";
        assert!(Manifest::parse(input).is_err());
    }

    // --- verify_file tests ---

    /// Pre-computed hashes for the 5-byte content b"hello"
    fn hello_hashes() -> Vec<(String, String)> {
        use sha2::Digest;
        let blake2b = hex::encode(blake2::Blake2b512::digest(b"hello"));
        let sha512 = hex::encode(sha2::Sha512::digest(b"hello"));
        vec![
            ("BLAKE2B".to_string(), blake2b),
            ("SHA512".to_string(), sha512),
        ]
    }

    #[test]
    fn verify_correct_hashes() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut f, b"hello").unwrap();
        let entry = ManifestEntry::Dist {
            filename: "test".into(),
            size: 5,
            hashes: hello_hashes(),
        };
        assert!(entry.verify_file(f.path()).is_ok());
    }

    #[test]
    fn verify_wrong_hash() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut f, b"hello").unwrap();
        let mut hashes = hello_hashes();
        hashes[0].1 = "deadbeef".to_string(); // corrupt BLAKE2B hex
        let entry = ManifestEntry::Dist {
            filename: "test".into(),
            size: 5,
            hashes,
        };
        let err = entry.verify_file(f.path()).unwrap_err();
        assert!(matches!(
            err,
            crate::error::Error::ManifestVerifyFailed { .. }
        ));
        assert!(err.to_string().contains("BLAKE2B mismatch"));
    }

    #[test]
    fn verify_wrong_size() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut f, b"hello").unwrap();
        let entry = ManifestEntry::Dist {
            filename: "test".into(),
            size: 999, // wrong
            hashes: hello_hashes(),
        };
        let err = entry.verify_file(f.path()).unwrap_err();
        assert!(matches!(
            err,
            crate::error::Error::ManifestVerifyFailed { .. }
        ));
        assert!(err.to_string().contains("size mismatch"));
    }

    #[test]
    fn verify_refuses_an_entry_with_no_supported_hash() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut f, b"hello").unwrap();
        let entry = |hashes| ManifestEntry::Dist {
            filename: "test".into(),
            size: 5,
            hashes,
        };
        let only_unknown = vec![("WHIRLPOOL".to_string(), "doesnotmatter".to_string())];
        let err = entry(only_unknown).verify_file(f.path()).unwrap_err();
        assert!(err.to_string().contains("no supported hash"));
        assert!(entry(vec![]).verify_file(f.path()).is_err());

        let mut mixed = hello_hashes();
        mixed.push(("WHIRLPOOL".to_string(), "doesnotmatter".to_string()));
        assert!(entry(mixed).verify_file(f.path()).is_ok());
    }

    #[test]
    fn verify_ignore_noop() {
        let entry = ManifestEntry::Ignore {
            path: "some/path".into(),
        };
        // Path doesn't even need to exist.
        assert!(
            entry
                .verify_file(std::path::Path::new("/nonexistent/path"))
                .is_ok()
        );
    }

    #[test]
    fn verify_missing_file() {
        let entry = ManifestEntry::Dist {
            filename: "test".into(),
            size: 5,
            hashes: hello_hashes(),
        };
        let err = entry
            .verify_file(std::path::Path::new("/nonexistent/missing.tar.gz"))
            .unwrap_err();
        assert!(matches!(err, crate::error::Error::Io { .. }));
    }
}
