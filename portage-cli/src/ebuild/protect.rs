//! `CONFIG_PROTECT` / `CONFIG_PROTECT_MASK` resolution and `._cfg` diversion
//!
//! The lists are Portage's stacked values: a build shell holds them seeded from the
//! settings (make.globals and env.d included), and a caller without one reads the settings.

use camino::{Utf8Path, Utf8PathBuf};

/// CONFIG_PROTECT / CONFIG_PROTECT_MASK resolution (portage's `ConfigProtect`)
///
/// A path is protected when the longest matching `CONFIG_PROTECT` prefix is
/// longer than the longest matching `CONFIG_PROTECT_MASK` prefix. Protected
/// files that already exist and differ are diverted to `._cfgNNNN_<name>`
/// for `dispatch-conf`/`etc-update` instead of being overwritten.
///
/// Deliberately real-Portage semantics, not the PMS 13.3.3 letter (which
/// gives an ancestor `CONFIG_PROTECT_MASK` unconditional priority regardless
/// of how deep the matching `CONFIG_PROTECT` entry is). Matching what real
/// `emerge` actually does on a system is the intended behavior here.
pub(crate) struct ConfigProtect {
    protect: Vec<String>,
    mask: Vec<String>,
}

impl ConfigProtect {
    /// Read the lists from a build shell
    pub(crate) fn from_shell(shell: &portage_repo::EbuildShell) -> Self {
        let read = |name: &str| shell.get_var(name).unwrap_or_default();
        Self::from_lists(&read("CONFIG_PROTECT"), &read("CONFIG_PROTECT_MASK"))
    }

    /// Read the lists from a config root's settings, for callers with no build shell (`em etc`)
    pub(crate) fn from_settings(settings: &portage_repo::Settings) -> Self {
        let read = |name: &str| settings.get(name).unwrap_or_default();
        Self::from_lists(read("CONFIG_PROTECT"), read("CONFIG_PROTECT_MASK"))
    }

    /// `/etc` is always protected, as make.globals guarantees.
    fn from_lists(protect: &str, mask: &str) -> Self {
        let paths = |list: &str| -> Vec<String> {
            list.split_whitespace()
                .map(|s| s.trim_end_matches('/').to_string())
                .filter(|s| !s.is_empty())
                .collect()
        };
        let mut protect = paths(protect);
        if !protect.iter().any(|p| p == "/etc") {
            protect.push("/etc".to_string());
        }
        Self {
            protect,
            mask: paths(mask),
        }
    }

    /// The protected directories themselves, for a caller that has to walk
    /// them rather than test a single path.
    pub(crate) fn protected_dirs(&self) -> &[String] {
        &self.protect
    }

    /// Length of the longest entry in `list` that prefix-matches `obj` on
    /// whole components (`obj == p` or `obj` under `p/`); 0 if none.
    fn longest_match(list: &[String], obj: &str) -> usize {
        list.iter()
            .filter(|p| obj == p.as_str() || obj.starts_with(&format!("{p}/")))
            .map(String::len)
            .max()
            .unwrap_or(0)
    }

    pub(crate) fn is_protected(&self, obj: &Utf8Path) -> bool {
        let obj = obj.as_str();
        Self::longest_match(&self.protect, obj) > Self::longest_match(&self.mask, obj)
    }

