use std::collections::HashMap;

use camino::{Utf8Path, Utf8PathBuf};
use portage_atom::interner::Interned;
use portage_atom::{Cpn, Cpv, Version};
use portage_atom_pubgrub::{CededFlag, PortagePackage, UseFlagState};
use portage_metadata::{RestrictExpr, SrcUriEntry};
use portage_repo::{Manifest, ManifestEntry};

use crate::effective_use::effective_use;
use crate::repo::{RepoData, ResolvePolicy, find_cache};

/// Per-package download size, in **bytes**, of the distfiles that are not
/// already present in `DISTDIR` — matching what `emerge -pv` totals as
/// "Size of downloads"
///
/// Distfiles are resolved from `SRC_URI` evaluated against each package's
/// effective USE (so USE-conditional sources are only counted when active) and
/// sized from the package's `Manifest`. A file present in `DISTDIR` at its
/// recorded size counts as zero (already fetched). `repo_path_of` names the
/// repository a package's ebuild (and so its Manifest) comes from.
pub fn compute(
    repo_path_of: &dyn Fn(&Cpv) -> Utf8PathBuf,
    distdir: &str,
    data: &RepoData,
    order: &[(PortagePackage, Version)],
    policy: &ResolvePolicy,
    ceded: &[CededFlag],
) -> HashMap<Cpv, u64> {
    let distdir = Utf8Path::new(distdir);
    // One Manifest parse per repo and CPN, reused across that package's versions.
    let mut manifests: HashMap<(Utf8PathBuf, Cpn), HashMap<String, u64>> = HashMap::new();
    // A distfile shared by several packages is fetched once: emerge counts it
    // against the first package in the list that needs it, and zero thereafter.
    let mut counted: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut sizes = HashMap::new();

    for (pkg, ver) in order {
        if pkg.is_virtual() {
            continue;
        }
        let Some(cache) = find_cache(data, pkg, ver) else {
            continue;
        };
        if cache.metadata.src_uri.is_empty_raw() {
            continue;
        }

        let cpv = Cpv::new(*pkg.cpn(), ver.clone());
        let repo = crate::repo::repo_name_of(data, &cpv);
        let stable = policy.accept_keywords.is_stable(
            &cache.metadata.keywords,
            &cpv,
            pkg.slot(),
            Some(repo),
        );
        let effective = effective_use(policy, pkg, ver, cache, repo, stable, ceded);
        let enabled = |flag: &str| -> bool {
            matches!(effective.get(Interned::intern(flag)), UseFlagState::Enabled)
        };

        let mut wanted: Vec<String> = Vec::new();
        for entry in cache.metadata.src_uri.list() {
            entry.collect_filenames(&enabled, &mut wanted);
        }
        wanted.sort();
        wanted.dedup();

        let manifest = manifests
            .entry((repo_path_of(&cpv), *pkg.cpn()))
            .or_insert_with_key(|(repo_path, cpn)| load_manifest_sizes(repo_path, cpn));

        let mut total = 0u64;
        for filename in &wanted {
            let Some(&size) = manifest.get(filename) else {
                continue;
            };
            let cached = distdir.join(filename);
            let present = std::fs::metadata(cached.as_std_path())
                .map(|m| m.len() == size)
                .unwrap_or(false);
            // Count a missing distfile once, against the first package needing it.
            if !present && counted.insert(filename.clone()) {
                total += size;
            }
        }
        sizes.insert(cpv, total);
    }

    sizes
}

