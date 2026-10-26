use std::collections::{HashMap, HashSet};

use blake2::{Blake2b512, Digest};
use portage_metadata::{RestrictExpr, SrcUriEntry};
use portage_repo::Repository;

use crate::error::{Error, Result};

/// Append the GLEP 75 filename-hash subdir, then the legacy flat path, to a
/// URL that already points at a mirror's distfiles root.
///
/// `distfiles.gentoo.org`'s `layout.conf` is `filename-hash BLAKE2B 8`: the file
/// lives under `<root>/<xx>/<filename>`, where `<xx>` is the first 8 bits (two
/// hex chars) of `BLAKE2B-512(filename)` — the hash of the *filename string*, not
/// the file content (GLEP 75; matches portage's `FilenameHashLayout`). The old
/// flat `<root>/<filename>` path now 404s on the official mirrors and their
/// regional copies alike, since they rsync the same on-disk layout.
fn hash_layout_urls(distfiles_root: &str, filename: &str) -> Vec<String> {
    let root = distfiles_root.trim_end_matches('/');
    let sub = format!("{:02x}", Blake2b512::digest(filename.as_bytes())[0]);
    vec![
        format!("{root}/{sub}/{filename}"),
        format!("{root}/{filename}"),
    ]
}

/// Candidate URLs for a distfile on a **bare** Gentoo mirror root, as
/// `GENTOO_MIRRORS` lists them (make.conf(5): e.g. `https://mirror.example/gentoo`,
/// with no `/distfiles` suffix — portage appends that itself).
fn gentoo_distfile_urls(mirror: &str, filename: &str) -> Vec<String> {
    let root = format!("{}/distfiles", mirror.trim_end_matches('/'));
    hash_layout_urls(&root, filename)
}

/// Candidate URLs for a distfile on a mirror root; an absolute path is a filesystem mirror,
/// which holds the files directly, with no `distfiles/` level
fn mirror_root_urls(mirror: &str, filename: &str) -> Vec<String> {
    if mirror.starts_with('/') {
        hash_layout_urls(&format!("file://{mirror}"), filename)
    } else {
        gentoo_distfile_urls(mirror, filename)
    }
}

fn push_unique(list: &mut Vec<String>, urls: impl IntoIterator<Item = String>) {
    for url in urls {
        if !list.contains(&url) {
            list.push(url);
        }
    }
}

/// Parse `/etc/portage/mirrors` (a file, or a directory of them): `name url...` per line
///
/// A later line for a name replaces an earlier one. A missing or unreadable path yields no mirrors.
pub fn read_custom_mirrors(path: &std::path::Path) -> Vec<(String, Vec<String>)> {
    let mut files = vec![path.to_path_buf()];
    if path.is_dir() {
        files = std::fs::read_dir(path)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| !n.starts_with('.') && !n.ends_with('~'))
            })
            .collect();
        files.sort();
    }
    let mut mirrors: Vec<(String, Vec<String>)> = Vec::new();
    for text in files.iter().filter_map(|f| std::fs::read_to_string(f).ok()) {
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("");
            let mut fields = line.split_whitespace();
            let Some(name) = fields.next() else {
                continue;
            };
            let urls: Vec<String> = fields.map(str::to_owned).collect();
            if urls.is_empty() {
                continue;
            }
            mirrors.retain(|(n, _)| n != name);
            mirrors.push((name.to_owned(), urls));
        }
    }
    mirrors
}

/// A fully resolved distfile: local filename + all candidate download URLs
///
/// URLs are in priority order — try each in turn until one succeeds.
#[derive(Debug, Clone)]
pub struct Distfile {
    /// The local filename to store in DISTDIR
    pub filename: String,
    /// Download URLs in priority order — GENTOO_MIRRORS first (mirrors-before-
    /// upstream, matching portage), then the expanded `mirror://`/upstream URLs.
    pub urls: Vec<String>,
    /// EAPI 8+ per-URI prefix (PMS 7.3.2): `Some("fetch")` for `fetch+`,
    /// `Some("mirror")` for `mirror+`. An **exemption** from package-level
    /// `RESTRICT=fetch` / `RESTRICT=mirror`, not a restriction on the URI.
    ///
    /// Also `Some("fetch")` when `RESTRICT=fetch` left `urls` holding only a
    /// `mirror://` URI's named mirrors, which portage still fetches from.
    pub restriction: Option<String>,
}

/// Package-level `RESTRICT` gate for [`DistfileResolver`] and the fetcher
///
/// A mirror builds it with [`Self::from_restrict`] (matchnone): a client-side
/// USE choice must never change what a mirror redistributes. A package fetch
/// uses [`Self::from_restrict_with_use`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RestrictGate {
    /// `RESTRICT=fetch` (unconditional) — drop every URI without a
    /// `fetch+`/`mirror+` prefix.
    pub fetch: bool,
    /// `RESTRICT=mirror`, or implied by `RESTRICT=fetch` — drop every URI
    /// without a `mirror+` prefix.
    pub mirror: bool,
    /// `RESTRICT=primaryuri`: try the ebuild's own URIs before `GENTOO_MIRRORS`
    pub primaryuri: bool,
}

impl RestrictGate {
    /// Build the gate from a parsed `RESTRICT` expression
    ///
    /// `RESTRICT=fetch` implies mirror-restriction too (real portage:
    /// `restrict_mirror = restrict_fetch or "mirror" in restrict`) — a
    /// `fetch+`-tagged URI is exempted from the fetch check only, so under
    /// plain `RESTRICT=fetch` it's still excluded from mirroring; only
    /// `mirror+` survives either.
    pub fn from_restrict(entries: &[RestrictExpr]) -> Self {
        let fetch = RestrictExpr::has_unconditional(entries, "fetch");
        let mirror = fetch || RestrictExpr::has_unconditional(entries, "mirror");
        let primaryuri = RestrictExpr::has_unconditional(entries, "primaryuri");
        Self {
            fetch,
            mirror,
            primaryuri,
        }
    }

