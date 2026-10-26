//! CLI-facing binpkg config resolution: `PKGDIR` lookup and the
//! `binrepos.conf`/`PORTAGE_BINHOST` remote-binhost list.
//!
//! The binpkg *format* itself (the `Packages` index, GPKG containers, the
//! USE-reuse check, maintenance operations) lives in the standalone
//! `portage-binpkg` crate — this module only holds the bits that genuinely
//! need `&Cli`/`make.conf`, which that crate deliberately doesn't depend on.

use std::collections::HashSet;

use camino::{Utf8Path, Utf8PathBuf};

use portage_atom::interner::{DefaultInterner, Interned};
use portage_repo::MakeConf;
use portage_resolve::Roots;

use crate::cli::Cli;

/// Real portage's own hardcoded system default — for the host-root test
/// only; `resolve_pkgdir` no longer references this directly since
/// `merge_root.join("var/cache/binpkgs")` reduces to the same string when
/// `merge_root` is `/`.
#[cfg(test)]
const DEFAULT_PKGDIR: &str = "/var/cache/binpkgs";
const MAKE_GLOBALS: &str = "/usr/share/portage/config/make.globals";

/// Resolve `PKGDIR` under `globals`'s own (`--target`-substituted) roots —
/// see [`resolve_pkgdir_for_roots`] for the algorithm. Shared by `em maint
/// binhost`/`em maint binpkg` and the `-k` consumer, all single-root
/// commands where "globals's own roots" is unambiguous.
pub(crate) async fn resolve_pkgdir(globals: &Cli) -> Utf8PathBuf {
    resolve_pkgdir_for_roots(&globals.roots()).await
}

/// Resolve `PKGDIR` under specific `roots`: `$PKGDIR` env → `make.conf`
/// (`roots.config()`) → `make.globals` (host builds only) →
/// `roots.merge_root().join("var/cache/binpkgs")`.
///
/// The per-roots twin of [`resolve_pkgdir`] — used by the merge path to open
/// the **host** PKGDIR for `MergeRoot::Host` plan entries under `--target`,
/// separately from the target's own PKGDIR that `resolve_pkgdir` resolves.
///
/// The `make.globals`/hardcoded-default steps are **host** defaults — real
/// portage's own system-wide install convention, unconditionally
/// `/var/cache/binpkgs` (confirmed: this repo's own `make.globals` hardcodes
/// exactly that). For a `--root`/`--target`/`--local`/`--prefix` merge root
/// (anything other than `/`), the host default is wrong (root-owned path).
/// Use a root-relative default; explicit `$PKGDIR` / make.conf still win.
pub(crate) async fn resolve_pkgdir_for_roots(roots: &Roots) -> Utf8PathBuf {
    if let Ok(v) = std::env::var("PKGDIR")
        && !v.trim().is_empty()
    {
        return Utf8PathBuf::from(v);
    }
    if let Some(v) = read_make_conf_var_for_roots(roots, "PKGDIR").await
        && !v.is_empty()
    {
        return Utf8PathBuf::from(v);
    }
    let merge_root = roots.merge_root().to_owned();
    // make.globals is a host-level default; only consult it for a real host
    // build. A non-host root falls through to the join below unconditionally
    // — no separate "is this the host?" branch needed there, since
    // `"/".join("var/cache/binpkgs")` already *is* the host default.
    if merge_root.as_str() == "/" {
        let mg = Utf8Path::new(MAKE_GLOBALS);
        if mg.exists()
            && let Ok(mc) = MakeConf::load(mg)
            && let Some(v) = mc.get("PKGDIR").filter(|s| !s.is_empty())
        {
            return Utf8PathBuf::from(v);
        }
    }
    merge_root.join("var/cache/binpkgs")
}

/// Resolve the GPG verify keyring directory (`BINPKG_GPG_VERIFY_GPG_HOME`):
/// `$BINPKG_GPG_VERIFY_GPG_HOME` env → make.conf → `<config root>/etc/portage/gnupg`.
/// Root-aware via `roots.config()` (never the real host's path for a
/// non-host `--root`/`--target`/`--prefix` — same bug class already fixed
/// once for `PKGDIR`) — a flat directory of armored public-key files, not a
/// real gpg keybox (see `portage_binpkg::gpg`'s module doc for why).
pub(crate) async fn resolve_gpg_verify_home_for_roots(roots: &Roots) -> Utf8PathBuf {
    if let Ok(v) = std::env::var("BINPKG_GPG_VERIFY_GPG_HOME")
        && !v.trim().is_empty()
    {
        return Utf8PathBuf::from(v);
    }
    if let Some(v) = read_make_conf_var_for_roots(roots, "BINPKG_GPG_VERIFY_GPG_HOME").await
        && !v.is_empty()
    {
        return Utf8PathBuf::from(v);
    }
    roots
        .config()
        .unwrap_or_else(|| Utf8Path::new("/"))
        .join("etc/portage/gnupg")
}

/// How `-k`/`-K`/`-g`/`-G` judge a binpkg against the configuration and the current ebuild
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ReusePolicy {
    use_match: portage_binpkg::UseMatch,
    /// Compare against the ebuild's `IUSE`, as portage does unless `--usepkgonly`
    ebuild_iuse: bool,
    /// `--binpkg-changed-deps`
    changed_deps: bool,
    /// Compare `DEPEND`/`BDEPEND` too, which portage does only under `--with-bdeps y`
    build_deps: bool,
}

impl ReusePolicy {
    /// The policy portage's `create_depgraph_params` derives from these options
    pub(crate) fn new(flags: &crate::cli::MergeFlags, newuse: bool, changed_use: bool) -> Self {
        use crate::cli::YesNo;
        use portage_binpkg::UseMatch;
        let binpkg_only = flags.usepkgonly || flags.getbinpkgonly;
        let respect_use = flags
            .binpkg_respect_use
            .map_or(!binpkg_only, |v| v == YesNo::Y);
        let use_match = if newuse || (respect_use && !changed_use) {
            UseMatch::Iuse
        } else if changed_use || respect_use {
            UseMatch::Enabled
        } else {
            UseMatch::Any
        };
        Self {
            use_match,
            ebuild_iuse: !binpkg_only,
            changed_deps: flags
                .binpkg_changed_deps
                .map_or(!binpkg_only, |v| v == YesNo::Y),
            build_deps: flags.with_bdeps == Some(YesNo::Y),
        }
    }

    /// Run `f` with the query a binpkg of an ebuild with `facts` must pass to stand in for
    /// building it with `use_flags`, under `env` and its build-env `key`
    ///
    /// Without `facts` (no metadata) only the binpkg's own `IUSE` counts; `build_id` admits
    /// only that build.
    pub(crate) fn with_query<R>(
        &self,
        use_flags: &[Interned<DefaultInterner>],
        facts: Option<&EbuildFacts>,
        build_id: Option<u64>,
        env: &DesiredBuildEnv,
        key: &str,
        f: impl FnOnce(&portage_binpkg::ReuseQuery) -> R,
    ) -> R {
        let accept = |entry: &portage_binpkg::BinpkgEntry| {
            facts.is_none_or(|facts| {
                !self.changed_deps || facts.same_deps(entry, self.build_deps, &env.libc)
            })
        };
        let query = portage_binpkg::ReuseQuery {
            use_flags: use_flags.iter().map(|f| f.as_str().to_owned()).collect(),
            use_match: self.use_match,
            iuse: facts.filter(|_| self.ebuild_iuse).map(|f| f.iuse.clone()),
            forced: facts.map(|f| f.forced.clone()).unwrap_or_default(),
            chost: &env.chost,
            build_env_key: key,
            build_id,
            accept: Some(&accept),
        };
        f(&query)
    }
}

/// What the current ebuild declares, to judge a binpkg built from an older one
#[derive(Debug, Clone, Default)]
pub(crate) struct EbuildFacts {
    /// `IUSE`, without `+`/`-` defaults
    pub iuse: HashSet<String>,
    /// The `use.force` and `use.mask` flags that apply to it
    pub forced: HashSet<String>,
    deps: [Vec<portage_atom::DepEntry>; 5],
}

