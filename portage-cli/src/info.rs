//! `em --info` — an `emerge --info` workalike: resolved profile/build
//! config (CHOST/CFLAGS/FEATURES/USE, with USE_EXPAND groups like
//! `VIDEO_CARDS` broken out onto their own line, exactly as portage does)
//! plus the configured repositories. Useful for bug reports and for
//! comparing against a real `emerge --info` when something like USE_EXPAND
//! derivation looks wrong. `--json` (real emerge has no equivalent) emits
//! the same data as structured JSON instead of the text layout. `-v` (also
//! no real-emerge equivalent) adds every known `@name` set, resolved —
//! see [`resolve_all_sets`].

use std::collections::BTreeMap;
use std::io::Write as _;

use anstyle::{AnsiColor, Color, Effects, Style};
use anyhow::Result;
use portage_atom::interner::{DefaultInterner, Interned};
use portage_repo::UseExpand;
use serde::Serialize;

use crate::cli::Cli;

// Real `emerge --info` has no coloring at all (verified: zero ANSI escapes
// even under --color=y) — there's no portage convention to match here, so
// this reuses `em`'s own established -pv palette instead: plain green for
// package/repo-name-shaped text (matching C_PKG), dimmed for labels. USE
// flags reuse query::depgraph::output::colorize_use_flag directly (same
// C_ON/C_OFF bold red/blue as -pv's own USE="..." line) rather than a
// second, duplicated implementation.
const C_PKG: Style = Style::new().fg_color(Some(Color::Ansi(AnsiColor::Green)));
const C_LABEL: Style = Style::new().fg_color(Some(Color::Ansi(AnsiColor::Green)));
const C_BOLD: Style = Style::new().effects(Effects::BOLD);
const C_DIM: Style = Style::new().effects(Effects::DIMMED);