    /// Build the gate from a parsed `RESTRICT` expression under the package's
    /// enabled USE flags, so `bindist? ( fetch )` applies when `bindist` is set
    pub fn from_restrict_with_use(entries: &[RestrictExpr], use_flags: &HashSet<String>) -> Self {
        let active = RestrictExpr::active_tokens(entries, &|f| use_flags.contains(f));
        let fetch = active.contains(&"fetch");
        let mirror = fetch || active.contains(&"mirror");
        Self {
            fetch,
            mirror,
            primaryuri: active.contains(&"primaryuri"),
        }
    }

    /// `fetch+` or `mirror+` exempts this URI from `RESTRICT=fetch` (PMS 7.3.2)
    pub fn uri_exempts_fetch(restriction: Option<&str>) -> bool {
        matches!(restriction, Some("fetch" | "mirror"))
    }

    /// `mirror+` exempts this URI from `RESTRICT=mirror` (PMS 7.3.2)
    pub fn uri_exempts_mirror(restriction: Option<&str>) -> bool {
        matches!(restriction, Some("mirror"))
    }
}

/// Options for [`DistfileResolver::resolve_uri_map`]
#[derive(Debug, Clone, Copy, Default)]
pub struct ResolveOpts {
    /// Append GENTOO_MIRRORS *after* the ebuild's own URIs, as a last-resort
    /// fallback.
    ///
    /// `false` is the right default for a mirror builder: it goes to
    /// upstream, not to a peer mirror — real emirrordist never falls back
    /// to GENTOO_MIRRORS at all. Named (and ordered) to match the
    /// `--gentoo-mirrors-fallback` CLI flag, unlike `resolve`/`resolve_all`'s
    /// unconditional mirrors-*first* behavior.
    pub gentoo_mirrors_fallback: bool,
    /// Package-level RESTRICT gate
    ///
    /// Default (`RestrictGate::default()`) is unrestricted.
    pub restrict: RestrictGate,
}

/// Resolves `SRC_URI` entries into concrete [`Distfile`]s
///
/// Expands `mirror://` URIs using the repository's `thirdpartymirrors` data
/// and appends GENTOO_MIRRORS as a fallback for every distfile.
pub struct DistfileResolver {
    /// Parsed `profiles/thirdpartymirrors`: mirror name → list of base URLs
    thirdparty: HashMap<String, Vec<String>>,
    /// GENTOO_MIRRORS — appended as final fallback for every distfile
    gentoo_mirrors: Vec<String>,
    /// `/etc/portage/mirrors`: mirror name to base URLs, tried before `thirdparty`'s;
    /// `local` lists mirror roots tried for every distfile
    custom: HashMap<String, Vec<String>>,
}

impl DistfileResolver {
    /// Build a resolver from explicit data (useful for testing)
    pub fn new(thirdparty: Vec<(String, Vec<String>)>, gentoo_mirrors: Vec<String>) -> Self {
        Self {
            thirdparty: thirdparty.into_iter().collect(),
            gentoo_mirrors,
            custom: HashMap::new(),
        }
    }

    /// Add the user's mirrors (see [`read_custom_mirrors`])
    pub fn with_custom_mirrors(mut self, custom: Vec<(String, Vec<String>)>) -> Self {
        self.custom = custom.into_iter().collect();
        self
    }

    /// Build a resolver from the `thirdpartymirrors` of every configured
    /// repository + a GENTOO_MIRRORS list
    ///
    /// `repos` is highest priority first. A mirror name's URLs stack across
    /// repos, and a later repo's `-url` or `-*` removes what came before, as in
    /// portage. `gentoo_mirrors` comes from `GENTOO_MIRRORS`, split on whitespace.
    pub fn from_repos(repos: &[&Repository], gentoo_mirrors: Vec<String>) -> Result<Self> {
        let mut stacked: Vec<(String, Vec<String>)> = Vec::new();
        for repo in repos {
            let mirrors = repo
                .thirdpartymirrors()
                .map_err(|e| Error::Manifest(e.to_string()))?;
            for (name, urls) in mirrors {
                let i = match stacked.iter().position(|(n, _)| *n == name) {
                    Some(i) => i,
                    None => {
                        stacked.push((name, Vec::new()));
                        stacked.len() - 1
                    }
                };
                let list = &mut stacked[i].1;
                for url in urls {
                    if url == "-*" {
                        list.clear();
                    } else if let Some(removed) = url.strip_prefix('-') {
                        list.retain(|u| u != removed);
                    } else if !list.contains(&url) {
                        list.push(url);
                    }
                }
            }
        }
        stacked.retain(|(_, urls)| !urls.is_empty());
        Ok(Self::new(stacked, gentoo_mirrors))
    }

    /// Resolve `SRC_URI` entries into distfiles given the active USE flags
    ///
    /// USE-conditional groups are evaluated; `mirror://` URIs are expanded.
    /// GENTOO_MIRRORS are prepended unless package `RESTRICT=mirror` applies
    /// and the URI is not `mirror+` (PMS 7.3.2).
    pub fn resolve(&self, entries: &[SrcUriEntry], use_flags: &HashSet<String>) -> Vec<Distfile> {
        self.resolve_with(entries, use_flags, RestrictGate::default())
    }