impl EbuildFacts {
    /// The facts of `cache`'s ebuild, given its forced and masked flags
    pub(crate) fn new<'a>(
        cache: &portage_metadata::CacheEntry,
        forced: impl IntoIterator<Item = &'a Interned<DefaultInterner>>,
    ) -> Self {
        let m = &cache.metadata;
        Self {
            iuse: m.iuse.iter().map(|i| i.name().to_owned()).collect(),
            forced: forced.into_iter().map(|f| f.as_str().to_owned()).collect(),
            deps: [&m.rdepend, &m.pdepend, &m.idepend, &m.depend, &m.bdepend]
                .map(|d| d.list().to_vec()),
        }
    }

    /// Whether `entry` was built from the same dependencies, reduced with its own USE
    fn same_deps(
        &self,
        entry: &portage_binpkg::BinpkgEntry,
        build_deps: bool,
        libc: &[portage_atom::Cpn],
    ) -> bool {
        let use_flags: HashSet<&str> = entry.use_set.iter().map(String::as_str).collect();
        let d = &entry.deps;
        let built = [&d.rdepend, &d.pdepend, &d.idepend, &d.depend, &d.bdepend];
        let compared = if build_deps { 5 } else { 3 };
        built
            .iter()
            .zip(&self.deps)
            .take(compared)
            .all(|(built, ebuild)| {
                let ebuild: Vec<String> = ebuild.iter().map(ToString::to_string).collect();
                crate::ebuild::build_info::same_deps(built, &ebuild.join(" "), &use_flags, libc)
            })
    }
}

/// What the `-p` display needs to mark the rows `-k`/`-K` would take from the local `PKGDIR`
///
/// Remote binhosts (`-g`/`-G`) are not checked: that would add a network fetch to a plain
/// preview. Single-index, unlike the merge's per-entry host/target `PKGDIR` split.
pub(crate) struct BinpkgPreview {
    index: portage_binpkg::BinpkgIndex,
    policy: ReusePolicy,
    target: (DesiredBuildEnv, Vec<std::path::PathBuf>),
    host: (DesiredBuildEnv, Vec<std::path::PathBuf>),
}

/// The build-env keys [`BinpkgPreview::reusable`] reads, per merge root and cpv
pub(crate) type PreviewKeys = std::collections::HashMap<(bool, portage_atom::Cpv), String>;

impl BinpkgPreview {
    /// Open the local index if `-k`/`--usepkg` or `-K`/`--usepkgonly` is active; silent on error
    pub(crate) async fn open(
        globals: &Cli,
        merge_flags: &crate::cli::MergeFlags,
        policy: ReusePolicy,
    ) -> Option<Self> {
        if !(merge_flags.usepkg || merge_flags.usepkgonly) {
            return None;
        }
        let pkgdir = resolve_pkgdir(globals).await;
        let index = portage_binpkg::BinpkgIndex::open(pkgdir.as_std_path()).ok()?;
        let env = async |roots: &Roots| {
            (
                DesiredBuildEnv::for_roots(roots).await,
                DesiredBuildEnv::portage_dirs(roots),
            )
        };
        Some(Self {
            index,
            policy,
            target: env(&globals.roots()).await,
            host: env(&globals.host_roots()).await,
        })
    }

    /// The build-env keys of `pkgs`, given as (merges to the host, cpv)
    pub(crate) async fn keys(
        &self,
        pkgs: impl IntoIterator<Item = (bool, portage_atom::Cpv)>,
    ) -> PreviewKeys {
        let mut keys = PreviewKeys::new();
        for (host, cpv) in pkgs {
            let (env, dirs) = if host { &self.host } else { &self.target };
            let key = env.key_for(dirs, &cpv).await;
            keys.insert((host, cpv), key);
        }
        keys
    }

    /// The local index
    pub(crate) fn index(&self) -> &portage_binpkg::BinpkgIndex {
        &self.index
    }

    /// A preview of `index` under `-k`'s policy, for an unconfigured host
    #[cfg(test)]
    pub(crate) fn for_test(index: portage_binpkg::BinpkgIndex) -> Self {
        let flags = crate::cli::MergeFlags {
            usepkg: true,
            ..Default::default()
        };
        Self {
            index,
            policy: ReusePolicy::new(&flags, false, false),
            target: (DesiredBuildEnv::default(), Vec::new()),
            host: (DesiredBuildEnv::default(), Vec::new()),
        }
    }

    /// The USE of the binpkg that would stand in for building `cpv` with `use_flags`, if any
    ///
    /// `build_id`, a build-id target's, admits only that build.
    pub(crate) fn reusable(
        &self,
        keys: &PreviewKeys,
        host: bool,
        cpv: &portage_atom::Cpv,
        build_id: Option<u64>,
        use_flags: &[Interned<DefaultInterner>],
        facts: Option<&EbuildFacts>,
    ) -> Option<HashSet<String>> {
        let env = if host { &self.host.0 } else { &self.target.0 };
        let key = keys.get(&(host, cpv.clone())).map_or("", String::as_str);
        self.policy
            .with_query(use_flags, facts, build_id, env, key, |query| {
                let (_, entry) = self.index.find_reusable_entry(&cpv.to_string(), query)?;
                Some(entry.use_set.clone())
            })
    }
}

/// Read a variable from `make.conf` under the resolved config root
pub(crate) async fn read_make_conf_var(globals: &Cli, var: &str) -> Option<String> {
    read_make_conf_var_for_roots(&globals.roots(), var).await
}

/// Read a variable from make.conf under specific roots — used to get
/// per-entry CHOST/CFLAGS/etc. for cross-compilation scenarios, where the
/// desired config root isn't `globals`'s own (e.g. a `MergeRoot::Host` entry
/// under `--target`).
///
/// Evaluates the file via [`MakeConf::apply_to`] (a real, minimal
/// `brush_core::Shell` sourcing the file) rather than a plain
/// [`MakeConf::get`], so `${VAR}` self-references expand — the stock Gentoo
/// `COMMON_FLAGS="-O2 -march=…"` + `CFLAGS="${COMMON_FLAGS}"` pattern used to
/// leave every reader seeing a literal, `-m*`-free `${COMMON_FLAGS}` string,
/// silently starving the binpkg `build_env_key` gate (rebuild loop).
///
/// A name this file never assigns now expands to empty rather than staying
/// literal — more correct, since that literal string was never usable.
pub(crate) async fn read_make_conf_var_for_roots(roots: &Roots, var: &str) -> Option<String> {
    let cfg_root = roots
        .config()
        .map(|c| c.to_path_buf())
        .unwrap_or_else(|| Utf8PathBuf::from("/"));
    for rel in ["etc/portage/make.conf", "etc/make.conf"] {
        let p = cfg_root.join(rel);
        if p.exists()
            && let Ok(mc) = MakeConf::load(&p)
        {
            let mut env = std::collections::BTreeMap::new();
            mc.apply_to(&mut env).await.ok()?;
            if let Some(v) = env.get(var).filter(|s| !s.is_empty()) {
                return Some(v.clone());
            }
        }
    }
    None
}

/// Evaluate whichever of `roots`' two candidate make.conf paths exists first
/// (`etc/portage/make.conf`, then `etc/make.conf`) into a flat `NAME ->
/// value` map via [`MakeConf::apply_to`]. Empty map if neither exists or
/// fails to parse. Unlike `read_make_conf_var_for_roots`, this picks one
/// file and returns its whole map rather than falling back per-variable — a
/// split make.conf across both paths isn't a real config this repo supports.
async fn evaluated_make_conf_env(roots: &Roots) -> std::collections::BTreeMap<String, String> {
    let cfg_root = roots
        .config()
        .map(|c| c.to_path_buf())
        .unwrap_or_else(|| Utf8PathBuf::from("/"));
    for rel in ["etc/portage/make.conf", "etc/make.conf"] {
        let p = cfg_root.join(rel);
        if p.exists()
            && let Ok(mc) = MakeConf::load(&p)
        {
            let mut env = std::collections::BTreeMap::new();
            if mc.apply_to(&mut env).await.is_ok() {
                return env;
            }
        }
    }
    std::collections::BTreeMap::new()
}