/// Color each USE token via the same helper `-pv`'s own USE="..." line uses
fn colorize_flags(flags: &[String]) -> String {
    flags
        .iter()
        .map(|f| crate::query::depgraph::output::colorize_use_flag(f))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The fixed, non-verbose var list real `emerge --info` prints (its
/// `myvars` list in `_emerge/actions.py:action_info`, trimmed to what `em`
/// actually resolves — deprecated PORTDIR/SYNC-era vars and the
/// bzip2/rsync-specific ones em has no equivalent for are dropped). `USE`
/// is handled separately (it also carries the USE_EXPAND groups).
const INFO_VARS: &[&str] = &[
    "ACCEPT_KEYWORDS",
    "ACCEPT_LICENSE",
    "CBUILD",
    "CFLAGS",
    "CHOST",
    "CONFIG_PROTECT",
    "CONFIG_PROTECT_MASK",
    "CXXFLAGS",
    "DISTDIR",
    "EMERGE_DEFAULT_OPTS",
    "FCFLAGS",
    "FEATURES",
    "FFLAGS",
    "GENTOO_MIRRORS",
    "LANG",
    "LDFLAGS",
    "MAKEOPTS",
    "PKGDIR",
    "PORTAGE_TMPDIR",
    "SHELL",
];

/// Real `emerge --info`'s always-checked toolchain atoms
/// (`_emerge/actions.py:action_info`'s `myvars`), before folding in
/// `profiles/info_pkgs`.
const TOOLCHAIN_ATOMS: &[&str] = &[
    "dev-build/autoconf",
    "dev-build/automake",
    "virtual/os-headers",
    "sys-devel/binutils",
    "dev-build/libtool",
    "dev-lang/python",
];

/// `em`-specific addition to the real-portage `TOOLCHAIN_ATOMS`/`info_pkgs`
/// set: `ninja` isn't in any Gentoo profile's `info_pkgs` (verified: absent
/// from this repo's own copy), but is common enough as a meson/cmake
/// generator backend to be worth surfacing unconditionally alongside them.
const EXTRA_TOOLCHAIN_ATOMS: &[&str] = &["dev-build/ninja"];

/// Compilers worth showing even when *not* installed at all — `(not
/// installed)` instead of silently omitting the line, since "is there a
/// compiler here at all" is exactly the kind of thing `--info` exists to
/// answer at a glance, unlike the rest of `TOOLCHAIN_ATOMS`.
const ALWAYS_SHOW_COMPILERS: &[&str] = &["sys-devel/gcc", "llvm-core/clang"];

#[derive(Serialize)]
struct RepoInfo {
    name: Interned<DefaultInterner>,
    location: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    masters: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sync_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sync_uri: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    volatile: Option<bool>,
    /// Resolution order: `ReposConf::repos()` already returns repos sorted
    /// ascending by `(priority, name)` (`portage-repo`'s `repos_conf.rs`),
    /// so this is purely informational here — real `emerge --info` prints
    /// it the same way (`repository/config.py`'s `"priority: " +
    /// str(self.priority)`, only when set).
    #[serde(skip_serializing_if = "Option::is_none")]
    priority: Option<i64>,
}

/// Portage's per-package `--info` variables, shown when the build used a value other than the
/// current one
const PACKAGE_VARS: &[&str] = &["CHOST", "CFLAGS", "CXXFLAGS", "FEATURES", "LDFLAGS"];

/// One `USE`-shaped variable of a package's build: `USE` itself or a USE_EXPAND group
#[derive(Serialize, Debug, PartialEq)]
struct UseGroup {
    name: String,
    /// Enabled flags, then `-`-prefixed disabled ones, each run sorted
    flags: Vec<String>,
}

/// What an installed package was built with (`emerge --info <atom>`'s Package Settings)
#[derive(Serialize)]
struct PackageSettings {
    cpv: String,
    repo: String,
    use_flags: Vec<UseGroup>,
    vars: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    unset: Vec<String>,
}

#[derive(Serialize)]
struct BinaryRepoInfo {
    name: String,
    sync_uri: String,
    verify_signature: Option<bool>,
}

/// One `@name` set's resolved atoms, or why it couldn't be resolved —
/// `em`-specific (`-v`), no real-emerge equivalent: unlike the fixed
/// `Installed sets:` line (only the ones actually tracked in `world_sets`),
/// this lists *every* set `KnownSets` can see, whether or not `em` can
/// resolve it — exactly the "which sets does `em` actually support" question
/// that once had to be answered by a manual audit.
#[derive(Serialize)]
struct SetEntry {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    atoms: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Serialize)]
struct MemInfo {
    total_kib: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    free_kib: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    swap_total_kib: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    swap_free_kib: Option<u64>,
}

#[derive(Serialize)]
struct Info {
    em_version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    profile: Option<String>,
    has_profile: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    chost: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system_uname: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mem: Option<MemInfo>,
    repositories: Vec<RepoInfo>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    binary_repositories: Vec<BinaryRepoInfo>,
    /// `category/package` → comma-joined `version::repo` list, for the
    /// fixed toolchain-atom set real `emerge --info` always checks (see
    /// [`toolchain_versions`]).
    toolchain: BTreeMap<String, String>,
    /// `@name` entries from `var/lib/portage/world_sets` — the `@set`
    /// references the user asked `em`/`emerge` to track, as opposed to
    /// plain atoms (real emerge's `Installed sets:` line).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    installed_sets: Vec<String>,
    /// `-v`/`-vv`: every set `KnownSets` can see, resolved
    ///
    /// `None` (not merely empty) when `-v` wasn't passed, so `--json` output omits the key
    /// entirely rather than an always-empty `{}`.
    #[serde(skip_serializing_if = "Option::is_none")]
    sets: Option<BTreeMap<String, SetEntry>>,
    /// `"global"` (the plain, non-USE_EXPAND flags) plus one entry per
    /// USE_EXPAND group, keyed by its uppercase variable name (`VIDEO_CARDS`).
    use_flags: BTreeMap<String, Vec<String>>,
    vars: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    unset: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    packages: Vec<PackageSettings>,
}

/// The installed packages `atoms` name, failing after a warning for each atom naming none
fn installed_matches(
    vdb: &portage_vdb::Vdb,
    atoms: &[String],
) -> Result<Vec<portage_vdb::InstalledPackage>> {
    let mut seen = std::collections::HashSet::new();
    let mut found = Vec::new();
    let mut missing = false;
    for atom in atoms {
        let pkgs = crate::vdb::find_packages(vdb, atom);
        if pkgs.is_empty() {
            crate::style::warn_line!("there are no installed packages to satisfy \"{atom}\"");
            missing = true;
        }
        found.extend(pkgs.into_iter().filter(|p| seen.insert(p.cpv().clone())));
    }
    if missing {
        return Err(crate::error::NoValidAtoms.into());
    }
    Ok(found)
}

/// `pkg`'s build settings: its USE by group (skipping `hidden` groups and the `arch` flag), and
/// each of [`PACKAGE_VARS`] whose recorded value differs from `current`'s
fn package_settings(
    pkg: &portage_vdb::InstalledPackage,
    current: impl Fn(&str) -> Option<String>,
    expand: &UseExpand,
    hidden: &[String],
    arch: Option<&str>,
) -> Result<PackageSettings> {
    let enabled: Vec<String> = pkg
        .use_flags()?
        .iter()
        .map(|f| f.to_string())
        .filter(|f| Some(f.as_str()) != arch)
        .collect();
    let iuse: Vec<String> = pkg.iuse()?.iter().map(|f| f.name().to_string()).collect();
    let mut groups: BTreeMap<&str, (Vec<&str>, Vec<&str>)> = BTreeMap::new();
    groups.entry("global").or_default();
    for flag in &enabled {
        let (group, value) = expand.split(flag);
        groups.entry(group).or_default().0.push(value);
    }
    for flag in iuse.iter().filter(|f| !enabled.contains(f)) {
        let (group, value) = expand.split(flag);
        groups.entry(group).or_default().1.push(value);
    }
    let mut use_flags = Vec::new();
    for (group, (mut on, mut off)) in groups {
        let name = if group == "global" {
            "USE".to_string()
        } else {
            group.to_uppercase()
        };
        if hidden.iter().any(|h| h.eq_ignore_ascii_case(&name)) {
            continue;
        }
        on.sort_unstable();
        off.sort_unstable();
        let flags = on
            .iter()
            .map(|f| f.to_string())
            .chain(off.iter().map(|f| format!("-{f}")))
            .collect();
        let entry = UseGroup { name, flags };
        if entry.name == "USE" {
            use_flags.insert(0, entry);
        } else {
            use_flags.push(entry);
        }
    }

    let mut vars = BTreeMap::new();
    let mut unset = Vec::new();
    for &name in PACKAGE_VARS {
        match pkg.field(name)? {
            None => unset.push(name.to_string()),
            Some(v) => {
                let now = current(name).unwrap_or_default();
                if !v.split_whitespace().eq(now.split_whitespace()) {
                    vars.insert(name.to_string(), v);
                }
            }
        }
    }
    let repo = pkg
        .repository()?
        .map_or_else(|| "<unknown repository>".to_string(), |r| r.to_string());
    Ok(PackageSettings {
        cpv: pkg.cpv().to_string(),
        repo,
        use_flags,
        vars,
        unset,
    })
}

pub(crate) async fn run(cli: &Cli) -> Result<()> {
    let roots = cli.roots();
    let atoms = cli.atoms();
    let installed = if atoms.is_empty() {
        Vec::new()
    } else {
        installed_matches(&crate::vdb::open_cli_vdb(cli)?, &atoms)?
    };
    let repo = crate::crossdev::main_repo(cli)?;
    let loaded = crate::settings::load(cli).await?;
    let has_profile = loaded.is_some();
    let empty = portage_repo::Settings::default();
    let settings = loaded.as_deref().unwrap_or(&empty);
    let derived = crate::settings::derived(&roots)?;
    let var = |name: &str| {
        settings.get(name).map(str::to_owned).or_else(|| {
            derived
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| v.clone())
        })
    };

    let profile = crate::select::profile::current_profile(cli, &repo);
    let chost = var("CHOST");

    let repositories: Vec<RepoInfo> = match roots.repos_conf() {
        Ok(conf) => {
            let mut out = Vec::new();
            for r in conf.repos() {
                let path = r.location.as_path();
                // repos.conf's `masters =` wins over the repo's own
                // metadata/layout.conf when set (matches real portage);
                // `.volatile` (when not set explicitly) is inferred from
                // filesystem ownership, which needs the repo actually
                // opened. Fall back to the repos.conf-only fields if that
                // fails (e.g. a virtual/alias location).
                let (masters, volatile) = match path.and_then(|p| crate::repo_open::open(p).ok()) {
                    Some(opened) => (
                        r.masters
                            .clone()
                            .unwrap_or_else(|| opened.layout().masters.clone()),
                        crate::maint::sync::resolve_volatile(r, opened.path().as_std_path()),
                    ),
                    None => (
                        r.masters.clone().unwrap_or_default(),
                        r.volatile.unwrap_or(false),
                    ),
                };
                out.push(RepoInfo {
                    name: r.name,
                    location: path
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "(virtual)".to_string()),
                    masters,
                    sync_type: r.sync_type.clone().filter(|t| !t.is_empty()),
                    sync_uri: r.sync_uri.clone().filter(|u| !u.is_empty()),
                    volatile: Some(volatile),
                    priority: r.priority,
                });
            }
            out
        }
        Err(_) => Vec::new(),
    };

    let toolchain = toolchain_versions(cli, &repo, chost.as_deref()).await;

    let binary_repositories: Vec<BinaryRepoInfo> = crate::binpkg::portage_binhosts(cli)
        .await
        .into_iter()
        .map(|r| BinaryRepoInfo {
            name: r.name,
            sync_uri: r.sync_uri,
            verify_signature: r.verify_signature,
        })
        .collect();

    let installed_sets: Vec<String> = std::fs::read_to_string(
        crate::maint::world::world_sets_path(Some(roots.merge_root())),
    )
    .unwrap_or_default()
    .lines()
    .map(str::trim)
    .filter(|l| l.starts_with('@'))
    .map(str::to_string)
    .collect();

    let sets = (cli.verbose() > 0).then(|| resolve_all_sets(&roots));

    let use_expand_names = var("USE_EXPAND").unwrap_or_default();
    let use_str = var("USE").unwrap_or_default();
    let expand = UseExpand::from_var(&use_expand_names);
    let flags: Vec<&str> = use_str.split_whitespace().collect();
    let mut groups = expand.group(flags);
    let mut global: Vec<String> = groups
        .remove("global")
        .unwrap_or_default()
        .into_iter()
        .map(str::to_string)
        .collect();
    global.sort_unstable();
    let mut use_flags: BTreeMap<String, Vec<String>> = BTreeMap::new();
    use_flags.insert("global".to_string(), global);
    for (group, values) in groups {
        if values.is_empty() {
            continue;
        }
        let mut values: Vec<String> = values.into_iter().map(str::to_string).collect();
        values.sort_unstable();
        use_flags.insert(group.to_uppercase(), values);
    }

    let mut vars = BTreeMap::new();
    let mut unset = Vec::new();
    for &name in INFO_VARS {
        match var(name) {
            Some(v) => {
                vars.insert(name.to_string(), v);
            }
            None => unset.push(name.to_string()),
        }
    }
    vars.extend(var("PORTAGE_CONFIGROOT").map(|v| ("PORTAGE_CONFIGROOT".to_string(), v)));

    let hidden: Vec<String> = var("USE_EXPAND_HIDDEN")
        .unwrap_or_default()
        .split_whitespace()
        .map(str::to_string)
        .collect();
    let arch = var("ARCH");
    let packages = installed
        .iter()
        .map(|pkg| package_settings(pkg, &var, &expand, &hidden, arch.as_deref()))
        .collect::<Result<Vec<_>>>()?;

    let info = Info {
        em_version: env!("CARGO_PKG_VERSION").to_string(),
        profile,
        has_profile,
        chost,
        system_uname: system_uname(),
        mem: mem_info(),
        repositories,
        binary_repositories,
        toolchain,
        installed_sets,
        sets,
        use_flags,
        vars,
        unset,
        packages,
    };

    if cli.merge_flags().json {
        println!("{}", serde_json::to_string_pretty(&info)?);
    } else {
        print_text(&info)?;
    }
    Ok(())
}