/// `RESTRICT=fetch` state of each fetch-restricted package in `order`:
/// `true` when every restricted distfile is already in `DISTDIR` (emerge's `f`),
/// `false` when one is missing (`F`). Unrestricted packages are absent.
pub fn fetch_restricted(
    repo_path_of: &dyn Fn(&Cpv) -> Utf8PathBuf,
    distdir: &str,
    data: &RepoData,
    order: &[(PortagePackage, Version)],
    policy: &ResolvePolicy,
    ceded: &[CededFlag],
) -> HashMap<Cpv, bool> {
    let distdir = Utf8Path::new(distdir);
    let mut out = HashMap::new();
    for (pkg, ver) in order {
        if pkg.is_virtual() {
            continue;
        }
        let Some(cache) = find_cache(data, pkg, ver) else {
            continue;
        };
        if !RestrictExpr::flat_tokens(&cache.metadata.restrict).contains(&"fetch") {
            continue;
        }
        let cpv = Cpv::new(*pkg.cpn(), ver.clone());
        let repo = crate::repo::repo_name_of(data, &cpv);
        let stable = policy.accept_keywords.is_stable(
            &cache.metadata.keywords,
            &cpv,
            pkg.slot(),
            Some(repo),
        );
        let effective = effective_use(policy, pkg, ver, cache, repo, stable, ceded);
        let enabled = |flag: &str| -> bool {
            matches!(effective.get(Interned::intern(flag)), UseFlagState::Enabled)
        };
        if !RestrictExpr::active_tokens(&cache.metadata.restrict, &enabled).contains(&"fetch") {
            continue;
        }
        let mut restricted = Vec::new();
        for entry in cache.metadata.src_uri.list() {
            restricted_filenames(entry, &enabled, &mut restricted);
        }
        let manifest = load_manifest_sizes(&repo_path_of(&cpv), pkg.cpn());
        let satisfied = restricted.iter().all(|filename| {
            std::fs::metadata(distdir.join(filename).as_std_path())
                .is_ok_and(|m| manifest.get(filename).is_none_or(|&size| m.len() == size))
        });
        out.insert(cpv, satisfied);
    }
    out
}

/// The active distfiles of `entry` that `RESTRICT=fetch` covers: a `fetch+` or
/// `mirror+` URI is exempt (PMS 7.3.2).
fn restricted_filenames(
    entry: &SrcUriEntry,
    enabled: &dyn Fn(&str) -> bool,
    out: &mut Vec<String>,
) {
    match entry {
        SrcUriEntry::Uri {
            filename: name,
            restriction,
            ..
        }
        | SrcUriEntry::Renamed {
            target: name,
            restriction,
            ..
        } => {
            if !matches!(restriction.as_deref(), Some("fetch" | "mirror")) {
                out.push(name.clone());
            }
        }
        SrcUriEntry::UseConditional {
            flag,
            negated,
            entries,
        } => {
            if enabled(flag) != *negated {
                for e in entries {
                    restricted_filenames(e, enabled, out);
                }
            }
        }
        SrcUriEntry::Group(entries) => {
            for e in entries {
                restricted_filenames(e, enabled, out);
            }
        }
    }
}