    /// [`Self::resolve`] with a package-level `RESTRICT` gate
    pub fn resolve_with(
        &self,
        entries: &[SrcUriEntry],
        use_flags: &HashSet<String>,
        gate: RestrictGate,
    ) -> Vec<Distfile> {
        let mut raw: Vec<(String, String, Option<String>)> = Vec::new();
        collect_uri_pairs(entries, use_flags, &mut raw);
        self.build_distfiles(raw, gate)
    }

    /// Resolve every `SRC_URI` entry regardless of USE conditionals —
    /// `-F`/`--fetch-all-uri`: fetch everything an ebuild could ever need,
    /// not just what the current USE selection asks for.
    pub fn resolve_all(&self, entries: &[SrcUriEntry]) -> Vec<Distfile> {
        self.resolve_all_with(entries, RestrictGate::default())
    }

    /// [`Self::resolve_all`] with a package-level `RESTRICT` gate
    pub fn resolve_all_with(&self, entries: &[SrcUriEntry], gate: RestrictGate) -> Vec<Distfile> {
        let mut raw: Vec<(String, String, Option<String>)> = Vec::new();
        collect_uri_pairs_all(entries, &mut raw);
        self.build_distfiles(raw, gate)
    }

    /// One [`Distfile`] per **filename** (URLs merged/deduped, in `SRC_URI`
    /// order, ignoring USE conditionals — like [`Self::resolve_all`]), gated
    /// by package-level `RESTRICT` via `opts.restrict`.
    ///
    /// Unlike [`Self::resolve`]/[`Self::resolve_all`] (one `Distfile`
    /// per URI — a bug for callers writing concurrently, using the
    /// per-URI prefix alone, currently inverted, see that field's
    /// docs), this does RESTRICT gating itself and returns
    /// `restriction: None`.
    ///
    /// Gating, per real portage: a `fetch+`/`mirror+` prefix is an
    /// *exemption* from `opts.restrict`, not a restriction.
    ///
    /// `opts.restrict.mirror` must already include the `RESTRICT=fetch`
    /// implication (`RestrictGate::from_restrict` does this) — under
    /// plain `RESTRICT=fetch`, only `mirror+` survives (`fetch+`
    /// exempts fetch only; fetch implies mirror too).
    pub fn resolve_uri_map(&self, entries: &[SrcUriEntry], opts: &ResolveOpts) -> Vec<Distfile> {
        let mut raw: Vec<(String, String, Option<String>)> = Vec::new();
        collect_uri_pairs_all(entries, &mut raw);

        let mut order: Vec<String> = Vec::new();
        let mut by_filename: HashMap<String, Vec<String>> = HashMap::new();

        for (url, filename, uri_restriction) in raw {
            let override_mirror = uri_restriction.as_deref() == Some("mirror");
            let override_fetch = override_mirror || uri_restriction.as_deref() == Some("fetch");
            if opts.restrict.fetch && !override_fetch {
                continue;
            }
            if opts.restrict.mirror && !override_mirror {
                continue;
            }

            let list = by_filename.entry(filename.clone()).or_insert_with(|| {
                order.push(filename.clone());
                Vec::new()
            });
            for candidate in self.expand_url(&url, &filename, true) {
                if !list.contains(&candidate) {
                    list.push(candidate);
                }
            }
        }

        if opts.gentoo_mirrors_fallback {
            for filename in &order {
                if let Some(list) = by_filename.get_mut(filename) {
                    for mirror in &self.gentoo_mirrors {
                        push_unique(list, mirror_root_urls(mirror, filename));
                    }
                }
            }
        }

        order
            .into_iter()
            .filter_map(|filename| {
                let urls = by_filename.remove(&filename)?;
                (!urls.is_empty()).then_some(Distfile {
                    filename,
                    urls,
                    restriction: None,
                })
            })
            .collect()
    }

