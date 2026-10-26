use std::collections::BTreeMap;
use std::io::Write as _;

use anyhow::{Context as _, Result, bail};
use portage_atom::{Cpn, Cpv};
use portage_metadata::RawCacheEntry;
use portage_repo::{CacheReadOpts, Repository, cache_entries_parallel};

use crate::style::{C_BOLD, C_LABEL, C_MASKED, C_PKG, C_STAR};

/// Open every configured repo path, skipping (with a warning) any that fail
/// Errors only when no path is configured, or none opened.
fn open_repos(repo_paths: &[std::path::PathBuf]) -> Result<Vec<Repository>> {
    if repo_paths.is_empty() {
        bail!("no repositories configured");
    }
    let mut repos: Vec<Repository> = Vec::with_capacity(repo_paths.len());
    for p in repo_paths {
        match crate::repo_open::open(p) {
            Ok(r) => repos.push(r),
            Err(e) => crate::style::warn_line!("skipping {}: {e}", p.display()),
        }
    }
    if repos.is_empty() {
        bail!("no usable repositories");
    }
    Ok(repos)
}

pub async fn run(
    repo_paths: &[std::path::PathBuf],
    pattern: Option<&str>,
    all: bool,
    search_desc: bool,
    name_only: bool,
    homepage: bool,
) -> Result<()> {
    let repos = open_repos(repo_paths)?;
    let pat = pattern.unwrap_or("");

    if search_desc {
        run_desc(&repos, pat, all, name_only, homepage).await
    } else {
        run_name(&repos, pat, all, name_only, homepage);
        Ok(())
    }
}

fn contains_ci(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let hb = haystack.as_bytes();
    let nb = needle.as_bytes();
    if nb.len() > hb.len() {
        return false;
    }
    hb.windows(nb.len()).any(|w| w.eq_ignore_ascii_case(nb))
}

fn run_name(repos: &[Repository], pat: &str, all: bool, name_only: bool, homepage: bool) {
    let pat_has_slash = pat.contains('/');
    let mut matched: BTreeMap<String, (Cpn, usize)> = BTreeMap::new();
    for (idx, repo) in repos.iter().enumerate() {
        for cat in repo.categories() {
            for pkg in cat.packages() {
                let hit = if all {
                    true
                } else if pat_has_slash {
                    let full = format!("{}/{}", cat.name(), pkg.name());
                    contains_ci(&full, pat)
                } else {
                    contains_ci(pkg.name(), pat)
                };
                if hit {
                    let key = format!("{}/{}", cat.name(), pkg.name());
                    matched.entry(key).or_insert_with(|| (*pkg.cpn(), idx));
                }
            }
        }
    }

    let mut out = anstream::stdout();

    if name_only {
        for key in matched.keys() {
            writeln!(out, "{C_PKG}{key}{C_PKG:#}").ok();
        }
        return;
    }

    for (key, (cpn, idx)) in &matched {
        let info = latest_entry_info(&repos[*idx], cpn, homepage);
        if info.is_empty() {
            writeln!(out, "{C_PKG}{key}{C_PKG:#}").ok();
        } else {
            writeln!(out, "{C_PKG}{key}{C_PKG:#}: {info}").ok();
        }
    }
}

fn latest_entry_info(repo: &Repository, cpn: &Cpn, homepage: bool) -> String {
    let Some(cat) = repo.category(cpn.category.as_str()) else {
        return String::new();
    };
    let Some(pkg) = cat.package(cpn.package.as_str()) else {
        return String::new();
    };
    let Ok(ebuilds) = pkg.ebuilds() else {
        return String::new();
    };
    let Some(latest) = ebuilds.last() else {
        return String::new();
    };
    match repo.cache_entry(latest.cpv()).ok().flatten() {
        Some(entry) if homepage => entry.metadata.homepage.join(" "),
        Some(entry) => entry.metadata.description,
        None => String::new(),
    }
}