fn print_text(info: &Info) -> Result<()> {
    let mut out = anstream::stdout();

    writeln!(
        out,
        "{C_PKG}em {} ({}{}{}){C_PKG:#}",
        info.em_version,
        info.profile.as_deref().unwrap_or("no profile set"),
        info.chost.as_deref().map(|_| ", ").unwrap_or(""),
        info.chost.as_deref().unwrap_or(""),
    )?;
    writeln!(out, "{}", "=".repeat(65))?;
    if !info.has_profile {
        use crate::style::C_WARN;
        writeln!(
            out,
            "{C_WARN}no usable profile at {}/etc/portage/make.profile — showing bare defaults{C_WARN:#}",
            info.vars.get("PORTAGE_CONFIGROOT").map_or("/", |v| v)
        )?;
    }
    if let Some(uname) = &info.system_uname {
        writeln!(out, "{C_DIM}System uname:{C_DIM:#} {uname}")?;
    }
    if let Some(mem) = &info.mem {
        match mem.free_kib {
            Some(free) => writeln!(
                out,
                "{C_DIM}KiB Mem:{C_DIM:#}   {} total ({}),  {free} free ({})",
                mem.total_kib,
                human_kib(mem.total_kib),
                human_kib(free)
            )?,
            None => writeln!(
                out,
                "{C_DIM}KiB Mem:{C_DIM:#}   {} total ({})",
                mem.total_kib,
                human_kib(mem.total_kib)
            )?,
        }
        if let (Some(total), Some(free)) = (mem.swap_total_kib, mem.swap_free_kib) {
            writeln!(
                out,
                "{C_DIM}KiB Swap:{C_DIM:#}  {total} total ({}),  {free} free ({})",
                human_kib(total),
                human_kib(free)
            )?;
        }
    }

    if !info.toolchain.is_empty() {
        writeln!(out)?;
        let width = info.toolchain.keys().map(String::len).max().unwrap_or(0);
        for (cp, versions) in &info.toolchain {
            writeln!(
                out,
                "{C_LABEL}{cp}:{C_LABEL:#}{:pad$} {versions}",
                "",
                pad = width - cp.len()
            )?;
        }
    }

    writeln!(out, "\n{C_BOLD}Repositories:{C_BOLD:#}\n")?;
    for r in &info.repositories {
        writeln!(out, "{C_PKG}{}{C_PKG:#}", r.name)?;
        writeln!(out, "    {C_DIM}location:{C_DIM:#} {}", r.location)?;
        if !r.masters.is_empty() {
            writeln!(out, "    {C_DIM}masters:{C_DIM:#} {}", r.masters.join(", "))?;
        }
        if let Some(t) = &r.sync_type {
            writeln!(out, "    {C_DIM}sync-type:{C_DIM:#} {t}")?;
        }
        if let Some(u) = &r.sync_uri {
            writeln!(out, "    {C_DIM}sync-uri:{C_DIM:#} {u}")?;
        }
        if let Some(p) = r.priority {
            writeln!(out, "    {C_DIM}priority:{C_DIM:#} {p}")?;
        }
        if let Some(v) = r.volatile {
            // Python-style capitalization, matching real emerge --info's
            // literal `volatile: True`/`volatile: False` (only the text
            // layout — JSON keeps a real bool).
            writeln!(
                out,
                "    {C_DIM}volatile:{C_DIM:#} {}",
                if v { "True" } else { "False" }
            )?;
        }
        writeln!(out)?;
    }

    if !info.binary_repositories.is_empty() {
        writeln!(out, "{C_BOLD}Binary Repositories:{C_BOLD:#}\n")?;
        for r in &info.binary_repositories {
            writeln!(out, "{C_PKG}{}{C_PKG:#}", r.name)?;
            writeln!(out, "    {C_DIM}sync-uri:{C_DIM:#} {}", r.sync_uri)?;
            writeln!(
                out,
                "    {C_DIM}verify-signature:{C_DIM:#} {}",
                match r.verify_signature {
                    Some(true) => "True",
                    Some(false) => "False",
                    None => "None",
                }
            )?;
            writeln!(out)?;
        }
    }

    if !info.installed_sets.is_empty() {
        writeln!(
            out,
            "{C_DIM}Installed sets:{C_DIM:#} {}",
            info.installed_sets.join(", ")
        )?;
    }

    if let Some(sets) = &info.sets {
        writeln!(out, "\n{C_BOLD}Sets:{C_BOLD:#}\n")?;
        for (name, entry) in sets {
            match &entry.error {
                Some(e) => writeln!(out, "{C_PKG}@{name}{C_PKG:#} {C_DIM}({e}){C_DIM:#}")?,
                None => {
                    writeln!(
                        out,
                        "{C_PKG}@{name}{C_PKG:#} {C_DIM}({} atom{}){C_DIM:#}",
                        entry.atoms.len(),
                        if entry.atoms.len() == 1 { "" } else { "s" }
                    )?;
                    for atom in &entry.atoms {
                        writeln!(out, "    {atom}")?;
                    }
                }
            }
        }
    }

    let global = info
        .use_flags
        .get("global")
        .map(|v| colorize_flags(v))
        .unwrap_or_default();
    writeln!(out, "{C_LABEL}USE{C_LABEL:#}=\"{global}\"")?;
    for (group, values) in &info.use_flags {
        if group == "global" {
            continue;
        }
        writeln!(
            out,
            "{C_LABEL}{group}{C_LABEL:#}=\"{}\"",
            colorize_flags(values)
        )?;
    }

    for (name, value) in &info.vars {
        writeln!(out, "{C_LABEL}{name}{C_LABEL:#}=\"{value}\"")?;
    }
    if !info.unset.is_empty() {
        writeln!(out, "{C_DIM}Unset:{C_DIM:#}  {}", info.unset.join(", "))?;
    }

    if !info.packages.is_empty() {
        let title = "Package Settings";
        writeln!(out, "\n{}", "=".repeat(65))?;
        writeln!(out, "{title:>40}")?;
        writeln!(out, "{}", "=".repeat(65))?;
    }
    for pkg in &info.packages {
        writeln!(
            out,
            "\n{C_PKG}{}::{}{C_PKG:#} was built with the following:",
            pkg.cpv, pkg.repo
        )?;
        let groups: Vec<String> = pkg
            .use_flags
            .iter()
            .map(|g| format!("{}=\"{}\"", g.name, colorize_flags(&g.flags)))
            .collect();
        writeln!(out, "{}", groups.join(" "))?;
        for (name, value) in &pkg.vars {
            writeln!(out, "{name}=\"{value}\"")?;
        }
        if !pkg.unset.is_empty() {
            writeln!(out, "Unset: {}", pkg.unset.join(", "))?;
        }
    }

    Ok(())
}