/// The desired build environment resolved from one config root's make.conf:
/// the four build-env flag vars (expanded) plus `CHOST`. Shared by `em maint
/// binpkg fingerprint` and the merge path's per-entry desired build_env_key
/// computation.
#[derive(Default)]
pub(crate) struct DesiredBuildEnv {
    pub cflags: String,
    pub cxxflags: String,
    pub ldflags: String,
    pub rustflags: String,
    pub chost: String,
    /// The root's installed `virtual/libc` providers, which `--binpkg-changed-deps` ignores
    pub libc: Vec<portage_atom::Cpn>,
    /// The full evaluated make.conf map this environment was derived from —
    /// kept so [`Self::key_for`] can seed a package.env overlay with it: an
    /// env file referencing e.g. `${COMMON_FLAGS}` needs the same baseline
    /// `for_roots` itself evaluated, not just the four flat flag values.
    make_conf_env: std::collections::BTreeMap<String, String>,
}

impl DesiredBuildEnv {
    /// Read the desired build env for `roots` — one make.conf evaluation, not five
    ///
    /// `chost` falls back to the process `CHOST` env var when make.conf doesn't set it, the
    /// same rule `merge_sequential`/ `merge_parallel` already apply for their own per-entry
    /// desired CHOST.
    pub(crate) async fn for_roots(roots: &Roots) -> Self {
        let make_conf_env = evaluated_make_conf_env(roots).await;
        let get = |name: &str| make_conf_env.get(name).cloned().unwrap_or_default();
        let chost = make_conf_env
            .get("CHOST")
            .cloned()
            .filter(|s| !s.is_empty())
            .or_else(|| std::env::var("CHOST").ok().filter(|s| !s.is_empty()))
            .unwrap_or_default();
        Self {
            cflags: get("CFLAGS"),
            cxxflags: get("CXXFLAGS"),
            ldflags: get("LDFLAGS"),
            rustflags: get("RUSTFLAGS"),
            chost,
            libc: portage_vdb::Vdb::open(roots.merge_root().join("var/db/pkg"))
                .map(|vdb| crate::ebuild::build_info::libc_cpns(&vdb))
                .unwrap_or_default(),
            make_conf_env,
        }
    }

    /// The build-env key derived from this environment's flags (see
    /// [`portage_binpkg::build_env_key`]) — the make.conf-only baseline, no
    /// per-package package.env overlay. See [`Self::key_for`] for that.
    pub(crate) fn key(&self) -> String {
        portage_binpkg::build_env_key(&self.cflags, &self.cxxflags, &self.ldflags, &self.rustflags)
    }

    /// A bare `chost`-only environment, no make.conf/filesystem involved —
    /// for tests that only care about host-vs-target selection, not flag
    /// evaluation (e.g. `entry_desired_env`'s own tests in `merge/mod.rs`).
    #[cfg(test)]
    pub(crate) fn for_test(chost: &str) -> Self {
        Self {
            chost: chost.to_string(),
            ..Default::default()
        }
    }

    /// Per-package build-env key (S6): overlay `cpv`'s `package.env` files
    /// (`portage_repo::env_files_for`) onto the make.conf baseline and
    /// re-derive. Falls back to [`Self::key`] when no env file matches this
    /// package — the common case, zero extra I/O beyond the package.env
    /// lookup itself.
    ///
    /// This overlays each env file onto the make.conf baseline via
    /// [`MakeConf::apply_to`] again — a real shell sourcing round-trip, so
    /// `CFLAGS="${CFLAGS} -foo"` appends and arbitrary shell logic in an env
    /// file is evaluated for real rather than silently ignored. A wrong
    /// *desired* key here only risks an extra rebuild or a missed reuse,
    /// never wrong-arch reuse — the binpkg side's key is always accurate.
    ///
    /// `slot: None` limitation: slot-qualified `package.env` atoms won't
    /// match here (a plan entry doesn't carry a slot at this point) — the
    /// real build still applies them; the worst case is a missed reuse for
    /// that package, not a wrong one.
    pub(crate) async fn key_for(
        &self,
        portage_dirs: &[std::path::PathBuf],
        cpv: &portage_atom::Cpv,
    ) -> String {
        let env_files = portage_repo::env_files_for(portage_dirs, cpv, None);
        if env_files.is_empty() {
            return self.key();
        }
        let mut env = self.make_conf_env.clone();
        for f in &env_files {
            if let Some(p) = Utf8Path::from_path(f)
                && let Ok(mc) = MakeConf::load(p)
            {
                let _ = mc.apply_to(&mut env).await;
            }
        }
        // build_env_key only needs &str, so borrow straight out of `env`
        // instead of allocating a fresh String per flag just to re-borrow it.
        let get = |name: &str| env.get(name).map(String::as_str).unwrap_or("");
        portage_binpkg::build_env_key(
            get("CFLAGS"),
            get("CXXFLAGS"),
            get("LDFLAGS"),
            get("RUSTFLAGS"),
        )
    }

    /// The `/etc/portage` directories [`portage_repo::env_files_for`] should
    /// search for `roots`' `package.env` — mirrors the real build path's own
    /// construction (`ebuild.rs`'s per-package build-environment sourcing,
    /// `config_root.join("etc/portage")` plus the eprefix overlay) so
    /// plan-time and build-time can't silently drift apart.
    pub(crate) fn portage_dirs(roots: &Roots) -> Vec<std::path::PathBuf> {
        let base = roots
            .config()
            .map(|c| c.to_path_buf())
            .unwrap_or_else(|| Utf8PathBuf::from("/"));
        let mut dirs = vec![base.join("etc/portage").into_std_path_buf()];
        if let Some(overlay) = roots.config_overlay() {
            dirs.push(overlay.as_std_path().to_path_buf());
        }
        dirs
    }
}

/// One `binrepos.conf` section — real portage's `BinRepoConfig`, restricted
/// to the fields em's remote binpkg fetch path uses.
///
/// `frozen` is passed to [`fetch_index_cached`] to prefer this binhost's
/// cached `Packages` over refetching; `verify_signature` decides the
/// [`VerifyPolicy`](portage_binpkg::VerifyPolicy) a fetched container is
/// extracted with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinRepoEntry {
    /// Section name, or an md5 hex digest of the `sync-uri` for a
    /// `PORTAGE_BINHOST`-derived implicit entry (matches real portage's own
    /// `_digest_uri` naming — this is display/debugging only, never a sort
    /// tie-breaker in practice: implicit entries always get a distinct
    /// priority `>= 1`, so they never actually tie against an explicit
    /// section's default `priority` of `0`).
    pub name: String,
    /// The binhost base URI, trailing slash stripped
    pub sync_uri: String,
    pub frozen: bool,
    /// `verify-signature`, `None` when unset so `FEATURES` decide
    pub verify_signature: Option<bool>,
}

/// Resolve the configured remote binhosts: `binrepos.conf` (global then
/// `${PORTAGE_CONFIGROOT}/etc/portage/binrepos.conf`, either may be a
/// directory of `*.conf` files) plus legacy `PORTAGE_BINHOST`, combined in
/// real portage's priority order (`BinRepoConfigLoader.__init__`): explicit
/// sections use their own `priority =` (default `0`, ties by name);
/// `PORTAGE_BINHOST` URLs fold in as unnamed, auto-prioritized entries.
///
/// Sorted ascending by `(priority, name)` then reversed, matching
/// `bintree.py`'s `reversed(list(self._binrepos_conf.values()))` — for a
/// plain `PORTAGE_BINHOST` list with no `binrepos.conf`, the reversals
/// cancel to left-to-right order (see unit tests below). `[DEFAULT]` keys
/// apply to every section; no `%(VAR)s` interpolation.
pub(crate) async fn portage_binhosts(globals: &Cli) -> Vec<BinRepoEntry> {
    let config_root = globals
        .roots()
        .config()
        .map(|c| c.to_path_buf())
        .unwrap_or_else(|| Utf8PathBuf::from("/"));

    let mut sections: std::collections::HashMap<String, std::collections::HashMap<String, String>> =
        std::collections::HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for rel in [
        "usr/share/portage/config/binrepos.conf",
        "etc/portage/binrepos.conf",
    ] {
        let path = config_root.join(rel);
        for file in portage_repo::ini::collect_conf_files(path.as_std_path()).unwrap_or_default() {
            if let Ok(contents) = std::fs::read_to_string(&file) {
                portage_repo::ini::merge_sections(&mut sections, &mut order, &contents);
            }
        }
    }

    let binhost_var = std::env::var("PORTAGE_BINHOST")
        .ok()
        .filter(|v| !v.trim().is_empty());
    let binhost_var = match binhost_var {
        Some(v) => Some(v),
        None => read_make_conf_var(globals, "PORTAGE_BINHOST")
            .await
            .filter(|v| !v.is_empty()),
    };

    combine_binhosts(&sections, &order, binhost_var.as_deref())
}