    /// One [`Distfile`] per **filename**, merging every raw URI entry that
    /// resolves to it (URLs concatenated/deduped in `SRC_URI` order).
    ///
    /// Must never produce two `Distfile`s sharing a filename: concurrent
    /// fetchers key on the filename alone, and two "different" `Distfile`s
    /// for the same file race writing the same `DISTDIR` path.
    ///
    /// Filesystem mirrors come first, then `local` custom mirrors and `GENTOO_MIRRORS`, then
    /// the ebuild's own URIs; `RESTRICT=primaryuri` puts those URIs before the network mirrors.
    fn build_distfiles(
        &self,
        raw: Vec<(String, String, Option<String>)>,
        gate: RestrictGate,
    ) -> Vec<Distfile> {
        struct Sources {
            restriction: Option<String>,
            fs: Vec<String>,
            mirrors: Vec<String>,
            upstream: Vec<String>,
        }
        let (fs_mirrors, net_mirrors): (Vec<&String>, Vec<&String>) = self
            .custom
            .get("local")
            .into_iter()
            .flatten()
            .partition(|m| m.starts_with('/'));
        let (fs_gentoo, public): (Vec<&String>, Vec<&String>) =
            self.gentoo_mirrors.iter().partition(|m| m.starts_with('/'));

        let mut order: Vec<String> = Vec::new();
        let mut by_filename: HashMap<String, Sources> = HashMap::new();

        for (url, filename, restriction) in raw {
            // Skip GENTOO_MIRRORS when the package is `RESTRICT=mirror` and this URI is not
            // `mirror+`, and for `mirror://gentoo/`, whose expansion already holds them.
            let package_blocks_mirrors = (gate.mirror || gate.fetch)
                && !RestrictGate::uri_exempts_mirror(restriction.as_deref());
            let use_gentoo_mirrors =
                !package_blocks_mirrors && !url.starts_with("mirror://gentoo/");
            let fetch_blocked =
                gate.fetch && !RestrictGate::uri_exempts_fetch(restriction.as_deref());

            let src = by_filename.entry(filename.clone()).or_insert_with(|| {
                order.push(filename.clone());
                Sources {
                    restriction: restriction.clone(),
                    fs: Vec::new(),
                    mirrors: Vec::new(),
                    upstream: Vec::new(),
                }
            });
            // First exemption seen for this filename wins: once one contributing URI is
            // `fetch+`/`mirror+`, there is a legitimately fetchable source under RESTRICT.
            if src.restriction.is_none() {
                src.restriction = restriction;
            }
            for mirror in fs_mirrors.iter().chain(&fs_gentoo) {
                push_unique(&mut src.fs, mirror_root_urls(mirror, &filename));
            }
            for mirror in &net_mirrors {
                push_unique(&mut src.mirrors, mirror_root_urls(mirror, &filename));
            }
            // Under RESTRICT=fetch, portage still fetches from the user's own mirrors and a
            // `mirror://` URI's named mirrors, never from GENTOO_MIRRORS or a plain URI.
            if fetch_blocked {
                if url.starts_with("mirror://") {
                    push_unique(&mut src.upstream, self.expand_url(&url, &filename, false));
                }
                if !(src.fs.is_empty() && src.mirrors.is_empty() && src.upstream.is_empty()) {
                    src.restriction.get_or_insert_with(|| "fetch".to_owned());
                }
                continue;
            }
            if use_gentoo_mirrors {
                for mirror in &public {
                    push_unique(&mut src.mirrors, mirror_root_urls(mirror, &filename));
                }
            }
            push_unique(
                &mut src.upstream,
                self.expand_url(&url, &filename, !package_blocks_mirrors),
            );
        }

        order
            .into_iter()
            .filter_map(|filename| {
                let src = by_filename.remove(&filename)?;
                let (first, second) = if gate.primaryuri {
                    (src.upstream, src.mirrors)
                } else {
                    (src.mirrors, src.upstream)
                };
                let mut urls = src.fs;
                push_unique(&mut urls, first.into_iter().chain(second));
                Some(Distfile {
                    filename,
                    urls,
                    restriction: src.restriction,
                })
            })
            .collect()
    }

    /// Expand a single URL to one or more concrete download URLs
    ///
    /// `mirror://name/path` is expanded via the user's custom mirrors, then `profiles/thirdpartymirrors`
    /// (matching real emirrordist's `Config.mirrors`), except `gentoo`: its
    /// thirdpartymirrors bases and any `GENTOO_MIRRORS` entries all serve the
    /// GLEP 75 filename-hash layout, so they get [`hash_layout_urls`] instead
    /// of a flat path-append; `public_mirrors` adds `GENTOO_MIRRORS` there.
    /// Direct URLs are returned as-is.
    fn expand_url(&self, url: &str, filename: &str, public_mirrors: bool) -> Vec<String> {
        if let Some(rest) = url.strip_prefix("mirror://") {
            let (mirror_name, path) = rest.split_once('/').unwrap_or((rest, filename));
            if mirror_name == "gentoo" {
                let fname = path.rsplit('/').next().unwrap_or(path);
                let mut urls: Vec<String> = self
                    .custom
                    .get("gentoo")
                    .into_iter()
                    .flatten()
                    .map(|base| format!("{}/{path}", base.trim_end_matches('/')))
                    .collect();
                for root in self.thirdparty.get("gentoo").into_iter().flatten() {
                    push_unique(&mut urls, hash_layout_urls(root, fname));
                }
                for mirror in self.gentoo_mirrors.iter().filter(|_| public_mirrors) {
                    push_unique(&mut urls, mirror_root_urls(mirror, fname));
                }
                return urls;
            }
            // Flat path-append for every non-gentoo mirror; emirrordist's FetchTask does the same.
            let urls: Vec<String> = [&self.custom, &self.thirdparty]
                .into_iter()
                .filter_map(|map| map.get(mirror_name))
                .flatten()
                .map(|base| format!("{}/{path}", base.trim_end_matches('/')))
                .collect();
            if urls.is_empty() {
                tracing::warn!("No known mirror by the name: {mirror_name}");
            }
            urls
        } else {
            vec![url.to_owned()]
        }
    }
}

fn collect_uri_pairs(
    entries: &[SrcUriEntry],
    use_flags: &HashSet<String>,
    out: &mut Vec<(String, String, Option<String>)>,
) {
    for entry in entries {
        match entry {
            SrcUriEntry::Uri {
                url,
                filename,
                restriction,
            } => {
                out.push((url.clone(), filename.clone(), restriction.clone()));
            }
            SrcUriEntry::Renamed {
                url,
                target,
                restriction,
            } => {
                out.push((url.clone(), target.clone(), restriction.clone()));
            }
            SrcUriEntry::UseConditional {
                flag,
                negated,
                entries,
            } => {
                let active = use_flags.contains(flag.as_str());
                if active != *negated {
                    collect_uri_pairs(entries, use_flags, out);
                }
            }
            SrcUriEntry::Group(entries) => {
                collect_uri_pairs(entries, use_flags, out);
            }
        }
    }
}