/// Resolve every set `crate::maint::sets::KnownSets` knows about — real
/// portage's shipped built-ins plus this root's `sets.conf`/`sets/*` —
/// through the same [`crate::maint::world::resolve_set`] the depgraph and
/// `-W` use, so this reports what `em` can *actually* resolve today, not
/// just what's configured.
///
/// `@security` goes through [`crate::glsa::security_atoms_from_roots`]
/// instead. Any other set `em` doesn't recognise shows up with its resolve
/// error rather than being silently absent from the list.
fn resolve_all_sets(roots: &portage_resolve::Roots) -> BTreeMap<String, SetEntry> {
    let eroot = roots.merge_root();
    let known = crate::maint::sets::KnownSets::load(Some(eroot));
    known
        .iter()
        .map(|name| {
            // `@security` spans the GLSA repo + arch + VDB worlds, so it goes
            // through glsa::security_atoms_from_roots, not resolve_set.
            let result = if name == "security" {
                crate::glsa::security_atoms_from_roots(roots)
            } else {
                crate::maint::world::resolve_set(roots.config(), eroot, name)
            };
            let entry = match result {
                Ok(atoms) => {
                    let mut atoms: Vec<String> = atoms.iter().map(|d| d.to_string()).collect();
                    atoms.sort_unstable();
                    SetEntry { atoms, error: None }
                }
                Err(e) => SetEntry {
                    atoms: Vec::new(),
                    error: Some(describe_set_failure(&e, name, &known)),
                },
            };
            (name.to_string(), entry)
        })
        .collect()
}