    /// Explicit lists, for a test that needs a specific protect/mask pair
    /// without a shell or a `make.conf`.
    #[cfg(test)]
    pub(crate) fn for_test(protect: &[&str], mask: &[&str]) -> Self {
        Self {
            protect: protect.iter().map(|s| (*s).to_string()).collect(),
            mask: mask.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    /// A config-protection set that protects nothing (for tests / contexts
    /// where protection does not apply).
    #[cfg(test)]
    pub(crate) fn none() -> Self {
        Self {
            protect: vec![],
            mask: vec![],
        }
    }
}

/// portage's `new_protect_filename`: the next `._cfgNNNN_<name>` beside
/// `dest` (highest existing index + 1), plus the most recent existing one
/// so the caller can reuse it when the content already matches.
pub(crate) fn scan_cfg(dest: &Utf8Path) -> (Utf8PathBuf, Option<Utf8PathBuf>) {
    let dir = dest.parent().unwrap_or_else(|| Utf8Path::new("/"));
    let name = dest.file_name().unwrap_or_default();
    let mut highest: i32 = -1;
    let mut latest: Option<Utf8PathBuf> = None;
    if let Ok(rd) = std::fs::read_dir(dir.as_std_path()) {
        for entry in rd.flatten() {
            let Ok(f) = entry.file_name().into_string() else {
                continue;
            };
            // ._cfg<4 digits>_<name>
            let Some(rest) = f.strip_prefix("._cfg") else {
                continue;
            };
            if rest.len() > 5
                && rest.as_bytes()[4] == b'_'
                && &rest[5..] == name
                && let Ok(n) = rest[..4].parse::<i32>()
                && n > highest
            {
                highest = n;
                latest = Some(dir.join(&f));
            }
        }
    }
    (dir.join(format!("._cfg{:04}_{name}", highest + 1)), latest)
}

/// Where the update of one config-protected image entry lands
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Landing {
    /// Overwrite the live path
    Replace,
    /// Write a `._cfg` sibling
    Divert,
    /// Write nothing: this exact update was already offered once
    Keep,
}

/// The live path a protected image entry would replace
pub(crate) enum LiveNode {
    File {
        md5: String,
    },
    Link {
        target: std::path::PathBuf,
    },
    /// A directory, FIFO, device or socket
    Other,
}

impl LiveNode {
    /// What is at `path`, without following a final symlink
    pub(crate) fn at(path: &Utf8Path) -> Option<Self> {
        let ft = std::fs::symlink_metadata(path.as_std_path())
            .ok()?
            .file_type();
        Some(if ft.is_symlink() {
            Self::Link {
                target: std::fs::read_link(path.as_std_path()).unwrap_or_default(),
            }
        } else if ft.is_file() {
            Self::File {
                md5: std::fs::read(path.as_std_path())
                    .map(|d| format!("{:x}", md5::compute(&d)))
                    .unwrap_or_default(),
            }
        } else {
            Self::Other
        })
    }

    /// Whether this node already holds the update `src_md5` / `src_link`
    pub(crate) fn holds(&self, src_md5: &str, src_link: Option<&Utf8Path>) -> bool {
        match (self, src_link) {
            (Self::File { md5 }, None) => md5 == src_md5,
            (Self::Link { target }, Some(src)) => target == src.as_std_path(),
            _ => false,
        }
    }
}

/// Portage's config memory (`/var/lib/portage/config`) plus the replaced package's
/// CONTENTS: what `FEATURES=config-protect-if-modified` and `._cfg` diversion consult
///
/// The default is an empty memory with no replaced package.
#[derive(Default)]
pub(crate) struct ConfigMemory {
    file: Utf8PathBuf,
    /// `path md5` pairs in file order, as portage's `writedict` keeps them
    entries: Vec<(String, String)>,
    changed: bool,
    /// `NOCONFMEM` or a downgrade: offer every differing update again
    ignore: bool,
    if_modified: bool,
    old: std::collections::HashMap<Utf8PathBuf, portage_vdb::ContentsEntry>,
}

impl ConfigMemory {
    /// Read `<eroot>/var/lib/portage/config`; a missing or unreadable file is empty
    pub(crate) fn load(
        eroot: &Utf8Path,
        old: Vec<portage_vdb::ContentsEntry>,
        if_modified: bool,
        ignore: bool,
    ) -> Self {
        let file = eroot.join("var/lib/portage/config");
        let text = std::fs::read_to_string(file.as_std_path()).unwrap_or_default();
        let mut entries: Vec<(String, String)> = vec![];
        for line in text.lines().filter(|l| !l.starts_with('#')) {
            let mut words = line.split_whitespace().take_while(|w| !w.starts_with('#'));
            if let (Some(path), Some(md5)) = (words.next(), words.next()) {
                entries.retain(|(p, _)| p != path);
                entries.push((path.to_string(), md5.to_string()));
            }
        }
        Self {
            file,
            entries,
            changed: false,
            ignore,
            if_modified,
            old: old.into_iter().map(|e| (e.path.clone(), e)).collect(),
        }
    }

    fn remembered(&self, path: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|(p, _)| p == path)
            .map(|(_, m)| m.as_str())
    }

    fn remember(&mut self, path: &str, md5: &str) {
        match self.entries.iter_mut().find(|(p, _)| p == path) {
            Some((_, m)) if m == md5 => return,
            Some((_, m)) => *m = md5.to_string(),
            None => self.entries.push((path.to_string(), md5.to_string())),
        }
        self.changed = true;
    }

    /// Decide where the update of protected `path` lands
    ///
    /// `src_md5` is the new file's md5, or for a symlink [`link_md5`] of its target.
    pub(crate) fn decide(
        &mut self,
        path: &Utf8Path,
        src_md5: &str,
        src_link: Option<&Utf8Path>,
        live: Option<&LiveNode>,
    ) -> Landing {
        let old = self.old.get(path);
        let Some(live) = live else {
            // The admin removed a file the old version installed: ask before restoring it.
            return if old.is_some() {
                Landing::Divert
            } else {
                Landing::Replace
            };
        };
        if self.if_modified
            && let Some(old) = old
        {
            let untouched = match live {
                LiveNode::File { md5 } => old.md5.as_deref() == Some(md5.as_str()),
                LiveNode::Link { target } => old
                    .target
                    .as_ref()
                    .is_some_and(|t| t.as_std_path() == target),
                LiveNode::Other => false,
            };
            if untouched {
                return Landing::Replace;
            }
        }
        let same = live.holds(src_md5, src_link);
        if !same && !self.ignore && self.remembered(path.as_str()) == Some(src_md5) {
            return Landing::Keep;
        }
        self.remember(path.as_str(), src_md5);
        if same {
            Landing::Replace
        } else {
            Landing::Divert
        }
    }

    /// Write the memory back if a decision changed it
    pub(crate) fn store(&self) -> std::io::Result<()> {
        if !self.changed {
            return Ok(());
        }
        if let Some(dir) = self.file.parent() {
            std::fs::create_dir_all(dir.as_std_path())?;
        }
        let text: String = self
            .entries
            .iter()
            .map(|(p, m)| format!("{p} {m}\n"))
            .collect();
        let tmp = self
            .file
            .with_file_name(format!(".config.{}", std::process::id()));
        std::fs::write(tmp.as_std_path(), text)?;
        // Portage leaves the file root:portage 0660; keep whatever it has.
        if let Ok(meta) = std::fs::metadata(self.file.as_std_path()) {
            use std::os::unix::fs::MetadataExt;
            let _ = std::os::unix::fs::chown(tmp.as_std_path(), Some(meta.uid()), Some(meta.gid()));
            std::fs::set_permissions(tmp.as_std_path(), meta.permissions())?;
        }
        std::fs::rename(tmp.as_std_path(), self.file.as_std_path())
    }
}

/// The md5 a symlink is remembered by: that of its target string
pub(crate) fn link_md5(target: &std::path::Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    format!("{:x}", md5::compute(target.as_os_str().as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use portage_vdb::{ContentsEntry, ContentsKind};

    fn obj(path: &str, md5: &str) -> ContentsEntry {
        ContentsEntry {
            kind: ContentsKind::Obj,
            path: path.into(),
            md5: Some(md5.into()),
            mtime: Some(0),
            target: None,
        }
    }

    fn file(md5: &str) -> LiveNode {
        LiveNode::File { md5: md5.into() }
    }

    fn with_old(old: Vec<ContentsEntry>, if_modified: bool) -> ConfigMemory {
        ConfigMemory::load(Utf8Path::new("/nonexistent"), old, if_modified, false)
    }

    #[test]
    fn untouched_config_is_replaced_only_with_if_modified() {
        let path = Utf8Path::new("/etc/foo.conf");
        let mut on = with_old(vec![obj("/etc/foo.conf", "aaa")], true);
        assert_eq!(
            on.decide(path, "bbb", None, Some(&file("aaa"))),
            Landing::Replace
        );
        assert!(!on.changed);
        let mut off = with_old(vec![obj("/etc/foo.conf", "aaa")], false);
        assert_eq!(
            off.decide(path, "bbb", None, Some(&file("aaa"))),
            Landing::Divert
        );
        assert_eq!(off.remembered("/etc/foo.conf"), Some("bbb"));
    }

    #[test]
    fn remembered_update_is_offered_once() {
        let path = Utf8Path::new("/etc/foo.conf");
        let edited = file("edited");
        let mut m = ConfigMemory::default();
        assert_eq!(m.decide(path, "new", None, Some(&edited)), Landing::Divert);
        assert_eq!(m.decide(path, "new", None, Some(&edited)), Landing::Keep);
        assert_eq!(
            m.decide(path, "newer", None, Some(&edited)),
            Landing::Divert
        );
        m.ignore = true;
        assert_eq!(
            m.decide(path, "newer", None, Some(&edited)),
            Landing::Divert
        );
    }

    #[test]
    fn missing_file_the_old_version_owned_is_diverted() {
        let mut m = with_old(vec![obj("/etc/foo.conf", "aaa")], true);
        let path = Utf8Path::new("/etc/foo.conf");
        assert_eq!(m.decide(path, "aaa", None, None), Landing::Divert);
        assert_eq!(
            m.decide(Utf8Path::new("/etc/new"), "x", None, None),
            Landing::Replace
        );
    }

    #[test]
    fn identical_update_is_replaced_and_remembered() {
        let mut m = ConfigMemory::default();
        let path = Utf8Path::new("/etc/foo.conf");
        assert_eq!(
            m.decide(path, "same", None, Some(&file("same"))),
            Landing::Replace
        );
        assert_eq!(m.remembered("/etc/foo.conf"), Some("same"));
    }

    #[test]
    fn memory_file_round_trips_in_portage_format() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(tmp.path()).unwrap();
        let file = root.join("var/lib/portage/config");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "# c\n/etc/a 111\n/etc/b 222 # x\nbroken\n").unwrap();
        let mut m = ConfigMemory::load(root, vec![], true, false);
        assert_eq!(m.remembered("/etc/b"), Some("222"));
        m.remember("/etc/a", "333");
        m.remember("/etc/c", "444");
        m.store().unwrap();
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "/etc/a 333\n/etc/b 222\n/etc/c 444\n"
        );
    }
}
