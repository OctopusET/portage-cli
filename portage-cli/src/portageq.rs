//! `em portageq` — Portage's own query interface (subset)
//!
//! Implements the "VDB group" of real `portageq`'s subcommands
//! (`has_version`/`best_version`/`match`/`mass_best_version`) and `envvar`.
//! The repos, metadata/contents/owners, best_visible and protect/eclass/license
//! groups are not implemented.
//!
//! The VDB commands take `<EROOT>` as their own positional argument (real
//! portageq's `uses_eroot` convention), independent of `em`'s own
//! `--root`/`--prefix`/`--local` — this applet deliberately carries none of
//! those (see `cli/topology.rs`'s module doc). `envvar` reads the config root
//! `em`'s prefix-position `--root`/`--prefix`/`--local`/`--target` select.

use std::io::Write;

use anyhow::{Context, Result};
use portage_atom::{Cpv, Dep};
use portage_vdb::{InstalledPackage, Vdb};

use crate::cli::Cli;

/// Real portageq's exit code when `<EROOT>` doesn't exist as a directory.
const EROOT_NOT_A_DIRECTORY: u8 = 64;
/// Real portageq's exit code for an atom that doesn't parse.
const INVALID_ATOM: u8 = 2;

/// Validate `<EROOT>` per real portageq: it must already exist as a
/// directory (an empty/not-yet-bootstrapped root is fine — only a
/// genuinely missing path is rejected here).
fn validate_eroot(eroot: &str) -> std::result::Result<(), u8> {
    if std::path::Path::new(eroot).is_dir() {
        Ok(())
    } else {
        eprintln!("Not a directory: '{eroot}'");
        eprintln!("Run portageq with --help for info");
        Err(EROOT_NOT_A_DIRECTORY)
    }
}

/// Every installed package under `<EROOT>/var/db/pkg`, or empty when that
/// directory doesn't exist yet (a fresh root with nothing installed is a
/// normal case, not an error — `<EROOT>` itself was already validated).
fn installed_packages(eroot: &str) -> Vec<InstalledPackage> {
    let vdb_path = format!("{}/var/db/pkg", eroot.trim_end_matches('/'));
    match Vdb::open(vdb_path.as_str()) {
        Ok(vdb) => vdb.packages().into_iter().collect(),
        Err(_) => Vec::new(),
    }
}

/// `atom` as a query: a blocker or a conditional USE dependency, which need a package to be
/// relative to, is invalid here as in portageq
fn parse_atom(atom: &str) -> std::result::Result<Dep, u8> {
    use portage_atom::UseDepKind;
    Dep::parse(atom)
        .ok()
        .filter(|dep| {
            dep.blocker.is_none()
                && dep
                    .use_deps
                    .iter()
                    .flatten()
                    .all(|ud| matches!(ud.kind, UseDepKind::Enabled | UseDepKind::Disabled))
        })
        .ok_or_else(|| {
            eprintln!("ERROR: Invalid atom: '{atom}'");
            INVALID_ATOM
        })
}

/// Whether installed `pkg` satisfies `dep`: version, slot, `::repo` and USE dependencies
fn matches(dep: &Dep, pkg: &InstalledPackage) -> bool {
    use portage_atom::interner::{DefaultInterner, Interned};
    use std::collections::HashSet;
    if !dep.matches_cpv(pkg.cpv(), pkg.slot().ok().as_ref()) {
        return false;
    }
    if dep
        .repo
        .is_some_and(|repo| pkg.repository().ok().flatten() != Some(repo))
    {
        return false;
    }
    let Some(use_deps) = dep.use_deps.as_deref().filter(|u| !u.is_empty()) else {
        return true;
    };
    let installed_use: HashSet<Interned<DefaultInterner>> =
        pkg.use_flags().unwrap_or_default().into_iter().collect();
    let installed_iuse: HashSet<Interned<DefaultInterner>> = pkg
        .iuse()
        .unwrap_or_default()
        .iter()
        .map(Interned::from)
        .collect();
    portage_repo::use_deps_satisfied(use_deps, &installed_use, &installed_iuse, &HashSet::new())
}