/// The pure core of [`portage_binhosts`]: combine parsed `binrepos.conf`
/// sections with a legacy `PORTAGE_BINHOST` value into the final,
/// priority-ordered list. Split out from the I/O so the priority/reversal
/// algorithm is unit-testable without mutating the real process environment
/// (`PORTAGE_BINHOST` is process-global; tests run threaded, so setting it
/// would race any other test touching the same var).
fn combine_binhosts(
    sections: &std::collections::HashMap<String, std::collections::HashMap<String, String>>,
    order: &[String],
    binhost_var: Option<&str>,
) -> Vec<BinRepoEntry> {
    let mut seen_uris: HashSet<String> = HashSet::new();
    // (priority, name) carried alongside each entry purely for the final
    // sort — not part of the public `BinRepoEntry`, since callers only ever
    // need the already-resolved order.
    let mut repos: Vec<(Option<i64>, String, BinRepoEntry)> = Vec::new();
    for name in order {
        let s = &portage_repo::ini::section_with_defaults(sections, name);
        let get = |key: &str| s.get(key);
        let Some(sync_uri) = get("sync-uri").map(|v| normalize_binhost_uri(v)) else {
            crate::style::warn_line!("missing sync-uri setting for binrepo {name}");
            continue;
        };
        seen_uris.insert(sync_uri.clone());
        let priority = get("priority").and_then(|v| v.parse::<i64>().ok());
        repos.push((
            priority,
            name.clone(),
            BinRepoEntry {
                name: name.clone(),
                sync_uri,
                frozen: parse_binrepo_bool(get("frozen")).unwrap_or(false),
                verify_signature: parse_binrepo_bool(get("verify-signature")),
            },
        ));
    }

    if let Some(val) = binhost_var {
        let mut current_priority: i64 = 0;
        for url in val.split_whitespace().rev() {
            let sync_uri = normalize_binhost_uri(url);
            if seen_uris.insert(sync_uri.clone()) {
                current_priority += 1;
                let name = format!("{:x}", md5::compute(sync_uri.as_bytes()));
                repos.push((
                    Some(current_priority),
                    name.clone(),
                    BinRepoEntry {
                        name,
                        sync_uri,
                        frozen: false,
                        verify_signature: None,
                    },
                ));
            }
        }
    }

    repos.sort_by(|a, b| (a.0.unwrap_or(0), &a.1).cmp(&(b.0.unwrap_or(0), &b.1)));
    repos.into_iter().rev().map(|(_, _, e)| e).collect()
}

fn normalize_binhost_uri(uri: &str) -> String {
    uri.trim().trim_end_matches('/').to_string()
}

fn parse_binrepo_bool(v: Option<&String>) -> Option<bool> {
    v.map(|s| matches!(s.to_lowercase().as_str(), "true" | "yes"))
}

/// A held [`lock_pkgdir`]; the lock file is removed on release, as Portage removes its own
pub(crate) struct PkgdirLock {
    path: Utf8PathBuf,
    _file: std::fs::File,
}

impl Drop for PkgdirLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(self.path.as_std_path());
    }
}

/// Exclusive lock on `pkgdir`, held while em allocates a build id, writes or removes containers,
/// or rewrites the index; another em run or an emerge on the same `PKGDIR` waits
///
/// The file is Portage's index lock, `.Packages.portage_lockfile`: the record lock excludes
/// emerge, the `flock` other em threads, which record locks of one process do not.
pub(crate) fn lock_pkgdir(pkgdir: &Utf8Path) -> anyhow::Result<PkgdirLock> {
    use anyhow::Context;
    use rustix::fs::FlockOperation::LockExclusive;
    std::fs::create_dir_all(pkgdir.as_std_path()).with_context(|| format!("creating {pkgdir}"))?;
    let path = pkgdir.join(".Packages.portage_lockfile");
    loop {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path.as_std_path())
            .with_context(|| format!("opening {path}"))?;
        rustix::fs::flock(&file, LockExclusive).with_context(|| format!("locking {path}"))?;
        while rustix::fs::fcntl_lock(&file, LockExclusive) == Err(rustix::io::Errno::INTR) {}
        // The previous holder unlinks the file before releasing it; retry on the new one.
        let held = rustix::fs::fstat(&file).with_context(|| format!("checking {path}"))?;
        if let Ok(now) = rustix::fs::stat(path.as_std_path())
            && (now.st_dev, now.st_ino) == (held.st_dev, held.st_ino)
        {
            return Ok(PkgdirLock { path, _file: file });
        }
    }
}

/// The next free build id for `<cat>/<pf>`: past every container of it, in Portage's `<cat>/<pn>/`
/// directory and in the flat `<cat>/` layout older em wrote
fn next_build_id(pkgdir: &Utf8Path, cat: &str, pn: &str, pf: &str) -> u32 {
    let prefix = format!("{pf}-");
    let mut max = 0u32;
    for dir in [pkgdir.join(cat), pkgdir.join(cat).join(pn)] {
        let Ok(rd) = std::fs::read_dir(dir.as_std_path()) else {
            continue;
        };
        for e in rd.flatten() {
            if let Some(rest) = e.file_name().to_string_lossy().strip_prefix(&prefix)
                && let Some(id) = rest.strip_suffix(".gpkg.tar")
                && let Ok(n) = id.parse::<u32>()
            {
                max = max.max(n);
            }
        }
    }
    max + 1
}

/// Reserve a build id and the container path for `<cat>/<pn>/<pf>`, as Portage's
/// binpkg-multi-instance lays it out: `PKGDIR/<cat>/<pn>/<pf>-<id>.gpkg.tar`
///
/// The path is created empty, so a writer that does not hold [`lock_pkgdir`] (Portage) takes the
/// next id; the caller removes it if writing the container fails. Without `multi_instance` the
/// path is `PKGDIR/<cat>/<pf>.gpkg.tar`, replaced in place, with no build id.
pub(crate) fn reserve_container(
    pkgdir: &Utf8Path,
    cat: &str,
    pn: &str,
    pf: &str,
    multi_instance: bool,
) -> anyhow::Result<(Option<u32>, Utf8PathBuf)> {
    use anyhow::Context;
    if !multi_instance {
        let dir = pkgdir.join(cat);
        std::fs::create_dir_all(dir.as_std_path()).with_context(|| format!("creating {dir}"))?;
        return Ok((None, dir.join(format!("{pf}.gpkg.tar"))));
    }
    let dir = pkgdir.join(cat).join(pn);
    std::fs::create_dir_all(dir.as_std_path()).with_context(|| format!("creating {dir}"))?;
    let mut id = next_build_id(pkgdir, cat, pn, pf);
    loop {
        let path = dir.join(format!("{pf}-{id}.gpkg.tar"));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path.as_std_path())
        {
            Ok(_) => return Ok((Some(id), path)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => id += 1,
            Err(e) => return Err(e).with_context(|| format!("creating {path}")),
        }
    }
}

/// Settings Portage's `_update_pkgindex_header` copies into the `Packages` header
const INDEX_HEADER_KEYS: &[&str] = &[
    "ACCEPT_KEYWORDS",
    "ACCEPT_LICENSE",
    "ACCEPT_PROPERTIES",
    "ACCEPT_RESTRICT",
    "CBUILD",
    "CONFIG_PROTECT",
    "CONFIG_PROTECT_MASK",
    "FEATURES",
    "GENTOO_MIRRORS",
    "INSTALL_MASK",
    "IUSE_IMPLICIT",
    "USE",
    "USE_EXPAND",
    "USE_EXPAND_HIDDEN",
    "USE_EXPAND_IMPLICIT",
    "USE_EXPAND_UNPREFIXED",
];

