//! `INSTALL_MASK` / `PKG_INSTALL_MASK`: paths pruned from an image before it is merged or packaged
//!
//! A word starting with `/` is a path under `${ED}` (it also masks everything below it; a
//! trailing `/` limits it to directories), any other word is matched against non-directory names, and
//! `-word` unmasks what an earlier word masked. Globs are fnmatch(3) style: `*` crosses `/`.

use std::path::Path;

/// One `INSTALL_MASK` word
struct Word {
    unmask: bool,
    /// The word started with `/`
    anchored: bool,
    /// Directories only (`/path/`)
    dir_only: bool,
    glob: glob::Pattern,
    /// `glob` followed by `/*`, for what lies below an anchored word
    below: Option<glob::Pattern>,
}

/// A parsed `INSTALL_MASK` value
pub(crate) struct InstallMask {
    words: Vec<Word>,
}

fn fnmatch_pattern(pattern: &str) -> glob::Pattern {
    // fnmatch has no `**`; a run of `*` is one `*`.
    let mut collapsed = String::with_capacity(pattern.len());
    for c in pattern.chars() {
        if !(c == '*' && collapsed.ends_with('*')) {
            collapsed.push(c);
        }
    }
    glob::Pattern::new(&collapsed)
        .unwrap_or_else(|_| glob::Pattern::new(&glob::Pattern::escape(pattern)).unwrap())
}

const FNMATCH: glob::MatchOptions = glob::MatchOptions {
    case_sensitive: true,
    require_literal_separator: false,
    require_literal_leading_dot: false,
};

impl InstallMask {
    /// Parse a whitespace-separated `INSTALL_MASK` value
    pub(crate) fn new(value: &str) -> Self {
        let words = value
            .split_whitespace()
            .filter_map(|word| {
                let (unmask, word) = match word.strip_prefix('-') {
                    Some(rest) => (true, rest),
                    None => (false, word),
                };
                let Some(path) = word.strip_prefix('/') else {
                    return (!word.is_empty()).then(|| Word {
                        unmask,
                        anchored: false,
                        dir_only: false,
                        glob: fnmatch_pattern(word),
                        below: None,
                    });
                };
                let dir_only = path.ends_with('/');
                let path = path.trim_end_matches('/');
                Some(Word {
                    unmask,
                    anchored: true,
                    dir_only,
                    glob: fnmatch_pattern(path),
                    below: Some(fnmatch_pattern(&format!("{path}/*"))),
                })
            })
            .collect();
        Self { words }
    }

    /// Whether the value holds no words
    pub(crate) fn is_empty(&self) -> bool {
        self.words.is_empty()
    }

    /// Whether `path` (relative to `${ED}`) is masked; the last matching word decides
    fn matches(&self, path: &str, is_dir: bool) -> bool {
        let name = path.rsplit('/').next().unwrap_or(path);
        let mut masked = false;
        for w in &self.words {
            let hit = if w.anchored {
                let exact = (is_dir || !w.dir_only) && w.glob.matches_with(path, FNMATCH);
                exact
                    || w.below
                        .as_ref()
                        .is_some_and(|b| b.matches_with(path, FNMATCH))
            } else {
                !is_dir && w.glob.matches_with(name, FNMATCH)
            };
            if hit {
                masked = !w.unmask;
            }
        }
        masked
    }

    /// Remove the masked paths under `ed`; a masked directory goes only once it is empty
    pub(crate) fn apply(&self, ed: &Path) -> std::io::Result<()> {
        if self.is_empty() || !ed.is_dir() {
            return Ok(());
        }
        self.apply_in(ed, "")
    }

    fn apply_in(&self, dir: &Path, rel: &str) -> std::io::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            let child = if rel.is_empty() {
                name
            } else {
                format!("{rel}/{name}")
            };
            if entry.file_type()?.is_dir() {
                self.apply_in(&entry.path(), &child)?;
                if self.matches(&child, true) {
                    let _ = std::fs::remove_dir(entry.path());
                }
            } else if self.matches(&child, false) {
                std::fs::remove_file(entry.path())?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchored_words_mask_the_path_and_below() {
        let m = InstallMask::new("/usr/share/doc /etc/foo*/ *.la");
        assert!(m.matches("usr/share/doc", true));
        assert!(m.matches("usr/share/doc/pkg/README", false));
        assert!(!m.matches("usr/share/docs", true));
        assert!(m.matches("etc/foo.d", true));
        assert!(!m.matches("etc/foo.conf", false));
        assert!(m.matches("etc/foo.d/x", false));
        assert!(m.matches("usr/lib64/libx.la", false));
        assert!(!m.matches("usr/lib64/libx.so", false));
    }

    #[test]
    fn later_unmask_word_wins() {
        let m = InstallMask::new("/usr/share/locale -/usr/share/locale/en*");
        assert!(m.matches("usr/share/locale/de/LC_MESSAGES/x.mo", false));
        assert!(!m.matches("usr/share/locale/en_GB/LC_MESSAGES/x.mo", false));
        let m = InstallMask::new("-/usr/share/locale/en* /usr/share/locale");
        assert!(m.matches("usr/share/locale/en_GB/x.mo", false));
    }

    #[test]
    fn apply_keeps_a_masked_dir_holding_unmasked_files() {
        let tmp = tempfile::tempdir().unwrap();
        let ed = tmp.path();
        for f in [
            "usr/share/doc/a/README",
            "usr/share/doc/en/x",
            "usr/bin/tool",
        ] {
            let p = ed.join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, "").unwrap();
        }
        InstallMask::new("/usr/share/doc -/usr/share/doc/en")
            .apply(ed)
            .unwrap();
        assert!(!ed.join("usr/share/doc/a").exists());
        assert!(ed.join("usr/share/doc/en/x").exists());
        assert!(ed.join("usr/bin/tool").exists());
    }
}