pub(crate) fn has_version(eroot: &str, atom: &str) -> Result<u8> {
    if let Err(code) = validate_eroot(eroot) {
        return Ok(code);
    }
    let dep = match parse_atom(atom) {
        Ok(d) => d,
        Err(code) => return Ok(code),
    };
    let pkgs = installed_packages(eroot);
    Ok(if pkgs.iter().any(|p| matches(&dep, p)) {
        0
    } else {
        1
    })
}

pub(crate) fn best_version(eroot: &str, atom: &str) -> Result<u8> {
    if let Err(code) = validate_eroot(eroot) {
        return Ok(code);
    }
    let dep = match parse_atom(atom) {
        Ok(d) => d,
        Err(code) => return Ok(code),
    };
    let pkgs = installed_packages(eroot);
    let best = pkgs
        .iter()
        .filter(|p| matches(&dep, p))
        .map(|p| p.cpv())
        .max();
    match best {
        Some(cpv) => println!("{cpv}"),
        None => println!(),
    }
    Ok(0)
}

/// `atom` empty matches every installed package. A non-empty atom that
/// isn't a plain dependency atom (e.g. `qgrep`-style `cat/*` glob syntax)
/// is not supported.
pub(crate) fn run_match(eroot: &str, atom: &str) -> Result<u8> {
    if let Err(code) = validate_eroot(eroot) {
        return Ok(code);
    }
    let pkgs = installed_packages(eroot);
    let mut cpvs: Vec<Cpv> = if atom.is_empty() {
        pkgs.iter().map(|p| p.cpv().clone()).collect()
    } else {
        let dep = match parse_atom(atom) {
            Ok(d) => d,
            Err(code) => return Ok(code),
        };
        pkgs.iter()
            .filter(|p| matches(&dep, p))
            .map(|p| p.cpv().clone())
            .collect()
    };
    cpvs.sort();
    for cpv in &cpvs {
        println!("{cpv}");
    }
    Ok(0)
}

pub(crate) fn mass_best_version(eroot: &str, atoms: &[String]) -> Result<u8> {
    if let Err(code) = validate_eroot(eroot) {
        return Ok(code);
    }
    let pkgs = installed_packages(eroot);
    for atom in atoms {
        let best = Dep::parse(atom).ok().and_then(|dep| {
            pkgs.iter()
                .filter(|p| matches(&dep, p))
                .map(|p| p.cpv())
                .max()
        });
        match best {
            Some(cpv) => println!("{atom}:{cpv}"),
            None => println!("{atom}:"),
        }
    }
    Ok(0)
}

/// `envvar [-v] NAME…`: the configuration of the config root `em`'s topology selects, as
/// Portage resolves it, with the variables it derives from the roots and repositories
pub(crate) async fn envvar(cli: &Cli, verbose: bool, names: &[String]) -> Result<u8> {
    let loaded = crate::settings::load(cli).await?;
    let empty = portage_repo::Settings::default();
    let settings = loaded.as_deref().unwrap_or(&empty);
    let mut vars: std::collections::BTreeMap<String, String> = settings
        .iter()
        .map(|(name, setting)| (name.to_owned(), setting.value.clone()))
        .collect();
    vars.extend(
        crate::settings::derived(&cli.roots())?
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value)),
    );
    let lookup = |name: &str| vars.get(name).cloned();
    let all = || vars.keys().cloned().collect();
    let mut out = std::io::stdout().lock();
    print_envvar(
        lookup,
        all,
        verbose,
        names,
        &mut out,
        &mut std::io::stderr(),
    )
    .context("writing envvar output")
}