/// Why a set in the known list didn't resolve, as displayed after its name
///
/// Portage's own `.conf` files declare sets by Python class
/// (`class = portage.sets.dbapi.ChangedDepsSet`), so `em` advertises names it
/// has no resolver for. `SetResolver` can only report those as absent —
/// `KnownSets` is the half that knows Portage declares them, so they read as
/// unimplemented rather than as a real Portage set being unknown.
fn describe_set_failure(
    err: &anyhow::Error,
    name: &str,
    known: &crate::maint::sets::KnownSets,
) -> String {
    if known.is_declared(name)
        && err
            .downcast_ref::<portage_repo::Error>()
            .is_some_and(|e| matches!(e, portage_repo::Error::UnknownSet(_)))
    {
        return "not implemented".to_string();
    }
    format!("not resolvable: {err:#}")
}

/// Installed-version summary for real `emerge --info`'s toolchain-package
/// block (`TOOLCHAIN_ATOMS` + the repo's own `profiles/info_pkgs`), e.g.
/// `sys-devel/gcc: 15.2.1_p20260529::gentoo, 16.1.1_p20260613::gentoo`.
///
/// Simplification vs real portage: a bare atom (`virtual/os-headers`) is
/// matched directly against the VDB rather than first resolved through
/// `expand_new_virt` to whichever real package provides the virtual — em
/// has no such GLEP-virtual-provider mapping. Most `info_pkgs` entries are
/// real packages anyway, so this only affects a couple of `virtual/*` rows
/// (they show as unmatched instead of resolving to their provider).
async fn toolchain_versions(
    cli: &Cli,
    repo: &portage_repo::Repository,
    chost: Option<&str>,
) -> BTreeMap<String, String> {
    let mut atoms: std::collections::HashSet<String> = TOOLCHAIN_ATOMS
        .iter()
        .chain(EXTRA_TOOLCHAIN_ATOMS)
        .chain(ALWAYS_SHOW_COMPILERS)
        .map(|s| s.to_string())
        .collect();
    if let Ok(content) = std::fs::read_to_string(repo.path().join("profiles/info_pkgs")) {
        for line in content.lines() {
            let line = line.trim();
            if !line.is_empty() && !line.starts_with('#') {
                atoms.insert(line.to_string());
            }
        }
    }

    let Ok(vdb) = crate::vdb::open_cli_vdb(cli) else {
        return BTreeMap::new();
    };
    let installed: Vec<portage_vdb::InstalledPackage> = vdb.packages().into_iter().collect();

    let mut by_cp: BTreeMap<String, std::collections::BTreeSet<String>> = BTreeMap::new();
    for atom_str in &atoms {
        let Ok(dep) = portage_atom::Dep::parse(atom_str) else {
            continue;
        };
        for pkg in &installed {
            if !dep.matches_cpv(pkg.cpv(), pkg.slot().ok().as_ref()) {
                continue;
            }
            let repo_name = pkg.field("repository").ok().flatten().unwrap_or_default();
            by_cp
                .entry(pkg.cpn().to_string())
                .or_default()
                .insert(format!("{}::{repo_name}", pkg.cpv().version));
        }
    }

    // `sys-devel/gcc`/`llvm-core/clang`: show explicitly even when absent,
    // so "is there a compiler here at all" is answered at a glance instead
    // of the line just quietly not appearing.
    for &cp in ALWAYS_SHOW_COMPILERS {
        by_cp.entry(cp.to_string()).or_default();
    }

    // The active gcc-config slot, appended to sys-devel/gcc's own line —
    // "which of the installed versions is actually in use."
    if let Some(chost) = chost
        && let Some(slot) = crate::select::compiler::current_slot(&cli.roots(), chost)
    {
        by_cp
            .entry("sys-devel/gcc".to_string())
            .or_default()
            .insert(format!("(active: {slot})"));
    }

    by_cp
        .into_iter()
        .map(|(cp, vers)| {
            let joined = if vers.is_empty() {
                "(not installed)".to_string()
            } else {
                vers.into_iter().collect::<Vec<_>>().join(", ")
            };
            (cp, joined)
        })
        .collect()
}

