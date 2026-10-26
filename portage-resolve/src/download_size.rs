use std::collections::HashMap;

use camino::{Utf8Path, Utf8PathBuf};
use portage_atom::interner::Interned;
use portage_atom::{Cpn, Cpv, Version};
use portage_atom_pubgrub::{CededFlag, PortagePackage, UseFlagState};
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

    #[test]
    fn an_overlay_package_is_sized_from_its_own_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(tmp.path()).unwrap();
        let (main, overlay) = (root.join("main"), root.join("overlay"));
        write_manifest(&main, 1024);
        write_manifest(&overlay, 4096);

        let cpv = Cpv::parse("app-misc/foo-1").unwrap();
        let entry = CacheEntry::parse(
            "EAPI=8\nSLOT=0\nKEYWORDS=amd64\nDESCRIPTION=t\nSRC_URI=https://x/foo-1.tar.gz\n",
        )
        .unwrap();
        let data = RepoData::from_parts(
            vec![cpv.cpn],
            HashMap::from([(cpv.cpn, vec![(cpv.clone(), entry)])]),
            "gentoo".into(),
            HashMap::from([(cpv.clone(), "overlay".into())]),
            HashMap::new(),
        );

        let accept_keywords =
            AcceptKeywords::from_global(&gentoo_core::Arch::intern("amd64"), &["amd64"]);
        let licenses = AcceptOverlay::new(
            AcceptSet::from_tokens(&["*".into()], &LicenseGroupRegistry::default()),
            Vec::new(),
        );
        let layer = portage_atom_pubgrub::UseLayer::default();
        let force_mask = ForceMask::default();
        let policy = ResolvePolicy {
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
        };

        let repo_path_of = |c: &Cpv| {
            if crate::repo::repo_name_of(&data, c).as_str() == "overlay" {
                overlay.clone()
            } else {
                main.clone()
            }
        };
        let order = [(PortagePackage::unslotted(cpv.cpn), cpv.version.clone())];
        let distdir = root.join("distfiles");
        let sizes = compute(&repo_path_of, distdir.as_str(), &data, &order, &policy, &[]);
        assert_eq!(sizes.get(&cpv), Some(&4096));
    }
}