/// `envvar`'s output for `names`, given how to look one up and list them all
///
/// A trailing `*` expands to every variable with that prefix, in sorted order (Portage's own
/// order is its dict's). An unset variable prints an empty value and makes the status 1.
fn print_envvar(
    lookup: impl Fn(&str) -> Option<String>,
    all: impl Fn() -> Vec<String>,
    verbose: bool,
    names: &[String],
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> std::io::Result<u8> {
    let mut status = 0;
    for name in names {
        if matches!(name.as_str(), "PORTDIR" | "PORTDIR_OVERLAY" | "SYNC") {
            writeln!(
                err,
                "WARNING: 'portageq envvar {name}' is deprecated. Use any of \
                 'get_repos, get_repo_path, repos_config' instead."
            )?;
        }
        let keys = match name.strip_suffix('*') {
            Some(prefix) => {
                let mut keys: Vec<String> = all()
                    .into_iter()
                    .filter(|k| k.starts_with(prefix))
                    .collect();
                keys.sort();
                keys
            }
            None => vec![name.clone()],
        };
        for key in keys {
            let value = lookup(&key).unwrap_or_else(|| {
                status = 1;
                String::new()
            });
            if verbose {
                writeln!(out, "{key}={}", shell_quote(&value))?;
            } else {
                writeln!(out, "{value}")?;
            }
        }
    }
    Ok(status)
}

/// Python's `shlex.quote`: unchanged when every character is safe, else single-quoted
fn shell_quote(value: &str) -> String {
    let safe = |c: char| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c);
    if !value.is_empty() && value.chars().all(safe) {
        value.to_owned()
    } else {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envvar_output(verbose: bool, names: &[&str]) -> (String, String, u8) {
        let vars = [
            ("CHOST", "aarch64-unknown-linux-gnu"),
            ("CHOST_arm64", "aarch64-unknown-linux-gnu"),
            ("CFLAGS", "-mcpu=ampere1a -O3 -pipe"),
            ("ARCH", "arm64"),
            ("QUOTE", "it's"),
        ];
        let lookup = |n: &str| {
            vars.iter()
                .find(|(k, _)| *k == n)
                .map(|(_, v)| v.to_string())
        };
        let all = || vars.iter().map(|(k, _)| k.to_string()).collect();
        let names: Vec<String> = names.iter().map(|n| n.to_string()).collect();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let status = print_envvar(lookup, all, verbose, &names, &mut out, &mut err).unwrap();
        (
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
            status,
        )
    }

    // Byte formats checked against portageq 3.0.82 on a Gentoo host.
    #[test]
    fn envvar_prints_values_like_portageq() {
        assert_eq!(
            envvar_output(false, &["CHOST", "NOPE", "ARCH"]),
            (
                "aarch64-unknown-linux-gnu\n\narm64\n".into(),
                String::new(),
                1
            )
        );
        assert_eq!(
            envvar_output(true, &["CHOST", "CFLAGS", "QUOTE", "NOPE"]).0,
            "CHOST=aarch64-unknown-linux-gnu\nCFLAGS='-mcpu=ampere1a -O3 -pipe'\nQUOTE='it'\"'\"'s'\nNOPE=''\n"
        );
        // shlex.quote's safe set is ASCII only.
        assert_eq!(shell_quote("caf\u{e9}"), "'caf\u{e9}'");
        assert_eq!(
            envvar_output(false, &["CHO*"]).0,
            "aarch64-unknown-linux-gnu\naarch64-unknown-linux-gnu\n"
        );
        assert!(
            envvar_output(false, &["PORTDIR"])
                .1
                .starts_with("WARNING: 'portageq envvar PORTDIR' is deprecated.")
        );
    }

    /// A throwaway VDB tree: `<tmp>/var/db/pkg/<cat>/<pf>/` with the
    /// minimal files `InstalledPackage`/`slot()` need.
    struct TestVdb {
        _tmp: tempfile::TempDir,
        eroot: String,
    }

    impl TestVdb {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            Self {
                eroot: tmp.path().to_string_lossy().into_owned(),
                _tmp: tmp,
            }
        }

        fn add(&self, cpv: &str, slot: &str) {
            let cpv_parsed = Cpv::parse(cpv).unwrap();
            let pf = format!("{}-{}", cpv_parsed.cpn.package, cpv_parsed.version);
            let dir = std::path::Path::new(&self.eroot)
                .join("var/db/pkg")
                .join(cpv_parsed.cpn.category.as_str())
                .join(&pf);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("SLOT"), format!("{slot}\n")).unwrap();
        }
    }

    #[test]
    fn has_version_hit_and_miss() {
        let vdb = TestVdb::new();
        vdb.add("dev-python/setuptools-83.0.0", "0");
        assert_eq!(has_version(&vdb.eroot, "dev-python/setuptools").unwrap(), 0);
        assert_eq!(has_version(&vdb.eroot, "dev-python/pip").unwrap(), 1);
    }

    #[test]
    fn has_version_holds_use_and_repo_deps_against_the_installed_package() {
        let vdb = TestVdb::new();
        vdb.add("app-misc/foo-1.0", "0");
        let dir = std::path::Path::new(&vdb.eroot).join("var/db/pkg/app-misc/foo-1.0");
        std::fs::write(dir.join("USE"), "foo\n").unwrap();
        std::fs::write(dir.join("IUSE"), "foo bar\n").unwrap();
        std::fs::write(dir.join("repository"), "gentoo\n").unwrap();
        let has = |atom: &str| has_version(&vdb.eroot, atom).unwrap();
        assert_eq!(has("app-misc/foo[foo]"), 0);
        assert_eq!(has("app-misc/foo[-bar]"), 0);
        assert_eq!(has("app-misc/foo[bar]"), 1);
        assert_eq!(has("app-misc/foo[baz]"), 1);
        assert_eq!(has("app-misc/foo[baz(+)]"), 0);
        assert_eq!(has("app-misc/foo::gentoo"), 0);
        assert_eq!(has("app-misc/foo::other"), 1);
        assert_eq!(has("app-misc/foo[foo?]"), 2);
        assert_eq!(has("!app-misc/foo"), 2);
    }

    #[test]
    fn has_version_invalid_atom() {
        let vdb = TestVdb::new();
        assert_eq!(has_version(&vdb.eroot, "not an atom!!").unwrap(), 2);
    }

    #[test]
    fn has_version_missing_eroot() {
        assert_eq!(
            has_version("/no/such/eroot", "dev-lang/python").unwrap(),
            64
        );
    }

    #[test]
    fn best_version_hit_exits_zero() {
        // best_version prints via println!, which these fast unit tests
        // don't capture — cpv_ordering_picks_the_highest_version below
        // exercises the actual max-by-version selection directly.
        let vdb = TestVdb::new();
        vdb.add("dev-python/setuptools-79.0.1", "0");
        vdb.add("dev-python/setuptools-83.0.0", "0");
        assert_eq!(
            best_version(&vdb.eroot, "dev-python/setuptools").unwrap(),
            0
        );
    }

    #[test]
    fn best_version_no_match_still_exits_zero() {
        let vdb = TestVdb::new();
        assert_eq!(
            best_version(&vdb.eroot, "dev-python/setuptools").unwrap(),
            0
        );
    }

    #[test]
    fn run_match_empty_atom_matches_everything() {
        let vdb = TestVdb::new();
        vdb.add("dev-python/setuptools-83.0.0", "0");
        vdb.add("dev-python/pip-26.1.2", "0");
        assert_eq!(run_match(&vdb.eroot, "").unwrap(), 0);
    }

    #[test]
    fn run_match_invalid_atom_is_rejected() {
        let vdb = TestVdb::new();
        assert_eq!(run_match(&vdb.eroot, "not an atom!!").unwrap(), 2);
    }

    #[test]
    fn mass_best_version_mixes_hits_and_misses() {
        let vdb = TestVdb::new();
        vdb.add("dev-python/setuptools-83.0.0", "0");
        let atoms = vec![
            "dev-python/setuptools".to_string(),
            "dev-python/pip".to_string(),
        ];
        assert_eq!(mass_best_version(&vdb.eroot, &atoms).unwrap(), 0);
    }

    #[test]
    fn cpv_ordering_picks_the_highest_version() {
        // Directly exercises the same `.max()` call best_version/
        // mass_best_version rely on, with an assertable return value
        // (println! output isn't easily captured in-process).
        let low = Cpv::parse("dev-python/setuptools-79.0.1").unwrap();
        let high = Cpv::parse("dev-python/setuptools-83.0.0").unwrap();
        assert_eq!(vec![&low, &high].into_iter().max(), Some(&high));
    }

    #[test]
    fn slot_file_is_written_and_readable() {
        // Sanity check on the TestVdb fixture itself: a real InstalledPackage
        // built from it must resolve the SLOT file `matches()` depends on.
        let vdb = TestVdb::new();
        vdb.add("dev-python/setuptools-83.0.0", "1");
        let pkgs = installed_packages(&vdb.eroot);
        assert_eq!(pkgs.len(), 1);
        assert_eq!(pkgs[0].slot().unwrap().to_string(), "1");
    }
}
