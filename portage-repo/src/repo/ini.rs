//! Minimal single-level INI parsing shared by `repos.conf`/`binrepos.conf`-style
//! config files: `[section]` headers, `key = value` lines, `#`/`;` comments,
//! later files overriding earlier keys within the same section, and `[DEFAULT]` keys
//! inherited by every section. No value interpolation (`%(VAR)s`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::error::Result;
use crate::repo::util;

/// Resolve `path` to the files it contributes, in application order
///
/// A single file as itself, or (if a directory) every file beneath it whatever its name,
/// nested directories included, in Portage's recursive order (dotfiles, `~` backups and VCS
/// directories skipped). A missing path contributes nothing.
pub fn collect_conf_files(path: &Path) -> Result<Vec<PathBuf>> {
    util::list_config_files(path, util::ConfigFilesMode::Recursive)
}

/// Parse one file's contents into `sections` (accumulated across files —
/// later files override earlier keys within the same section) and `order`
/// (first-seen section order, `[DEFAULT]` excluded — callers that need
/// `DEFAULT` read it from `sections` directly).
pub fn merge_sections(
    sections: &mut HashMap<String, HashMap<String, String>>,
    order: &mut Vec<String>,
    contents: &str,
) {
    let mut current: String = "DEFAULT".to_string();
    for raw in contents.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(stripped) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            current = stripped.trim().to_string();
            if !sections.contains_key(&current) {
                if current != "DEFAULT" {
                    order.push(current.clone());
                }
                sections.insert(current.clone(), HashMap::new());
            }
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            sections
                .entry(current.clone())
                .or_default()
                .insert(k.trim().to_string(), v.trim().to_string());
        }
    }
}

/// Section `name`'s keys over the `[DEFAULT]` section's, as `ConfigParser` reads an option
pub fn section_with_defaults(
    sections: &HashMap<String, HashMap<String, String>>,
    name: &str,
) -> HashMap<String, String> {
    let mut out = sections.get("DEFAULT").cloned().unwrap_or_default();
    if let Some(s) = sections.get(name) {
        out.extend(s.iter().map(|(k, v)| (k.clone(), v.clone())));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn section_keys_override_inherited_defaults() {
        let mut sections = HashMap::new();
        let mut order = Vec::new();
        merge_sections(
            &mut sections,
            &mut order,
            "[DEFAULT]\nx = 1\ny = 2\n[a]\ny = 3\n",
        );
        let a = section_with_defaults(&sections, "a");
        assert_eq!((a["x"].as_str(), a["y"].as_str()), ("1", "3"));
    }

    #[test]
    fn merge_sections_tracks_first_seen_order_and_overrides() {
        let mut sections = HashMap::new();
        let mut order = Vec::new();
        merge_sections(&mut sections, &mut order, "[a]\nx = 1\n[b]\ny = 2\n");
        merge_sections(&mut sections, &mut order, "[a]\nx = 3\n");
        assert_eq!(order, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(sections["a"]["x"], "3");
        assert_eq!(sections["b"]["y"], "2");
    }

    #[test]
    fn merge_sections_skips_comments_and_blank_lines() {
        let mut sections = HashMap::new();
        let mut order = Vec::new();
        merge_sections(
            &mut sections,
            &mut order,
            "# comment\n\n[a]\n; also a comment\nx = 1\n",
        );
        assert_eq!(sections["a"]["x"], "1");
    }

    #[test]
    fn collect_conf_files_reads_every_fragment_nested_or_unsuffixed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repos.conf");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("gentoo"), "").unwrap();
        std::fs::write(root.join("sub/overlay.conf"), "").unwrap();
        std::fs::write(root.join("old~"), "").unwrap();
        let files = collect_conf_files(&root).unwrap();
        assert_eq!(files, [root.join("gentoo"), root.join("sub/overlay.conf")]);
    }
}