/// Parse `<repo>/<cat>/<pkg>/Manifest` into a `filename -> size` map
fn load_manifest_sizes(repo_path: &Utf8Path, cpn: &Cpn) -> HashMap<String, u64> {
    let path = repo_path
        .join(cpn.category.as_str())
        .join(cpn.package.as_str())
        .join("Manifest");
    let Ok(content) = std::fs::read_to_string(path.as_std_path()) else {
        return HashMap::new();
    };
    let Ok(manifest) = Manifest::parse(&content) else {
        return HashMap::new();
    };
    manifest
        .dist_entries()
        .filter_map(|e| match e {
            ManifestEntry::Dist { filename, size, .. } => Some((filename.clone(), *size)),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use portage_metadata::CacheEntry;
    use portage_repo::{AcceptSet, LicenseGroupRegistry};

    use super::*;
    use crate::force_mask::ForceMask;
    use crate::repo::{AcceptKeywords, AcceptOverlay, PolicyMaskList};

    fn write_manifest(repo: &Utf8Path, size: u64) {
        let dir = repo.join("app-misc/foo");
        std::fs::create_dir_all(&dir).unwrap();
        let line = format!("DIST foo-1.tar.gz {size} BLAKE2B aa SHA512 bb\n");
        std::fs::write(dir.join("Manifest"), line).unwrap();
    }

    // `app-misc/foo-1` from `repo` with the given extra md5-cache lines
    fn foo(extra: &str, repo: &str) -> (Cpv, RepoData) {
        let cpv = Cpv::parse("app-misc/foo-1").unwrap();
        let text = format!(
            "EAPI=8\nSLOT=0\nKEYWORDS=amd64\nDESCRIPTION=t\nSRC_URI=https://x/foo-1.tar.gz\n{extra}"
        );
        let data = RepoData::from_parts(
            vec![cpv.cpn],
            HashMap::from([(
                cpv.cpn,
                vec![(cpv.clone(), CacheEntry::parse(&text).unwrap())],
            )]),
            "gentoo".into(),
            HashMap::from([(cpv.clone(), repo.into())]),
            HashMap::new(),
        );
        (cpv, data)
    }

    fn with_policy<R>(f: impl FnOnce(&ResolvePolicy) -> R) -> R {
        let accept_keywords =
            AcceptKeywords::from_global(&gentoo_core::Arch::intern("amd64"), &["amd64"]);
        let licenses = AcceptOverlay::new(
            AcceptSet::from_tokens(&["*".into()], &LicenseGroupRegistry::default()),
            Vec::new(),
        );
        let layer = portage_atom_pubgrub::UseLayer::default();
        let force_mask = ForceMask::default();
        f(&ResolvePolicy {
            accept_keywords: &accept_keywords,
            package_mask: &PolicyMaskList::EMPTY,
            package_unmask: &PolicyMaskList::EMPTY,
            accept_licenses: &licenses,
            accept_properties: &licenses,
            accept_restrict: &licenses,
            defaults: &layer,
            conf: &layer,
            env_use: &layer,
            package_use: &[],
            profile_package_use: &[],
            force_mask: &force_mask,
            facts: None,
        })
    }

    #[test]
    fn an_overlay_package_is_sized_from_its_own_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(tmp.path()).unwrap();
        let (main, overlay) = (root.join("main"), root.join("overlay"));
        write_manifest(&main, 1024);
        write_manifest(&overlay, 4096);
        let (cpv, data) = foo("", "overlay");
        let repo_path_of = |c: &Cpv| {
            if crate::repo::repo_name_of(&data, c).as_str() == "overlay" {
                overlay.clone()
            } else {
                main.clone()
            }
        };
        let order = [(PortagePackage::unslotted(cpv.cpn), cpv.version.clone())];
        let distdir = root.join("distfiles");
        let sizes = with_policy(|policy| {
            compute(&repo_path_of, distdir.as_str(), &data, &order, policy, &[])
        });
        assert_eq!(sizes.get(&cpv), Some(&4096));
    }

    #[test]
    fn fetch_restriction_is_satisfied_once_the_distfile_is_in_distdir() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(tmp.path()).unwrap();
        let repo = root.join("repo");
        let distdir = root.join("distfiles");
        std::fs::create_dir_all(&distdir).unwrap();
        write_manifest(&repo, 3);
        let repo_path_of = |_: &Cpv| repo.clone();
        let check = |extra: &str| {
            let (cpv, data) = foo(extra, "gentoo");
            let order = [(PortagePackage::unslotted(cpv.cpn), cpv.version.clone())];
            with_policy(|policy| {
                fetch_restricted(&repo_path_of, distdir.as_str(), &data, &order, policy, &[])
            })
            .get(&cpv)
            .copied()
        };
        assert_eq!(check(""), None);
        assert_eq!(check("RESTRICT=fetch\n"), Some(false));
        std::fs::write(distdir.join("foo-1.tar.gz"), "abc").unwrap();
        assert_eq!(check("RESTRICT=fetch\n"), Some(true));
    }
}