/// The `Packages` header and `compress-index` for an index written under `settings`
///
/// As in Portage, a config root whose profile lacks `IUSE_IMPLICIT` gets only `CHOST`.
pub(crate) fn index_config(
    settings: &portage_repo::Settings,
    config_root: &Utf8Path,
) -> portage_binpkg::IndexConfig {
    let get = |key: &str| settings.get(key).filter(|v| !v.is_empty());
    let mut header = std::collections::BTreeMap::new();
    let mut set = |key: &str, value: Option<&str>| {
        if let Some(value) = value {
            header.insert(key.to_owned(), value.to_owned());
        }
    };
    set("CHOST", get("CHOST"));
    let profile = std::fs::canonicalize(config_root.join("etc/portage/make.profile"));
    if let (Ok(profile), Some(_)) = (profile, get("IUSE_IMPLICIT")) {
        let relative = profile
            .ancestors()
            .find(|dir| dir.file_name().is_some_and(|n| n == "profiles"))
            .and_then(|base| profile.strip_prefix(base).ok())
            .unwrap_or(&profile);
        set("PROFILE", relative.to_str());
        set("URI", get("PORTAGE_BINHOST_HEADER_URI"));
        set("TTL", get("PORTAGE_BINHOST_TTL"));
        let implicit = get("USE_EXPAND_IMPLICIT").unwrap_or_default();
        let unprefixed = get("USE_EXPAND_UNPREFIXED").unwrap_or_default();
        for key in INDEX_HEADER_KEYS
            .iter()
            .copied()
            .chain(implicit.split_whitespace())
            .chain(unprefixed.split_whitespace())
        {
            set(key, get(key));
        }
        for name in implicit.split_whitespace() {
            let key = format!("USE_EXPAND_VALUES_{name}");
            set(&key, get(&key));
        }
    }
    let compress =
        get("FEATURES").is_some_and(|f| f.split_whitespace().any(|w| w == "compress-index"));
    portage_binpkg::IndexConfig { header, compress }
}