async fn run_desc(
    repos: &[Repository],
    pat: &str,
    all: bool,
    name_only: bool,
    homepage: bool,
) -> Result<()> {
    let opts = CacheReadOpts {
        latest_per_cpn: true,
        ..Default::default()
    };

    let pat_owned = pat.to_string();
    let mut entries: Vec<(Cpv, Option<String>)> =
        cache_entries_parallel(repos, &opts, move |text| {
            let raw = RawCacheEntry::new(text);
            let desc = raw.field("DESCRIPTION").unwrap_or("");
            if !all && !contains_ci(desc, &pat_owned) {
                return Ok(None);
            }
            let info = if name_only {
                None
            } else if homepage {
                Some(raw.field("HOMEPAGE").unwrap_or("").to_string())
            } else {
                Some(desc.to_string())
            };
            Ok::<_, portage_repo::Error>(Some(info))
        })
        .await?
        .into_iter()
        .filter_map(|(cpv, r)| r.ok().flatten().map(|info| (cpv, info)))
        .collect();

    entries.sort_by(|(a, _), (b, _)| {
        a.cpn
            .category
            .cmp(&b.cpn.category)
            .then_with(|| a.cpn.package.cmp(&b.cpn.package))
    });

    let mut out = anstream::stdout();
    for (cpv, info) in &entries {
        let key = format!("{}/{}", cpv.cpn.category, cpv.cpn.package);
        match info {
            None => writeln!(out, "{C_PKG}{key}{C_PKG:#}").ok(),
            Some(s) => writeln!(out, "{C_PKG}{key}{C_PKG:#}: {s}").ok(),
        };
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// emerge -s / -S style search
// ---------------------------------------------------------------------------

/// One `-s`/`-S` search key, read as emerge reads it: a leading `%` makes it a regular
/// expression, a leading `@` or any `/` matches it against `category/package`
#[derive(Debug, Clone)]
struct SearchKey {
    re: regex::Regex,
    category: bool,
}

impl SearchKey {
    /// `pat` as a key; with `regex_auto`, one holding regex metacharacters that compiles is a
    /// regular expression too
    fn parse(pat: &str, regex_auto: bool) -> Result<Self> {
        let (key, explicit) = match pat.strip_prefix('%') {
            Some(key) => (key, true),
            None => (pat, false),
        };
        let (key, at) = match key.strip_prefix('@') {
            Some(key) => (key, true),
            None => (key, false),
        };
        let build = |p: &str| regex::RegexBuilder::new(p).case_insensitive(true).build();
        let looks_like_regex =
            key.contains(['^', '$', '*', '[', ']', '{', '}', '|', '?']) || key.contains(".+");
        let re = if explicit {
            build(key)?
        } else {
            match (regex_auto && looks_like_regex).then(|| build(key)) {
                Some(Ok(re)) => re,
                _ => build(&regex::escape(key))?,
            }
        };
        Ok(Self {
            re,
            category: at || key.contains('/'),
        })
    }

    /// Whether the package `category/name` matches
    fn matches_package(&self, category: &str, name: &str) -> bool {
        if self.category {
            self.re.is_match(&format!("{category}/{name}"))
        } else {
            self.re.is_match(name)
        }
    }
}

/// Search with emerge's output format: one block per package with latest
/// available/installed versions, distfile size, homepage, description and
/// license, bracketed by the search-key header and the result count.
pub async fn run_emerge_style(
    cli: &crate::cli::Cli,
    patterns: &[String],
    search_desc: bool,
    regex_auto: bool,
) -> Result<()> {
    let repos = open_repos(&cli.search_repos())?;
    let keys = patterns
        .iter()
        .map(|p| SearchKey::parse(p, regex_auto))
        .collect::<Result<Vec<_>>>()?;

    let roots = cli.roots();
    let set = portage_repo::RepoSet::from_ordered(
        repos.iter().cloned().map(std::sync::Arc::new).collect(),
        0,
        Vec::new(),
    );
    let use_env = portage_resolve::use_env::build_use_env(
        &set,
        roots.config(),
        roots.config_overlay(),
        None,
        None,
    )
    .await
    .context("resolving the profile for search visibility")?;
    let (policy, _) = portage_resolve::repo::ResolvedPolicy::from_use_env(use_env, &cli.arch());
    let policy = policy.as_policy();
    let accepts = |cpv: &Cpv, entry: &portage_metadata::CacheEntry, repo: &Repository| {
        policy.version_accepted(cpv, entry, repo.name())
    };

    // Installed versions (best per cpn) from the VDB.
    let mut installed: BTreeMap<Cpn, portage_atom::Version> = BTreeMap::new();
    if let Ok(vdb) = crate::vdb::open_cli_vdb(cli) {
        for pkg in vdb.packages() {
            let cpv = pkg.cpv();
            installed
                .entry(cpv.cpn)
                .and_modify(|v| {
                    if cpv.version > *v {
                        *v = cpv.version.clone();
                    }
                })
                .or_insert_with(|| cpv.version.clone());
        }
    }

    for (pat, key) in patterns.iter().zip(&keys) {
        let mut out = anstream::stdout();
        writeln!(
            out,
            "\n[ Results for search key : {C_BOLD}{pat}{C_BOLD:#} ]"
        )
        .ok();
        writeln!(out, "Searching...\n").ok();

        // Name matches from a category walk over every repo. For repos
        // without an md5-cache (overlays), the description is matched here
        // too — the parallel cache pass below cannot see them.
        let mut matched: BTreeMap<String, (Cpn, usize)> = BTreeMap::new();
        for (idx, repo) in repos.iter().enumerate() {
            let has_cache = repo.has_primary_cache();
            for cat in repo.categories() {
                for pkg in cat.packages() {
                    let mut hit = key.matches_package(cat.name(), pkg.name());
                    if !hit && search_desc && !has_cache {
                        hit = latest_visible(&repos, idx, pkg.cpn(), &accepts)
                            .is_some_and(|(_, e, _)| key.re.is_match(&e.metadata.description));
                    }
                    if hit {
                        matched
                            .entry(format!("{}/{}", cat.name(), pkg.name()))
                            .or_insert_with(|| (*pkg.cpn(), idx));
                    }
                }
            }
        }

        // -S: also match descriptions (md5-cache pass over all repos).
        if search_desc {
            let opts = CacheReadOpts {
                latest_per_cpn: true,
                ..Default::default()
            };
            let re = key.re.clone();
            let desc_hits = cache_entries_parallel(&repos, &opts, move |text| {
                let raw = RawCacheEntry::new(text);
                Ok::<_, portage_repo::Error>(re.is_match(raw.field("DESCRIPTION").unwrap_or("")))
            })
            .await?;
            for (cpv, hit) in desc_hits {
                if hit.unwrap_or(false) {
                    let key = format!("{}/{}", cpv.cpn.category, cpv.cpn.package);
                    if let Some(idx) = repo_providing(&repos, &cpv.cpn) {
                        matched.entry(key).or_insert((cpv.cpn, idx));
                    }
                }
            }
        }

        for (key, (cpn, idx)) in &matched {
            print_search_block(&repos, *idx, cpn, key, &installed, &accepts);
        }
        println!("[ Applications found : {} ]\n", matched.len());
    }
    Ok(())
}

/// Whether a version from a repo is visible on the searched root
type Accepts<'a> = dyn Fn(&Cpv, &portage_metadata::CacheEntry, &Repository) -> bool + 'a;

/// The first of `repos` that has `cpn`
fn repo_providing(repos: &[Repository], cpn: &Cpn) -> Option<usize> {
    repos.iter().position(|r| {
        r.category(cpn.category.as_str())
            .is_some_and(|c| c.package(cpn.package.as_str()).is_some())
    })
}

/// The latest version `accepts` lets through, with its metadata and a visibility flag
///
/// The bool is `false` when no version is accepted and the entry is the newest one with
/// metadata instead (the package appears as "\[ Masked \]" in emerge-style search output).
fn latest_visible(
    repos: &[Repository],
    idx: usize,
    cpn: &Cpn,
    accepts: &Accepts<'_>,
) -> Option<(portage_atom::Version, portage_metadata::CacheEntry, bool)> {
    let repo = &repos[idx];
    let cat = repo.category(cpn.category.as_str())?;
    let pkg = cat.package(cpn.package.as_str())?;
    let ebuilds = pkg.ebuilds().ok()?;
    let mut newest: Option<(portage_atom::Version, portage_metadata::CacheEntry, bool)> = None;
    for eb in ebuilds.iter().rev() {
        let Some(entry) = entry_for(repos, idx, eb) else {
            continue;
        };
        if newest.is_none() {
            newest = Some((eb.cpv().version.clone(), entry.clone(), false));
        }
        if accepts(eb.cpv(), &entry, repo) {
            return Some((eb.cpv().version.clone(), entry, true));
        }
    }
    newest
}

/// Metadata for one ebuild: the repo's own md5-cache, the user-side sourced
/// cache, or — for a symlinked ebuild (crossdev) — the md5-cache of the repo
/// the link resolves into.
fn entry_for(
    repos: &[Repository],
    idx: usize,
    ebuild: &portage_repo::Ebuild,
) -> Option<portage_metadata::CacheEntry> {
    let repo = &repos[idx];
    let cpv = ebuild.cpv();
    // Primary then secondary (configured at open via builder).
    if let Ok(Some(entry)) = repo.cache_entry(cpv) {
        return Some(entry);
    }
    // Symlinked ebuild: the target repo's cache entry is byte-exact.
    let real = ebuild.path().canonicalize_utf8().ok()?;
    for other in repos {
        let Ok(rel) = real.strip_prefix(other.path()) else {
            continue;
        };
        let category = rel.components().next()?.as_str();
        let cache_file = other
            .path()
            .join("metadata/md5-cache")
            .join(category)
            .join(rel.file_stem()?);
        if let Ok(text) = std::fs::read_to_string(&cache_file)
            && let Ok(entry) = portage_metadata::CacheEntry::parse(&text)
        {
            return Some(entry);
        }
    }
    None
}

fn print_search_block(
    repos: &[Repository],
    idx: usize,
    cpn: &Cpn,
    key: &str,
    installed: &BTreeMap<Cpn, portage_atom::Version>,
    accepts: &Accepts<'_>,
) {
    let latest = latest_visible(repos, idx, cpn, accepts);

    let is_masked = latest.as_ref().is_some_and(|(_, _, vis)| !vis);

    let mut out = anstream::stdout();
    write!(out, "{C_STAR}*{C_STAR:#}  {C_BOLD}{key}{C_BOLD:#}").ok();
    if is_masked {
        write!(out, " {C_MASKED}[ Masked ]{C_MASKED:#}").ok();
    }
    writeln!(out).ok();

    match &latest {
        Some((ver, _, _)) => writeln!(
            out,
            "      {C_LABEL}Latest version available:{C_LABEL:#} {ver}"
        )
        .ok(),
        None => writeln!(
            out,
            "      {C_LABEL}Latest version available:{C_LABEL:#} [ Unknown ]"
        )
        .ok(),
    };
    match installed.get(cpn) {
        Some(v) => writeln!(
            out,
            "      {C_LABEL}Latest version installed:{C_LABEL:#} {v}"
        )
        .ok(),
        None => writeln!(
            out,
            "      {C_LABEL}Latest version installed:{C_LABEL:#} [ Not Installed ]"
        )
        .ok(),
    };
    if let Some((_, entry, _)) = &latest {
        let size = distfiles_size(&repos[idx], cpn, &entry.metadata);
        writeln!(
            out,
            "      {C_LABEL}Size of files:{C_LABEL:#} {} KiB",
            size.div_ceil(1024)
        )
        .ok();
        writeln!(
            out,
            "      {C_LABEL}Homepage:{C_LABEL:#}      {}",
            entry.metadata.homepage.join(" ")
        )
        .ok();
        writeln!(
            out,
            "      {C_LABEL}Description:{C_LABEL:#}   {}",
            entry.metadata.description
        )
        .ok();
        let license = entry
            .metadata
            .license
            .as_ref()
            .map(|l| l.to_string())
            .unwrap_or_default();
        writeln!(out, "      {C_LABEL}License:{C_LABEL:#}       {license}").ok();
    }
    writeln!(out).ok();
}

/// Sum of the package's `Manifest` DIST sizes for every file the latest
/// version's `SRC_URI` can reference (all USE conditionals taken, as emerge's
/// search does).
fn distfiles_size(repo: &Repository, cpn: &Cpn, meta: &portage_metadata::EbuildMetadata) -> u64 {
    let manifest_path = repo
        .path()
        .join(cpn.category.as_str())
        .join(cpn.package.as_str())
        .join("Manifest");
    let Ok(content) = std::fs::read_to_string(&manifest_path) else {
        return 0;
    };
    let Ok(manifest) = portage_repo::Manifest::parse(&content) else {
        return 0;
    };
    let sizes: BTreeMap<String, u64> = manifest
        .dist_entries()
        .filter_map(|e| match e {
            portage_repo::ManifestEntry::Dist { filename, size, .. } => {
                Some((filename.clone(), *size))
            }
            _ => None,
        })
        .collect();
    let mut filenames = Vec::new();
    for entry in meta.src_uri.list() {
        entry.collect_filenames(&|_| true, &mut filenames);
    }
    filenames.sort();
    filenames.dedup();
    filenames
        .into_iter()
        .filter_map(|f| sizes.get(&f).copied())
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_keys_read_regex_and_category_markers_as_emerge_does() {
        let key = |p: &str| SearchKey::parse(p, true).unwrap();
        assert!(key("^gcc$").matches_package("sys-devel", "gcc"));
        assert!(!key("^gcc$").matches_package("sys-devel", "gcc-config"));
        assert!(key("GCC").matches_package("sys-devel", "gcc-config"));
        assert!(key("@sys-devel/gcc").matches_package("sys-devel", "gcc"));
        assert!(!key("@sys-devel/gcc").matches_package("dev-util", "gcc"));
        assert!(key("%gc+").matches_package("sys-devel", "gcc"));
        assert!(!key("%^g.c$").matches_package("sys-devel", "gcc-config"));
        let plain = SearchKey::parse("^gcc$", false).unwrap();
        assert!(!plain.matches_package("sys-devel", "gcc"));
        assert!(
            key("g[cc").matches_package("dev-util", "g[cc"),
            "a bad regex is text"
        );
        assert!(SearchKey::parse("%(", true).is_err());
    }

    #[test]
    fn latest_available_is_the_newest_version_the_policy_accepts() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("metadata")).unwrap();
        std::fs::write(root.join("metadata/layout.conf"), "").unwrap();
        std::fs::create_dir_all(root.join("profiles")).unwrap();
        std::fs::write(root.join("profiles/repo_name"), "test\n").unwrap();
        std::fs::write(root.join("profiles/categories"), "app-misc\n").unwrap();
        std::fs::create_dir_all(root.join("app-misc/foo")).unwrap();
        let repo = Repository::builder().in_memory_cache().open(root).unwrap();
        for (ver, keywords) in [("1", "amd64"), ("2", "~amd64")] {
            std::fs::write(
                root.join(format!("app-misc/foo/foo-{ver}.ebuild")),
                "EAPI=8\nSLOT=0\n",
            )
            .unwrap();
            let text = format!("EAPI=8\nDESCRIPTION=d\nSLOT=0\nKEYWORDS={keywords}\n");
            let entry = portage_metadata::CacheEntry::parse(&text).unwrap();
            let cpv = Cpv::parse(&format!("app-misc/foo-{ver}")).unwrap();
            repo.write_cache_entry(&cpv, &entry).unwrap();
        }
        let repos = [repo];
        let cpn = Cpn::parse("app-misc/foo").unwrap();
        assert_eq!(repo_providing(&repos, &cpn), Some(0));
        let arch = gentoo_core::Arch::intern("amd64");
        let latest = |accept: &[&str]| {
            let keywords = portage_resolve::repo::AcceptKeywords::from_global(&arch, accept);
            let accepts = |cpv: &Cpv, e: &portage_metadata::CacheEntry, _: &Repository| {
                keywords.accepts(&e.metadata.keywords, cpv, Some(e.metadata.slot.slot))
            };
            let (ver, _, visible) = latest_visible(&repos, 0, &cpn, &accepts).unwrap();
            (ver.to_string(), visible)
        };
        assert_eq!(latest(&["amd64"]), ("1".to_string(), true));
        assert_eq!(latest(&["amd64", "~amd64"]), ("2".to_string(), true));
        assert_eq!(latest(&["arm64"]), ("2".to_string(), false));
    }
}