/// Same as [`collect_uri_pairs`], but descends into every `UseConditional`
/// branch unconditionally instead of gating on `use_flags` — the union of
/// what every USE setting could ever request.
fn collect_uri_pairs_all(entries: &[SrcUriEntry], out: &mut Vec<(String, String, Option<String>)>) {
    for entry in entries {
        match entry {
            SrcUriEntry::Uri {
                url,
                filename,
                restriction,
            } => {
                out.push((url.clone(), filename.clone(), restriction.clone()));
            }
            SrcUriEntry::Renamed {
                url,
                target,
                restriction,
            } => {
                out.push((url.clone(), target.clone(), restriction.clone()));
            }
            SrcUriEntry::UseConditional { entries, .. } | SrcUriEntry::Group(entries) => {
                collect_uri_pairs_all(entries, out);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolver(gentoo_mirrors: &[&str]) -> DistfileResolver {
        DistfileResolver::new(
            vec![(
                "kde".to_owned(),
                vec!["https://mirrors.kde.org/".to_owned()],
            )],
            gentoo_mirrors.iter().map(|s| s.to_string()).collect(),
        )
    }

    #[test]
    fn gentoo_mirrors_tried_before_upstream() {
        // make.conf(5): GENTOO_MIRRORS "are used to download files before the
        // ones listed in the ebuild scripts" — mirrors-first, upstream last.
        let r = resolver(&["https://mirror.gentoo.org"]);
        let entries = SrcUriEntry::parse("https://example.com/foo-1.0.tar.gz").unwrap();
        let dfs = r.resolve(&entries, &HashSet::new());
        let mut expected = gentoo_distfile_urls("https://mirror.gentoo.org", "foo-1.0.tar.gz");
        expected.push("https://example.com/foo-1.0.tar.gz".to_owned());
        assert_eq!(dfs[0].urls, expected);
    }

    #[test]
    fn mirror_gentoo_uses_filename_hash_layout() {
        let r = resolver(&["https://mirror.gentoo.org"]);
        let entries = SrcUriEntry::parse("mirror://gentoo/subdir/foo-1.0.tar.gz").unwrap();
        let dfs = r.resolve(&entries, &HashSet::new());
        // Keyed on the filename component, hashed-first then flat — the historical
        // subdir is dropped (content-mirror layout ignores it).
        assert_eq!(
            dfs[0].urls,
            gentoo_distfile_urls("https://mirror.gentoo.org", "foo-1.0.tar.gz")
        );
    }

    #[test]
    fn gentoo_filename_hash_subdir_matches_portage() {
        // portage's FilenameHashLayout("BLAKE2B", "8"): first 2 hex of
        // BLAKE2B-512(filename) as the subdir.
        let urls = gentoo_distfile_urls("https://m", "psmisc-23.7.tar.xz");
        let sub = format!(
            "{:02x}",
            Blake2b512::digest("psmisc-23.7.tar.xz".as_bytes())[0]
        );
        assert_eq!(
            urls[0],
            format!("https://m/distfiles/{sub}/psmisc-23.7.tar.xz")
        );
        assert_eq!(urls[1], "https://m/distfiles/psmisc-23.7.tar.xz");
    }

    #[test]
    fn custom_mirrors_order_local_first_and_named_before_thirdparty() {
        let r = resolver(&["https://mirror.gentoo.org", "/srv/gentoo"]).with_custom_mirrors(vec![
            (
                "local".to_owned(),
                vec!["https://lan.example".to_owned(), "/srv/local".to_owned()],
            ),
            ("kde".to_owned(), vec!["https://kde.lan/".to_owned()]),
        ]);
        let entries = SrcUriEntry::parse("mirror://kde/stable/foo.tar.xz").unwrap();
        let dfs = r.resolve(&entries, &HashSet::new());
        let mut expected = mirror_root_urls("/srv/local", "foo.tar.xz");
        expected.extend(mirror_root_urls("/srv/gentoo", "foo.tar.xz"));
        expected.extend(gentoo_distfile_urls("https://lan.example", "foo.tar.xz"));
        expected.extend(gentoo_distfile_urls(
            "https://mirror.gentoo.org",
            "foo.tar.xz",
        ));
        expected.push("https://kde.lan/stable/foo.tar.xz".to_owned());
        expected.push("https://mirrors.kde.org/stable/foo.tar.xz".to_owned());
        assert_eq!(dfs[0].urls, expected);
        assert!(expected[0].starts_with("file:///srv/local/"));
        assert!(expected[3].starts_with("file:///srv/gentoo/"));
    }

    #[test]
    fn local_mirrors_survive_restrict_fetch() {
        let r = resolver(&["https://mirror.gentoo.org"]).with_custom_mirrors(vec![(
            "local".to_owned(),
            vec!["https://lan.example".to_owned()],
        )]);
        let entries = SrcUriEntry::parse("https://example.com/secret.tar.gz").unwrap();
        let gate = RestrictGate {
            fetch: true,
            mirror: true,
            primaryuri: false,
        };
        let dfs = r.resolve_with(&entries, &HashSet::new(), gate);
        assert_eq!(
            dfs[0].urls,
            gentoo_distfile_urls("https://lan.example", "secret.tar.gz")
        );
        assert!(RestrictGate::uri_exempts_fetch(
            dfs[0].restriction.as_deref()
        ));
    }

    #[test]
    fn restrict_primaryuri_tries_upstream_before_gentoo_mirrors() {
        let r = resolver(&["https://mirror.gentoo.org"]);
        let entries = SrcUriEntry::parse("https://example.com/foo-1.0.tar.gz").unwrap();
        let gate = RestrictGate {
            primaryuri: true,
            ..RestrictGate::default()
        };
        let dfs = r.resolve_with(&entries, &HashSet::new(), gate);
        let mut expected = vec!["https://example.com/foo-1.0.tar.gz".to_owned()];
        expected.extend(gentoo_distfile_urls(
            "https://mirror.gentoo.org",
            "foo-1.0.tar.gz",
        ));
        assert_eq!(dfs[0].urls, expected);
    }

    #[test]
    fn restrict_gate_reads_primaryuri() {
        let entries = RestrictExpr::parse("primaryuri").unwrap();
        assert!(RestrictGate::from_restrict(&entries).primaryuri);
        assert!(RestrictGate::from_restrict_with_use(&entries, &HashSet::new()).primaryuri);
    }

    #[test]
    fn read_custom_mirrors_lets_a_later_line_replace_a_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mirrors");
        std::fs::write(
            &path,
            "# comment\nlocal /srv/a\ngnu https://g.example\nlocal /srv/b # x\n",
        )
        .unwrap();
        assert_eq!(
            read_custom_mirrors(&path),
            [
                ("gnu".to_owned(), vec!["https://g.example".to_owned()]),
                ("local".to_owned(), vec!["/srv/b".to_owned()]),
            ]
        );
        assert!(read_custom_mirrors(&dir.path().join("absent")).is_empty());
    }

    #[test]
    fn package_restrict_mirror_suppresses_gentoo_fallback() {
        let r = resolver(&["https://mirror.gentoo.org"]);
        let entries = vec![SrcUriEntry::Renamed {
            url: "https://proprietary.example.com/secret.tar.gz".to_owned(),
            target: "secret.tar.gz".to_owned(),
            restriction: None,
        }];
        let gate = RestrictGate {
            fetch: false,
            mirror: true,
            primaryuri: false,
        };
        let dfs = r.resolve_with(&entries, &HashSet::new(), gate);
        assert_eq!(
            dfs[0].urls,
            ["https://proprietary.example.com/secret.tar.gz"]
        );
        assert_eq!(dfs[0].urls.len(), 1);
    }

    #[test]
    fn mirror_plus_exempts_package_restrict_mirror() {
        let r = resolver(&["https://mirror.gentoo.org"]);
        let entries = vec![SrcUriEntry::Renamed {
            url: "https://example.com/foo.tar.gz".to_owned(),
            target: "foo.tar.gz".to_owned(),
            restriction: Some("mirror".to_owned()),
        }];
        let gate = RestrictGate {
            fetch: false,
            mirror: true,
            primaryuri: false,
        };
        let dfs = r.resolve_with(&entries, &HashSet::new(), gate);
        assert!(
            dfs[0]
                .urls
                .iter()
                .any(|u| u.starts_with("https://mirror.gentoo.org/")),
            "mirror+ must still see GENTOO_MIRRORS under RESTRICT=mirror"
        );
    }

    #[test]
    fn thirdparty_mirror_expansion() {
        let r = resolver(&[]);
        let entries = SrcUriEntry::parse("mirror://kde/stable/frameworks/foo.tar.xz").unwrap();
        let dfs = r.resolve(&entries, &HashSet::new());
        assert_eq!(
            dfs[0].urls,
            ["https://mirrors.kde.org/stable/frameworks/foo.tar.xz"]
        );
    }

    // thirdpartymirrors' `gentoo` bases (already ending in `/distfiles`) take
    // priority, each hashed then flat; GENTOO_MIRRORS entries come after as
    // extra candidates, also hash-layout — never a flat path-append, since
    // every one of these hosts serves the GLEP 75 on-disk layout.
    #[test]
    fn mirror_gentoo_uses_filename_hash_layout_on_every_source() {
        let r = DistfileResolver::new(
            vec![(
                "gentoo".to_owned(),
                vec![
                    "https://distfiles.gentoo.org/distfiles".to_owned(),
                    "https://gentoo.osuosl.org/distfiles".to_owned(),
                ],
            )],
            vec!["https://peer-mirror.example".to_owned()],
        );
        let entries = SrcUriEntry::parse("mirror://gentoo/logsentry-1.1.1.tar.gz").unwrap();
        let dfs = r.resolve_uri_map(&entries, &ResolveOpts::default());
        assert_eq!(dfs.len(), 1);
        let mut expected = hash_layout_urls(
            "https://distfiles.gentoo.org/distfiles",
            "logsentry-1.1.1.tar.gz",
        );
        expected.extend(hash_layout_urls(
            "https://gentoo.osuosl.org/distfiles",
            "logsentry-1.1.1.tar.gz",
        ));
        expected.extend(gentoo_distfile_urls(
            "https://peer-mirror.example",
            "logsentry-1.1.1.tar.gz",
        ));
        assert_eq!(dfs[0].urls, expected);
    }

    // With no thirdpartymirrors `gentoo` key and empty GENTOO_MIRRORS
    // (mirrordist's default), `mirror://gentoo/` yields no URLs and the
    // file is dropped from the plan — same as an unknown mirror name.
    #[test]
    fn mirror_gentoo_with_no_thirdparty_and_empty_gentoo_mirrors_is_dropped() {
        let r = DistfileResolver::new(vec![], vec![]);
        let entries = SrcUriEntry::parse("mirror://gentoo/foo.tar.gz").unwrap();
        let dfs = r.resolve_uri_map(&entries, &ResolveOpts::default());
        assert!(dfs.is_empty());
    }

    #[test]
    fn resolve_skips_a_use_conditional_uri_when_the_flag_is_off() {
        let r = resolver(&[]);
        let entries = SrcUriEntry::parse(
            "https://example.com/base.tar.gz doc? ( https://example.com/doc.tar.gz )",
        )
        .unwrap();
        let dfs = r.resolve(&entries, &HashSet::new());
        let names: Vec<&str> = dfs.iter().map(|d| d.filename.as_str()).collect();
        assert_eq!(names, ["base.tar.gz"]);
    }

    #[test]
    fn resolve_all_ignores_use_conditionals_entirely() {
        // -F/--fetch-all-uri: every SRC_URI entry, regardless of USE — neither
        // an active nor an inactive flag gates it, unlike `resolve`.
        let r = resolver(&[]);
        let entries = SrcUriEntry::parse(
            "https://example.com/base.tar.gz doc? ( https://example.com/doc.tar.gz ) !static? ( https://example.com/dyn.tar.gz )",
        )
        .unwrap();
        let dfs = r.resolve_all(&entries);
        let mut names: Vec<&str> = dfs.iter().map(|d| d.filename.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, ["base.tar.gz", "doc.tar.gz", "dyn.tar.gz"]);
    }

    // Regression: `resolve`/`resolve_all` used to emit one `Distfile` per raw
    // SRC_URI token, so two URIs resolving to the same filename produced two
    // separate `Distfile`s sharing a DISTDIR destination — a concurrent
    // fetcher racing to write both corrupts the file. Must merge into one.
    #[test]
    fn resolve_all_merges_multiple_uris_for_one_filename() {
        let r = resolver(&[]);
        let entries =
            SrcUriEntry::parse("https://a.example.com/foo.tar.gz https://b.example.com/foo.tar.gz")
                .unwrap();
        let dfs = r.resolve_all(&entries);
        assert_eq!(dfs.len(), 1, "must not split into two Distfiles: {dfs:?}");
        assert_eq!(dfs[0].filename, "foo.tar.gz");
        assert_eq!(
            dfs[0].urls,
            [
                "https://a.example.com/foo.tar.gz",
                "https://b.example.com/foo.tar.gz"
            ]
        );
    }

    #[test]
    fn restrict_gate_from_restrict_fetch_implies_mirror() {
        let entries = RestrictExpr::parse("fetch").unwrap();
        let gate = RestrictGate::from_restrict(&entries);
        assert!(gate.fetch);
        assert!(gate.mirror);
    }

    #[test]
    fn restrict_gate_from_restrict_mirror_alone() {
        let entries = RestrictExpr::parse("mirror").unwrap();
        let gate = RestrictGate::from_restrict(&entries);
        assert!(!gate.fetch);
        assert!(gate.mirror);
    }

    #[test]
    fn restrict_gate_from_restrict_none() {
        let entries = RestrictExpr::parse("test").unwrap();
        let gate = RestrictGate::from_restrict(&entries);
        assert!(!gate.fetch);
        assert!(!gate.mirror);
    }

    #[test]
    fn resolve_uri_map_merges_multiple_uris_for_one_filename() {
        let r = resolver(&[]);
        let entries =
            SrcUriEntry::parse("https://a.example.com/foo.tar.gz https://b.example.com/foo.tar.gz")
                .unwrap();
        let dfs = r.resolve_uri_map(&entries, &ResolveOpts::default());
        assert_eq!(dfs.len(), 1);
        assert_eq!(dfs[0].filename, "foo.tar.gz");
        assert_eq!(
            dfs[0].urls,
            [
                "https://a.example.com/foo.tar.gz",
                "https://b.example.com/foo.tar.gz"
            ]
        );
        assert_eq!(dfs[0].restriction, None);
    }

    #[test]
    fn resolve_uri_map_dedupes_identical_urls() {
        let r = resolver(&[]);
        // Same URL appearing via two SRC_URI tokens (e.g. re-listed in a
        // USE-conditional branch that resolve_all also descends into).
        let entries = SrcUriEntry::parse(
            "https://a.example.com/foo.tar.gz doc? ( https://a.example.com/foo.tar.gz )",
        )
        .unwrap();
        let dfs = r.resolve_uri_map(&entries, &ResolveOpts::default());
        assert_eq!(dfs.len(), 1);
        assert_eq!(dfs[0].urls, ["https://a.example.com/foo.tar.gz"]);
    }

    #[test]
    fn resolve_uri_map_gentoo_mirrors_fallback_off_by_default() {
        let r = resolver(&["https://mirror.gentoo.org"]);
        let entries = SrcUriEntry::parse("https://example.com/foo.tar.gz").unwrap();
        let dfs = r.resolve_uri_map(&entries, &ResolveOpts::default());
        assert_eq!(dfs[0].urls, ["https://example.com/foo.tar.gz"]);
    }

    #[test]
    fn resolve_uri_map_gentoo_mirrors_fallback_appends_last() {
        let r = resolver(&["https://mirror.gentoo.org"]);
        let entries = SrcUriEntry::parse("https://example.com/foo.tar.gz").unwrap();
        let opts = ResolveOpts {
            gentoo_mirrors_fallback: true,
            ..Default::default()
        };
        let dfs = r.resolve_uri_map(&entries, &opts);
        let mut expected = vec!["https://example.com/foo.tar.gz".to_string()];
        expected.extend(gentoo_distfile_urls(
            "https://mirror.gentoo.org",
            "foo.tar.gz",
        ));
        assert_eq!(dfs[0].urls, expected);
    }

    // The real-portage regression case: `RESTRICT=fetch` + a `fetch+`-tagged
    // URI is still excluded — `fetch+` only exempts the fetch check, and
    // `RESTRICT=fetch` implies the mirror check too, which nothing exempts
    // it from.
    #[test]
    fn resolve_uri_map_restrict_fetch_with_fetch_plus_override_is_still_excluded() {
        let r = resolver(&[]);
        let entries = vec![SrcUriEntry::Uri {
            url: "https://example.com/foo.tar.gz".to_owned(),
            filename: "foo.tar.gz".to_owned(),
            restriction: Some("fetch".to_owned()),
        }];
        let opts = ResolveOpts {
            restrict: RestrictGate {
                fetch: true,
                mirror: true,
                primaryuri: false,
            },
            ..Default::default()
        };
        let dfs = r.resolve_uri_map(&entries, &opts);
        assert!(dfs.is_empty());
    }

    // Only a `mirror+`-tagged URI survives `RESTRICT=fetch`.
    #[test]
    fn resolve_uri_map_restrict_fetch_with_mirror_plus_override_is_included() {
        let r = resolver(&[]);
        let entries = vec![SrcUriEntry::Uri {
            url: "https://example.com/foo.tar.gz".to_owned(),
            filename: "foo.tar.gz".to_owned(),
            restriction: Some("mirror".to_owned()),
        }];
        let opts = ResolveOpts {
            restrict: RestrictGate {
                fetch: true,
                mirror: true,
                primaryuri: false,
            },
            ..Default::default()
        };
        let dfs = r.resolve_uri_map(&entries, &opts);
        assert_eq!(dfs.len(), 1);
        assert_eq!(dfs[0].urls, ["https://example.com/foo.tar.gz"]);
    }

    #[test]
    fn resolve_uri_map_plain_restrict_mirror_excludes_unoverridden_uri() {
        let r = resolver(&[]);
        let entries = vec![SrcUriEntry::Uri {
            url: "https://example.com/foo.tar.gz".to_owned(),
            filename: "foo.tar.gz".to_owned(),
            restriction: None,
        }];
        let opts = ResolveOpts {
            restrict: RestrictGate {
                fetch: false,
                mirror: true,
                primaryuri: false,
            },
            ..Default::default()
        };
        let dfs = r.resolve_uri_map(&entries, &opts);
        assert!(dfs.is_empty());
    }

    fn repo_with_mirrors(dir: &std::path::Path, name: &str, mirrors: &str) -> Repository {
        std::fs::create_dir_all(dir.join("profiles")).unwrap();
        std::fs::create_dir_all(dir.join("metadata")).unwrap();
        std::fs::write(dir.join("profiles/repo_name"), format!("{name}\n")).unwrap();
        std::fs::write(dir.join("metadata/layout.conf"), "masters =\n").unwrap();
        if !mirrors.is_empty() {
            std::fs::write(dir.join("profiles/thirdpartymirrors"), mirrors).unwrap();
        }
        Repository::builder().in_memory_cache().open(dir).unwrap()
    }

    #[test]
    fn from_repos_expands_a_mirror_only_another_repo_defines() {
        let tmp = tempfile::tempdir().unwrap();
        let overlay = repo_with_mirrors(&tmp.path().join("overlay"), "overlay", "");
        let gentoo = repo_with_mirrors(
            &tmp.path().join("gentoo"),
            "gentoo",
            "foo https://foo.example/pub https://foo2.example\nbar https://bar.example\n",
        );
        let r = DistfileResolver::from_repos(&[&overlay, &gentoo], vec![]).unwrap();
        let entries = SrcUriEntry::parse("mirror://foo/x.tar.gz mirror://nope/y.tar.gz").unwrap();
        let dfs = r.resolve(&entries, &HashSet::new());
        assert_eq!(
            dfs[0].urls,
            [
                "https://foo.example/pub/x.tar.gz",
                "https://foo2.example/x.tar.gz"
            ]
        );
        assert!(dfs[1].urls.is_empty());
    }

    #[test]
    fn from_repos_stacks_incrementally() {
        let tmp = tempfile::tempdir().unwrap();
        let high = repo_with_mirrors(&tmp.path().join("a"), "a", "foo https://a.example\n");
        let low = repo_with_mirrors(
            &tmp.path().join("b"),
            "b",
            "foo https://b.example -https://a.example\nbar https://bar.example\nbar -*\n",
        );
        let r = DistfileResolver::from_repos(&[&high, &low], vec![]).unwrap();
        assert_eq!(r.thirdparty["foo"], ["https://b.example"]);
        assert!(!r.thirdparty.contains_key("bar"));
    }

    #[test]
    fn restrict_fetch_still_fetches_a_mirror_uri_from_its_named_mirrors() {
        let r = resolver(&["https://mirror.gentoo.org"]);
        let entries =
            SrcUriEntry::parse("mirror://kde/a.tar.gz https://x.example/b.tar.gz").unwrap();
        let gate = RestrictGate::from_restrict(&RestrictExpr::parse("fetch").unwrap());
        let dfs = r.resolve_with(&entries, &HashSet::new(), gate);
        assert_eq!(dfs[0].filename, "a.tar.gz");
        assert_eq!(dfs[0].urls, ["https://mirrors.kde.org/a.tar.gz"]);
        assert!(RestrictGate::uri_exempts_fetch(
            dfs[0].restriction.as_deref()
        ));
        assert_eq!(dfs[1].filename, "b.tar.gz");
        assert!(dfs[1].urls.is_empty());
        assert!(!RestrictGate::uri_exempts_fetch(
            dfs[1].restriction.as_deref()
        ));
    }

    #[test]
    fn restrict_gate_with_use_honours_conditional_fetch() {
        let entries = RestrictExpr::parse("bindist? ( fetch )").unwrap();
        let on: HashSet<String> = ["bindist".to_owned()].into();
        let gate = RestrictGate::from_restrict_with_use(&entries, &on);
        assert!(gate.fetch && gate.mirror);
        let gate = RestrictGate::from_restrict_with_use(&entries, &HashSet::new());
        assert!(!gate.fetch && !gate.mirror);
        assert_eq!(
            RestrictGate::from_restrict(&entries),
            RestrictGate::default()
        );
    }

    #[test]
    fn resolve_uri_map_unrestricted_is_unaffected() {
        let r = resolver(&[]);
        let entries = SrcUriEntry::parse("https://example.com/foo.tar.gz").unwrap();
        let dfs = r.resolve_uri_map(&entries, &ResolveOpts::default());
        assert_eq!(dfs.len(), 1);
    }
}