/// [`index_config`] for the config root `globals` selects, or only make.conf's `CHOST` when it
/// has no profile to evaluate
pub(crate) async fn cli_index_config(globals: &Cli) -> portage_binpkg::IndexConfig {
    match crate::settings::load(globals).await {
        Ok(Some(settings)) => {
            let roots = globals.roots();
            index_config(&settings, roots.config().unwrap_or(Utf8Path::new("/")))
        }
        _ => {
            let mut config = portage_binpkg::IndexConfig::default();
            if let Some(chost) = read_make_conf_var(globals, "CHOST").await {
                config.header.insert("CHOST".to_owned(), chost);
            }
            config
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parallel_writers_under_the_pkgdir_lock_take_distinct_build_ids() {
        let dir = tempfile::tempdir().unwrap();
        let pkgdir = Utf8PathBuf::from_path_buf(dir.path().to_owned()).unwrap();
        let writers: Vec<_> = (0..8)
            .map(|_| {
                let pkgdir = pkgdir.clone();
                std::thread::spawn(move || {
                    let _lock = lock_pkgdir(&pkgdir).unwrap();
                    let (id, out) =
                        reserve_container(&pkgdir, "sys-libs", "glibc", "glibc-2.42", true)
                            .unwrap();
                    std::thread::sleep(std::time::Duration::from_millis(5));
                    std::fs::write(out.as_std_path(), b"").unwrap();
                    id.unwrap()
                })
            })
            .collect();
        let mut ids: Vec<u32> = writers.into_iter().map(|w| w.join().unwrap()).collect();
        ids.sort_unstable();
        assert_eq!(ids, (1..=8).collect::<Vec<_>>());
    }

    #[test]
    fn a_reserved_container_follows_portages_layout_past_older_flat_ids() {
        let dir = tempfile::tempdir().unwrap();
        let pkgdir = Utf8PathBuf::from_path_buf(dir.path().to_owned()).unwrap();
        for old in [
            "sys-libs/glibc-2.42-4.gpkg.tar",
            "sys-libs/glibc/glibc-2.42-5.gpkg.tar",
        ] {
            let path = pkgdir.join(old);
            std::fs::create_dir_all(path.parent().unwrap().as_std_path()).unwrap();
            std::fs::write(path.as_std_path(), b"").unwrap();
        }
        let (id, path) =
            reserve_container(&pkgdir, "sys-libs", "glibc", "glibc-2.42", true).unwrap();
        assert_eq!(id, Some(6));
        assert_eq!(path, pkgdir.join("sys-libs/glibc/glibc-2.42-6.gpkg.tar"));
        assert!(path.as_std_path().is_file());

        let (id, path) =
            reserve_container(&pkgdir, "sys-libs", "glibc", "glibc-2.42", false).unwrap();
        assert_eq!(id, None);
        assert_eq!(path, pkgdir.join("sys-libs/glibc-2.42.gpkg.tar"));
    }

    #[test]
    fn the_pkgdir_lock_is_portages_index_lock_and_goes_away() {
        let dir = tempfile::tempdir().unwrap();
        let pkgdir = Utf8PathBuf::from_path_buf(dir.path().to_owned()).unwrap();
        let lock_file = pkgdir.join(".Packages.portage_lockfile");
        let lock = lock_pkgdir(&pkgdir).unwrap();
        assert!(lock_file.is_file());
        drop(lock);
        assert!(!lock_file.exists());
    }

    async fn settings_of(repo_dir: &std::path::Path, defaults: &str) -> portage_repo::Settings {
        let profile = repo_dir.join("profiles/default/linux");
        std::fs::create_dir_all(&profile).unwrap();
        std::fs::create_dir_all(repo_dir.join("metadata")).unwrap();
        std::fs::write(repo_dir.join("metadata/layout.conf"), "").unwrap();
        std::fs::write(profile.join("make.defaults"), defaults).unwrap();
        let repo = portage_repo::Repository::builder()
            .in_memory_cache()
            .open(repo_dir)
            .unwrap();
        let stack = portage_repo::ProfileStack::build(profile).unwrap();
        let mut shell = repo.shell().await.unwrap();
        portage_repo::Settings::evaluate(
            &mut shell,
            portage_repo::SettingsSources {
                stack: &stack,
                env_d: None,
                globals: None,
                confs: &[],
                env: &portage_repo::Environment::default(),
            },
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn the_index_header_follows_portages_update_pkgindex_header() {
        let dir = tempfile::tempdir().unwrap();
        let repo_dir = dir.path().join("repo");
        let config_root = Utf8PathBuf::from_path_buf(dir.path().join("root")).unwrap();
        std::fs::create_dir_all(config_root.join("etc/portage").as_std_path()).unwrap();
        std::os::unix::fs::symlink(
            repo_dir.join("profiles/default/linux"),
            config_root.join("etc/portage/make.profile"),
        )
        .unwrap();

        let full = settings_of(
            &repo_dir,
            "CHOST=\"x86_64-pc-linux-gnu\"\nARCH=\"amd64\"\nIUSE_IMPLICIT=\"prefix\"\n\
             USE_EXPAND_IMPLICIT=\"ARCH\"\nUSE_EXPAND_UNPREFIXED=\"ARCH\"\n\
             USE_EXPAND_VALUES_ARCH=\"amd64 arm64\"\nFEATURES=\"compress-index\"\n\
             PORTAGE_BINHOST_HEADER_URI=\"https://example.invalid/\"\n",
        )
        .await;
        let config = index_config(&full, &config_root);
        let get = |k: &str| config.header.get(k).map(String::as_str);
        assert_eq!(get("PROFILE"), Some("default/linux"));
        assert_eq!(get("CHOST"), Some("x86_64-pc-linux-gnu"));
        assert_eq!(get("ARCH"), Some("amd64"));
        assert_eq!(get("USE_EXPAND_VALUES_ARCH"), Some("amd64 arm64"));
        assert_eq!(get("URI"), Some("https://example.invalid/"));
        assert_eq!(get("FEATURES"), Some("compress-index"));
        assert!(config.compress);

        let bare = settings_of(&repo_dir, "CHOST=\"x86_64-pc-linux-gnu\"\nARCH=\"amd64\"\n").await;
        let config = index_config(&bare, &config_root);
        assert_eq!(
            config.header.keys().collect::<Vec<_>>(),
            ["CHOST"],
            "without IUSE_IMPLICIT only CHOST"
        );
    }

    // Regression test for the stage3 --buildpkg failure: a non-host root
    // must never default PKGDIR to the real system's `/var/cache/binpkgs`
    // (root-owned, not writable, and not even meaningful for a different
    // root's package cache) — see `resolve_pkgdir`'s doc comment.
    #[tokio::test]
    async fn non_host_root_gets_root_relative_pkgdir_default() {
        assert!(
            std::env::var("PKGDIR").is_err(),
            "test assumes no ambient PKGDIR override"
        );
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_str().unwrap();
        let cli = crate::cli::parse_cli(&["em", "emerge", "--root", root]);
        let pkgdir = resolve_pkgdir(&cli).await;
        assert_eq!(
            pkgdir,
            camino::Utf8Path::new(root).join("var/cache/binpkgs")
        );
    }

    // A plain host build (root `/`, no --root/--prefix/--local/--target) is
    // unaffected by the root-aware branch — it still falls through to the
    // pre-existing make.globals/hardcoded-default lookup, exactly as before
    // this change.
    #[tokio::test]
    async fn host_root_skips_the_root_relative_branch() {
        assert!(
            std::env::var("PKGDIR").is_err(),
            "test assumes no ambient PKGDIR override"
        );
        // `["em"]` alone (zero args) trips `arg_required_else_help`
        // (prints help and exits the process) — pass --root explicitly.
        let cli = crate::cli::parse_cli(&["em", "emerge", "--root", "/"]);
        assert_eq!(cli.roots().merge_root().as_str(), "/");
        let expected = {
            let mg = Utf8Path::new(MAKE_GLOBALS);
            if mg.exists()
                && let Ok(mc) = MakeConf::load(mg)
                && let Some(v) = mc.get("PKGDIR").filter(|s| !s.is_empty())
            {
                Utf8PathBuf::from(v)
            } else {
                Utf8PathBuf::from(DEFAULT_PKGDIR)
            }
        };
        assert_eq!(resolve_pkgdir(&cli).await, expected);
    }

    // A `--target` plan's own roots (`resolve_pkgdir`) resolve under the
    // target sysroot, while `host_roots()` (what a `MergeRoot::Host` entry
    // actually wants) resolves as a plain host build — the two must
    // disagree here, or a Host BDEPEND entry would look up binpkgs in the
    // wrong PKGDIR.
    #[tokio::test]
    async fn resolve_pkgdir_for_roots_target_vs_host() {
        assert!(
            std::env::var("PKGDIR").is_err(),
            "test assumes no ambient PKGDIR override"
        );
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_str().unwrap();
        // A bare `--root`+`--target` is now a board-root override: config
        // stays at the toolchain's real bare `/usr/<tuple>` (a fake,
        // nonexistent tuple here, so it never collides with real host
        // crossdev state), while merge_root becomes `root` directly —
        // unnested, unlike the pre-board-root-decoupling behavior.
        let cli = crate::cli::parse_cli(&[
            "em",
            "emerge",
            "--root",
            root,
            "--target",
            "riscv64-unknown-linux-gnu-test-nonexistent",
            "-p",
            "sys-libs/zlib",
        ]);

        let target_roots = cli.roots();
        assert_eq!(target_roots.merge_root().as_str(), root);
        assert_eq!(
            resolve_pkgdir_for_roots(&target_roots).await,
            camino::Utf8Path::new(root).join("var/cache/binpkgs")
        );

        let host_roots = cli.host_roots();
        assert_eq!(
            host_roots.merge_root().as_str(),
            "/",
            "--root's BROOT is the real host, not the sysroot (task #17 fix)"
        );
        let expected_host = {
            let mg = Utf8Path::new(MAKE_GLOBALS);
            if mg.exists()
                && let Ok(mc) = MakeConf::load(mg)
                && let Some(v) = mc.get("PKGDIR").filter(|s| !s.is_empty())
            {
                Utf8PathBuf::from(v)
            } else {
                Utf8PathBuf::from(DEFAULT_PKGDIR)
            }
        };
        assert_eq!(resolve_pkgdir_for_roots(&host_roots).await, expected_host);
    }

    // Config-root make.conf `PKGDIR=` wins for whichever roots see that
    // config root — proven independently for target and host roots.
    #[tokio::test]
    async fn resolve_pkgdir_for_roots_honours_config_root_make_conf() {
        assert!(
            std::env::var("PKGDIR").is_err(),
            "test assumes no ambient PKGDIR override"
        );
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_str().unwrap();
        let conf_dir = dir.path().join("etc/portage");
        std::fs::create_dir_all(&conf_dir).unwrap();
        std::fs::write(conf_dir.join("make.conf"), "PKGDIR=\"/custom/pkgdir\"\n").unwrap();

        // `--config-root` required (see `portage_binhosts`'s own tests below):
        // a bare `--root` leaves `config()` at the real host `/`.
        let cli = crate::cli::parse_cli(&["em", "emerge", "--root", root, "--config-root", root]);
        assert_eq!(
            resolve_pkgdir_for_roots(&cli.roots()).await,
            camino::Utf8Path::new("/custom/pkgdir")
        );
    }

    // The stock Gentoo stage3 `COMMON_FLAGS=… CFLAGS="${COMMON_FLAGS}"`
    // pattern must resolve to the real flags, not the literal `${…}` text —
    // see `read_make_conf_var_for_roots`'s doc comment for why this matters
    // (an unexpanded read silently starves the binpkg reuse key).
    #[tokio::test]
    async fn read_make_conf_var_expands_common_flags_indirection() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_str().unwrap();
        let conf_dir = dir.path().join("etc/portage");
        std::fs::create_dir_all(&conf_dir).unwrap();
        std::fs::write(
            conf_dir.join("make.conf"),
            "COMMON_FLAGS=\"-O2 -march=x86-64-v3\"\nCFLAGS=\"${COMMON_FLAGS}\"\n",
        )
        .unwrap();

        let cli = crate::cli::parse_cli(&["em", "emerge", "--root", root, "--config-root", root]);
        assert_eq!(
            read_make_conf_var_for_roots(&cli.roots(), "CFLAGS")
                .await
                .as_deref(),
            Some("-O2 -march=x86-64-v3")
        );
    }

    #[tokio::test]
    async fn desired_build_env_for_roots_reads_expanded_flags() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_str().unwrap();
        let conf_dir = dir.path().join("etc/portage");
        std::fs::create_dir_all(&conf_dir).unwrap();
        std::fs::write(
            conf_dir.join("make.conf"),
            "COMMON_FLAGS=\"-O2 -march=x86-64-v3\"\nCFLAGS=\"${COMMON_FLAGS}\"\n",
        )
        .unwrap();

        let cli = crate::cli::parse_cli(&["em", "emerge", "--root", root, "--config-root", root]);
        let env = DesiredBuildEnv::for_roots(&cli.roots()).await;
        assert_eq!(env.cflags, "-O2 -march=x86-64-v3");
        let key = env.key();
        assert!(!key.is_empty());
        assert_eq!(
            key,
            portage_binpkg::build_env_key("-O2 -march=x86-64-v3", "", "", "")
        );
    }

    // Under `--target`, the sysroot's own config (`roots()`) and the host's
    // (`host_roots()`) can have genuinely different make.conf CFLAGS — the
    // fingerprint command's `--host` flag exists exactly for this split.
    #[tokio::test]
    async fn desired_build_env_host_vs_target() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_str().unwrap();
        std::fs::create_dir_all(dir.path().join("etc/portage")).unwrap();
        std::fs::write(
            dir.path().join("etc/portage/make.conf"),
            "CFLAGS=\"-O2 -march=armv8-a\"\n",
        )
        .unwrap();
        let sysroot_conf = dir.path().join("usr/riscv64-unknown-linux-gnu/etc/portage");
        std::fs::create_dir_all(&sysroot_conf).unwrap();
        std::fs::write(
            sysroot_conf.join("make.conf"),
            "CFLAGS=\"-O2 -march=rv64gcv\"\n",
        )
        .unwrap();

        // `--local` (not bare `--root`): a bare `--root`+`--target` now
        // means a board-root override, so the sysroot's own config lives at
        // the real bare `/usr/<tuple>` — a unit test can't safely write
        // there. `--local`'s own-build-context nesting still puts the
        // sysroot config under the prefix, as this fixture needs.
        let cli = crate::cli::parse_cli(&[
            "em",
            "emerge",
            "--local",
            root,
            "--config-root",
            root,
            "--target",
            "riscv64-unknown-linux-gnu",
            "-p",
            "sys-libs/zlib",
        ]);

        let target_env = DesiredBuildEnv::for_roots(&cli.roots()).await;
        let host_env = DesiredBuildEnv::for_roots(&cli.host_roots()).await;
        assert_eq!(target_env.cflags, "-O2 -march=rv64gcv");
        assert_eq!(host_env.cflags, "-O2 -march=armv8-a");
        assert_ne!(target_env.key(), host_env.key());
    }

    // Build a `--root R --config-root R` `Cli` with `R/etc/portage/make.conf`
    // (given CFLAGS) plus, optionally, `package.env` and its named env files
    // under that same `etc/portage` dir — the layout `env_files_for` (and
    // the real build path) actually reads.
    fn cli_with_make_conf_and_package_env(
        dir: &std::path::Path,
        make_conf_cflags: &str,
        package_env: &str,
        env_files: &[(&str, &str)],
    ) -> Cli {
        let conf_dir = dir.join("etc/portage");
        std::fs::create_dir_all(&conf_dir).unwrap();
        std::fs::write(
            conf_dir.join("make.conf"),
            format!("CFLAGS=\"{make_conf_cflags}\"\n"),
        )
        .unwrap();
        if !package_env.is_empty() {
            std::fs::write(conf_dir.join("package.env"), package_env).unwrap();
            std::fs::create_dir_all(conf_dir.join("env")).unwrap();
            for (name, body) in env_files {
                std::fs::write(conf_dir.join("env").join(name), body).unwrap();
            }
        }
        let root = dir.to_str().unwrap();
        crate::cli::parse_cli(&["em", "emerge", "--root", root, "--config-root", root])
    }

    #[tokio::test]
    async fn key_for_override_replaces_march() {
        let dir = tempfile::tempdir().unwrap();
        let cli = cli_with_make_conf_and_package_env(
            dir.path(),
            "-O2 -march=x86-64-v2",
            "dev-libs/foo  march_b\n",
            &[("march_b", "CFLAGS=\"-O2 -march=x86-64-v3\"\n")],
        );

        let roots = cli.roots();
        let env = DesiredBuildEnv::for_roots(&roots).await;
        let dirs = DesiredBuildEnv::portage_dirs(&roots);

        let foo = portage_atom::Cpv::parse("dev-libs/foo-1.0").unwrap();
        let bar = portage_atom::Cpv::parse("dev-libs/bar-1.0").unwrap();

        assert_eq!(
            env.key_for(&dirs, &foo).await,
            portage_binpkg::build_env_key("-O2 -march=x86-64-v3", "", "", "")
        );
        assert_ne!(env.key_for(&dirs, &foo).await, env.key());
        assert_eq!(
            env.key_for(&dirs, &bar).await,
            env.key(),
            "an unmatched package falls back to the make.conf-only key"
        );
    }

    #[tokio::test]
    async fn key_for_append_keeps_baseline() {
        let dir = tempfile::tempdir().unwrap();
        let cli = cli_with_make_conf_and_package_env(
            dir.path(),
            "-O2 -march=armv8-a",
            "dev-libs/foo  outline_atomics\n",
            &[(
                "outline_atomics",
                "CFLAGS=\"${CFLAGS} -mno-outline-atomics\"\n",
            )],
        );

        let roots = cli.roots();
        let env = DesiredBuildEnv::for_roots(&roots).await;
        let dirs = DesiredBuildEnv::portage_dirs(&roots);
        let foo = portage_atom::Cpv::parse("dev-libs/foo-1.0").unwrap();

        assert_eq!(
            env.key_for(&dirs, &foo).await,
            portage_binpkg::build_env_key("-O2 -march=armv8-a -mno-outline-atomics", "", "", "")
        );
        assert_ne!(env.key_for(&dirs, &foo).await, env.key());
    }

    #[tokio::test]
    async fn key_for_no_matching_env_file_is_baseline_key() {
        let dir = tempfile::tempdir().unwrap();
        let cli = cli_with_make_conf_and_package_env(dir.path(), "-O2 -march=x86-64-v3", "", &[]);
        let roots = cli.roots();
        let env = DesiredBuildEnv::for_roots(&roots).await;
        let dirs = DesiredBuildEnv::portage_dirs(&roots);
        let foo = portage_atom::Cpv::parse("dev-libs/foo-1.0").unwrap();
        assert_eq!(env.key_for(&dirs, &foo).await, env.key());
    }

    fn parse_sections(
        contents: &str,
    ) -> (
        std::collections::HashMap<String, std::collections::HashMap<String, String>>,
        Vec<String>,
    ) {
        let mut sections = std::collections::HashMap::new();
        let mut order = Vec::new();
        portage_repo::ini::merge_sections(&mut sections, &mut order, contents);
        (sections, order)
    }

    fn uris(entries: &[BinRepoEntry]) -> Vec<&str> {
        entries.iter().map(|e| e.sync_uri.as_str()).collect()
    }

    // The two reversals in real portage's own algorithm (`BinRepoConfigLoader`
    // assigns increasing priority walking `PORTAGE_BINHOST` *backwards*;
    // `bintree.py` then consumes the whole sorted list *reversed*) cancel out
    // for a plain `PORTAGE_BINHOST` with no `binrepos.conf` at all — verified
    // against the real source, not assumed (see `binrepo/config.py` +
    // `dbapi/bintree.py`).
    #[test]
    fn plain_portage_binhost_preserves_original_order() {
        let (sections, order) = parse_sections("");
        let result = combine_binhosts(&sections, &order, Some("A B C"));
        assert_eq!(uris(&result), vec!["A", "B", "C"]);
    }

    // A higher `priority =` in `binrepos.conf` is tried *first* (ascending
    // sort, then reversed for consumption — a higher number sorts later
    // ascending, so ends up first after the reversal).
    #[test]
    fn binrepos_conf_priority_higher_number_tried_first() {
        let (sections, order) = parse_sections(
            "[low]\nsync-uri = http://low\npriority = 1\n\n\
             [high]\nsync-uri = http://high\npriority = 10\n",
        );
        let result = combine_binhosts(&sections, &order, None);
        assert_eq!(uris(&result), vec!["http://high", "http://low"]);
    }

    // Explicit `binrepos.conf` sections (priority defaults to 0) and legacy
    // `PORTAGE_BINHOST` entries (always priority >= 1) combine correctly:
    // the `PORTAGE_BINHOST` entries outrank the unprioritized section.
    #[test]
    fn binrepos_conf_and_portage_binhost_combine() {
        let (sections, order) = parse_sections("[mine]\nsync-uri = http://mine\n");
        let result = combine_binhosts(&sections, &order, Some("http://a http://b"));
        // http://a and http://b (priority 2 and 1 respectively, per the
        // reversed-walk rule) outrank the unprioritized (priority 0) `mine`.
        assert_eq!(uris(&result), vec!["http://a", "http://b", "http://mine"]);
    }

    // A `PORTAGE_BINHOST` URL already covered by an explicit `binrepos.conf`
    // section is not duplicated.
    #[test]
    fn duplicate_sync_uri_is_not_added_twice() {
        let (sections, order) = parse_sections("[mine]\nsync-uri = http://dup\npriority = 5\n");
        let result = combine_binhosts(&sections, &order, Some("http://dup http://new"));
        assert_eq!(result.len(), 2);
        assert_eq!(uris(&result), vec!["http://dup", "http://new"]);
    }

    // A section with no `sync-uri` is skipped entirely (matching real
    // portage's own warn-and-skip behaviour), not merged with a blank URI.
    #[test]
    fn missing_sync_uri_is_skipped() {
        let (sections, order) = parse_sections("[broken]\npriority = 1\n");
        let result = combine_binhosts(&sections, &order, None);
        assert!(result.is_empty());
    }

    #[test]
    fn frozen_and_verify_signature_parsed_case_insensitively() {
        let (sections, order) =
            parse_sections("[a]\nsync-uri = http://a\nfrozen = True\nverify-signature = yes\n");
        let result = combine_binhosts(&sections, &order, None);
        assert_eq!(result.len(), 1);
        assert!(result[0].frozen);
        assert_eq!(result[0].verify_signature, Some(true));
    }

    #[test]
    fn frozen_defaults_false_and_verify_signature_unset() {
        let (sections, order) = parse_sections("[a]\nsync-uri = http://a\n");
        let result = combine_binhosts(&sections, &order, Some("http://b"));
        assert_eq!(result.len(), 2);
        assert!(!result[0].frozen);
        assert_eq!(result[0].verify_signature, None);
        assert_eq!(result[1].verify_signature, None);
    }

    #[test]
    fn default_section_is_inherited_and_overridden() {
        let (sections, order) = parse_sections(
            "[DEFAULT]\nverify-signature = true\n\n[gentoo]\nsync-uri = http://g\n\n\
             [mine]\nsync-uri = http://m\nverify-signature = false\n",
        );
        let result = combine_binhosts(&sections, &order, Some("http://implicit"));
        let by_name = |n: &str| result.iter().find(|r| r.sync_uri == n).unwrap();
        assert_eq!(by_name("http://g").verify_signature, Some(true));
        assert_eq!(by_name("http://m").verify_signature, Some(false));
        assert_eq!(by_name("http://implicit").verify_signature, None);
    }

    // Exercises the real `portage_binhosts` entry point end-to-end against a
    // real file on disk (not just `combine_binhosts`'s pure core): a real
    // `--root`, a real `etc/portage/binrepos.conf` file, real
    // `collect_conf_files`/`merge_sections` I/O.
    #[tokio::test]
    async fn portage_binhosts_reads_a_real_binrepos_conf_file() {
        assert!(
            std::env::var("PORTAGE_BINHOST").is_err(),
            "test assumes no ambient PORTAGE_BINHOST override"
        );
        let dir = tempfile::tempdir().unwrap();
        let portage_dir = dir.path().join("etc/portage");
        std::fs::create_dir_all(&portage_dir).unwrap();
        std::fs::write(
            portage_dir.join("binrepos.conf"),
            "[myhost]\nsync-uri = https://example.invalid/binhost\npriority = 3\n",
        )
        .unwrap();

        // `config()` defaults to the real host `/` for a bare `--root`
        // (portage `ROOT=`/`PORTAGE_CONFIGROOT` parity — see
        // `base_roots()`'s doc comment); `--config-root` is required here so
        // this test reads only the tempdir's own file, never the real host's
        // `/etc/portage/binrepos.conf`.
        let root = dir.path().to_str().unwrap();
        let cli = crate::cli::parse_cli(&["em", "emerge", "--root", root, "--config-root", root]);
        let result = portage_binhosts(&cli).await;
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "myhost");
        assert_eq!(result[0].sync_uri, "https://example.invalid/binhost");
    }

    const FOO_NLS: &str = "VERSION: 0\n\nCPV: app-test/foo-1.0\nIUSE: nls\n\
        PATH: app-test/foo-1.0-1.gpkg.tar\nUSE: nls\nRDEPEND: dev-libs/a:0/1=\n";

    fn policy(args: &[&str]) -> ReusePolicy {
        let cli = crate::cli::parse_cli(&[&["em", "emerge"], args, &["app-test/foo"]].concat());
        let f = cli.depgraph_flags();
        ReusePolicy::new(&cli.merge_flags(), f.newuse, f.changed_use)
    }

    fn facts(iuse: &[&str], rdepend: &str) -> EbuildFacts {
        EbuildFacts {
            iuse: iuse.iter().map(|f| f.to_string()).collect(),
            forced: HashSet::new(),
            deps: [
                portage_atom::DepEntry::parse(rdepend).unwrap(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
            ],
        }
    }

    fn reused(policy: ReusePolicy, use_flags: &[&str], facts: Option<&EbuildFacts>) -> bool {
        let idx = portage_binpkg::RemoteBinpkgIndex::new(FOO_NLS, "https://binhost.example");
        let flags: Vec<Interned<DefaultInterner>> =
            use_flags.iter().map(|f| Interned::intern(f)).collect();
        let env = DesiredBuildEnv::default();
        policy.with_query(&flags, facts, None, &env, "", |query| {
            idx.find_reusable("app-test/foo-1.0", query).is_some()
        })
    }

    #[test]
    fn usepkgonly_takes_a_binpkg_as_built_unless_use_is_respected() {
        assert!(!reused(policy(&["-k"]), &[], None));
        assert!(reused(
            policy(&["-k", "--binpkg-respect-use", "n"]),
            &[],
            None
        ));
        assert!(reused(policy(&["-K"]), &[], None));
        assert!(reused(policy(&["-G"]), &[], None));
        assert!(!reused(
            policy(&["-K", "--binpkg-respect-use=y"]),
            &[],
            None
        ));
        assert!(!reused(policy(&["-K", "-N"]), &[], None));
        assert!(!reused(policy(&["-K", "--changed-use"]), &[], None));
    }

    #[test]
    fn an_ebuild_iuse_change_rejects_a_binpkg_except_under_usepkgonly() {
        let ssl_added = facts(&["nls", "ssl"], "dev-libs/a:=");
        assert!(reused(
            policy(&["-k"]),
            &["nls"],
            Some(&facts(&["nls"], "dev-libs/a:="))
        ));
        assert!(!reused(policy(&["-k"]), &["nls"], Some(&ssl_added)));
        assert!(reused(
            policy(&["-k", "--changed-use"]),
            &["nls"],
            Some(&ssl_added)
        ));
        assert!(reused(policy(&["-K", "-N"]), &["nls"], Some(&ssl_added)));
    }

    #[test]
    fn a_binpkg_with_changed_runtime_deps_is_rejected() {
        let added = facts(&["nls"], "dev-libs/a:= dev-libs/b");
        assert!(!reused(policy(&["-k"]), &["nls"], Some(&added)));
        assert!(reused(
            policy(&["-k", "--binpkg-changed-deps=n"]),
            &["nls"],
            Some(&added)
        ));
        assert!(reused(policy(&["-K"]), &["nls"], Some(&added)));
    }

    #[test]
    fn the_preview_applies_the_chost_and_build_env_key_gates() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Packages"),
            "VERSION: 0\n\nCPV: app-test/foo-1.0\nCHOST: x86_64-pc-linux-gnu\n\
             CFLAGS: -march=x86-64-v3\nPATH: app-test/foo-1.0-1.gpkg.tar\n",
        )
        .unwrap();
        let key = portage_binpkg::build_env_key("-march=x86-64-v3", "", "", "");
        let cpv = portage_atom::Cpv::parse("app-test/foo-1.0").unwrap();
        let keys: PreviewKeys = [((false, cpv.clone()), key)].into();
        let preview = |chost: &str| BinpkgPreview {
            index: portage_binpkg::BinpkgIndex::open(dir.path()).unwrap(),
            policy: policy(&["-k"]),
            target: (DesiredBuildEnv::for_test(chost), Vec::new()),
            host: (DesiredBuildEnv::default(), Vec::new()),
        };
        let x86 = preview("x86_64-pc-linux-gnu");
        assert!(x86.reusable(&keys, false, &cpv, None, &[], None).is_some());
        assert!(
            x86.reusable(&PreviewKeys::new(), false, &cpv, None, &[], None)
                .is_none()
        );
        let aarch64 = preview("aarch64-unknown-linux-gnu");
        assert!(
            aarch64
                .reusable(&keys, false, &cpv, None, &[], None)
                .is_none()
        );
    }

    #[test]
    fn the_preview_marks_only_the_pinned_build() {
        let dir = tempfile::tempdir().unwrap();
        let entry = |id: u32, use_: &str| {
            format!(
                "CPV: app-misc/bar-1\nPATH: app-misc/bar/bar-1-{id}.gpkg.tar\nBUILD_ID: {id}\n\
                 IUSE: foo\nUSE: {use_}\n"
            )
        };
        let text = format!("VERSION: 0\n\n{}\n{}", entry(1, ""), entry(3, "foo"));
        std::fs::write(dir.path().join("Packages"), text).unwrap();
        let preview = BinpkgPreview {
            index: portage_binpkg::BinpkgIndex::open(dir.path()).unwrap(),
            policy: policy(&["-k"]),
            target: (DesiredBuildEnv::default(), Vec::new()),
            host: (DesiredBuildEnv::default(), Vec::new()),
        };
        let cpv = portage_atom::Cpv::parse("app-misc/bar-1").unwrap();
        let keys = PreviewKeys::new();
        let foo = [Interned::intern("foo")];
        assert!(
            preview
                .reusable(&keys, false, &cpv, None, &[], None)
                .is_some()
        );
        assert!(
            preview
                .reusable(&keys, false, &cpv, Some(3), &[], None)
                .is_none()
        );
        assert!(
            preview
                .reusable(&keys, false, &cpv, Some(3), &foo, None)
                .is_some()
        );
    }
}