/// `Linux-<release>-<machine>`-shaped, matching the style real `emerge
/// --info`'s `platform.platform()` line uses — via `uname`, not a new crate
/// dependency for a single diagnostic line. `None` if `uname` isn't on
/// `PATH` (never fatal: `--info` still prints everything else).
fn system_uname() -> Option<String> {
    let output = std::process::Command::new("uname")
        .args(["-s", "-r", "-m"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let parts: Vec<&str> = std::str::from_utf8(&output.stdout)
        .ok()?
        .split_whitespace()
        .collect();
    let [sysname, release, machine] = parts.as_slice() else {
        return None;
    };
    Some(format!("{sysname}-{release}-{machine}"))
}

/// Auto-scale a KiB count to the largest binary unit that keeps the value
/// readable (KiB/MiB/GiB/TiB, one decimal place). Text-display-only — JSON
/// output keeps `MemInfo`'s fields as raw KiB integers, matching this
/// module's existing raw-JSON/human-text split (e.g. `volatile`'s
/// True/False capitalization).
fn human_kib(kib: u64) -> String {
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    let mut value = kib as f64;
    let mut unit = UNITS[0];
    for &next in &UNITS[1..] {
        if value < 1024.0 {
            break;
        }
        value /= 1024.0;
        unit = next;
    }
    format!("{value:.1} {unit}")
}

/// `/proc/meminfo` total/available mem *and* swap, in KiB (Linux-only,
/// matching this project's Linux focus — see `no-emerge-equivalents-in-help`
/// and the `--prefix`/macOS support notes elsewhere: full parity isn't the
/// goal on non-Linux hosts). `None` when `/proc/meminfo` isn't readable.
fn mem_info() -> Option<MemInfo> {
    let content = std::fs::read_to_string("/proc/meminfo").ok()?;
    let mut total = None;
    let mut free = None;
    let mut swap_total = None;
    let mut swap_free = None;
    for line in content.lines() {
        let parse = |rest: &str| rest.split_whitespace().next().and_then(|v| v.parse().ok());
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            total = parse(rest);
        } else if let Some(rest) = line.strip_prefix("MemAvailable:") {
            free = parse(rest);
        } else if let Some(rest) = line.strip_prefix("SwapTotal:") {
            swap_total = parse(rest);
        } else if let Some(rest) = line.strip_prefix("SwapFree:") {
            swap_free = parse(rest);
        }
    }
    total.map(|total_kib| MemInfo {
        total_kib,
        free_kib: free,
        swap_total_kib: swap_total,
        swap_free_kib: swap_free,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // A minimal root `resolve_all_sets` can work against: an empty (but
    // valid) profile dir, one user-defined set, and an empty VDB so
    // `@preserved-rebuild` resolves cleanly instead of erroring on a
    // missing `var/db/pkg`.
    fn scratch_root() -> (tempfile::TempDir, camino::Utf8PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let root = camino::Utf8PathBuf::try_from(tmp.path().to_owned()).unwrap();
        std::fs::create_dir_all(root.join("etc/portage/make.profile")).unwrap();
        std::fs::create_dir_all(root.join("etc/portage/sets")).unwrap();
        std::fs::create_dir_all(root.join("var/db/pkg")).unwrap();
        std::fs::write(root.join("etc/portage/sets/myuserset"), "app-shells/bash\n").unwrap();
        (tmp, root)
    }

    #[test]
    fn package_settings_shows_use_groups_and_the_flags_that_changed() {
        let (_tmp, root) = scratch_root();
        let dir = root.join("var/db/pkg/app-misc/foo-1.0");
        std::fs::create_dir_all(&dir).unwrap();
        for (file, text) in [
            ("SLOT", "0"),
            ("EAPI", "8"),
            ("repository", "gentoo"),
            ("USE", "amd64 ssl video_cards_amdgpu abi_x86_64"),
            (
                "IUSE",
                "ssl gtk video_cards_amdgpu video_cards_intel abi_x86_64",
            ),
            ("CFLAGS", "-O2  -pipe"),
            ("CXXFLAGS", "-O3"),
            ("CHOST", "x86_64-pc-linux-gnu"),
        ] {
            std::fs::write(dir.join(file), text).unwrap();
        }
        let vdb = portage_vdb::Vdb::open(root.join("var/db/pkg")).unwrap();
        let pkgs = installed_matches(&vdb, &["foo".to_string()]).unwrap();
        assert!(installed_matches(&vdb, &["foo".to_string(), "bar".to_string()]).is_err());
        let current = |name: &str| match name {
            "CFLAGS" => Some("-O2 -pipe".to_string()),
            "CXXFLAGS" => Some("-O2 -pipe".to_string()),
            _ => None,
        };
        let expand = UseExpand::from_var("VIDEO_CARDS ABI_X86");
        let got = package_settings(
            &pkgs[0],
            current,
            &expand,
            &["ABI_X86".to_string()],
            Some("amd64"),
        )
        .unwrap();
        assert_eq!(got.cpv, "app-misc/foo-1.0");
        assert_eq!(got.repo, "gentoo");
        let group = |name: &str, flags: &[&str]| UseGroup {
            name: name.to_string(),
            flags: flags.iter().map(|f| f.to_string()).collect(),
        };
        assert_eq!(
            got.use_flags,
            [
                group("USE", &["ssl", "-gtk"]),
                group("VIDEO_CARDS", &["amdgpu", "-intel"])
            ]
        );
        assert_eq!(
            got.vars.into_iter().collect::<Vec<_>>(),
            [
                ("CHOST".to_string(), "x86_64-pc-linux-gnu".to_string()),
                ("CXXFLAGS".to_string(), "-O3".to_string())
            ]
        );
        assert_eq!(got.unset, ["FEATURES", "LDFLAGS"]);
    }

    #[test]
    fn resolve_all_sets_reports_both_resolved_and_unresolvable_sets() {
        let (_tmp, root) = scratch_root();
        // `for_test` alone leaves `config()` unset, which makes
        // `resolve_set` fall back to the *real* host `/etc/portage` for the
        // profile stack — on a dev box with Gentoo actually installed that
        // silently succeeds and masks the bug, but on a bare CI runner (no
        // `/etc/portage/make.profile` at all) it fails, so every set here
        // errors instead of resolving against this scratch root's own
        // (empty but valid) profile dir. `with_config` pins it down.
        let roots = portage_resolve::Roots::for_test(root.as_str()).with_config(Some(root.clone()));
        let sets = resolve_all_sets(&roots);

        let myset = sets.get("myuserset").expect("user set is known");
        assert_eq!(myset.atoms, vec!["app-shells/bash".to_string()]);
        assert!(myset.error.is_none());

        // Always known (`KnownSets`), and now resolvable even with no
        // profile-based `SetResolver` match arm for it.
        let preserved = sets.get("preserved-rebuild").expect("always known");
        assert!(preserved.atoms.is_empty());
        assert!(preserved.error.is_none());
    }

    #[test]
    fn resolve_all_sets_is_none_shaped_correctly_when_unresolvable() {
        // A set name `KnownSets` sees via a `sets.conf` section that `em` has
        // no backing implementation for resolves to an error rather than
        // vanishing — the shape a real host used to show for `@security`
        // before it was wired to the GLSA subsystem. Any unimplemented
        // `@name` `SetResolver`/`resolve_vdb_set` don't recognise behaves
        // this way; use a clearly-fake one so the test doesn't depend on the
        // real `@security` resolution (empty when no GLSAs apply).
        let (_tmp, root) = scratch_root();
        std::fs::write(
            root.join("etc/portage/sets.conf"),
            "[builtinfake]\nclass = portage.sets.builtin.FakeSet\n",
        )
        .unwrap();

        // See the comment above `with_config` in the previous test — same
        // host-`/etc/portage` leak, avoided the same way.
        let roots = portage_resolve::Roots::for_test(root.as_str()).with_config(Some(root.clone()));
        let sets = resolve_all_sets(&roots);

        let fake = sets.get("builtinfake").expect("sets.conf section is known");
        assert!(fake.atoms.is_empty());
        assert!(fake.error.is_some());
    }
}
