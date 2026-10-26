//! `-c`/`--depclean`: remove installed packages nothing in `@world`'s
//! (`@selected ∪ @system`) installed-dependency closure still needs —
//! matching real portage's `_emerge/actions.py::_calc_depclean`, read
//! rather than guessed from behavior.
//!
//! The real algorithm: seed a "required" set from `@world` (`@system` and the selected sets
//! when atoms are given), walk every requirement's *installed* `RDEPEND`/`IDEPEND`/`PDEPEND`
//! (`DEPEND`/`BDEPEND` excluded by default — build-time-only, the classic thing depclean
//! clears out) transitively over what's actually installed, and anything
//! installed but never reached is removal-eligible. When atoms are given
//! on the command line, every *other* installed package is folded into
//! the required set unconditionally, narrowing what depclean will even
//! consider (`_calc_depclean`'s `args_set`/`protected_set` handling).
//!
//! Deliberately simplified vs. real portage (documented, not silent):
//! no USE-dep bracket verification when matching an atom against an
//! installed package (only CPN/version/slot via `Dep::matches_cpv`) — a
//! mismatched USE-dep atom still counts its target as kept, over-keeping
//! rather than risking an under-keep. No virtual/provider indirection
//! beyond literal atom matching. A `||` group walks *every* branch rather
//! than preferring the one already satisfied by what's installed — also
//! biased toward over-keeping, but coarser than the other
//! simplifications: it can leave real orphans (an alternative branch's
//! otherwise-unneeded package) un-removed where real depclean would
//! clean them. No `-P`/`--prune`.

use std::collections::{HashMap, HashSet, VecDeque};

use anyhow::{Context, Result, bail};

use portage_atom::interner::{DefaultInterner, Interned};
use portage_atom::{Cpv, Dep, DepEntry};
use portage_vdb::InstalledPackage;

use crate::cli;
use crate::emerge::parse_atoms_strict;
use crate::vdb::open_cli_vdb;

/// Evaluate `pkg`'s own `RDEPEND`/`PDEPEND` (+ `IDEPEND` under `with_idepend`, `DEPEND`/`BDEPEND`
/// under `with_bdeps`) against its own recorded `USE`. `UseConditional`s are resolved by
/// `DepEntry::evaluate_use`, matching the convention
/// `bdepend_avail.rs`'s `collect_unsatisfied` already relies on.
fn own_entries(pkg: &InstalledPackage, with_idepend: bool, with_bdeps: bool) -> Vec<DepEntry> {
    let use_set: HashSet<Interned<DefaultInterner>> = pkg
        .use_flags()
        .unwrap_or_default()
        .iter()
        .map(|f| Interned::intern(f))
        .collect();

    let mut entries = Vec::new();
    if let Ok(Some(e)) = pkg.rdepend() {
        entries.extend(e);
    }
    if with_idepend && let Ok(Some(e)) = pkg.idepend() {
        entries.extend(e);
    }
    if let Ok(Some(e)) = pkg.pdepend() {
        entries.extend(e);
    }
    if with_bdeps {
        if let Ok(Some(e)) = pkg.depend() {
            entries.extend(e);
        }
        if let Ok(Some(e)) = pkg.bdepend() {
            entries.extend(e);
        }
    }

    DepEntry::evaluate_use(&entries, &use_set)
}

/// [`own_entries`] flattened into concrete, non-blocker atoms.
fn own_atoms(pkg: &InstalledPackage, with_bdeps: bool) -> Vec<Dep> {
    let mut atoms = Vec::new();
    collect_atoms(&own_entries(pkg, true, with_bdeps), &mut atoms);
    atoms
}

/// Whether `entry` is satisfied by `installed`; pushes each unsatisfied atom or `||` group to `out`.
fn satisfied(entry: &DepEntry, installed: &[InstalledPackage], out: &mut Vec<String>) -> bool {
    match entry {
        DepEntry::Atom(dep) if dep.blocker.is_none() => {
            let ok = matching(dep, installed).next().is_some();
            if !ok {
                out.push(dep.to_string());
            }
            ok
        }
        DepEntry::AnyOf(c) | DepEntry::ExactlyOneOf(c) => {
            let ok = c.iter().any(|e| satisfied(e, installed, &mut Vec::new()));
            if !ok {
                out.push(entry.to_string());
            }
            ok
        }
        DepEntry::AllOf(c) => {
            let mut ok = true;
            for e in c {
                ok &= satisfied(e, installed, out);
            }
            ok
        }
        _ => true,
    }
}

/// Runtime dependencies of the packages depclean keeps that nothing installed satisfies,
/// as `(dependency, parent)` pairs
///
/// `IDEPEND` resolves against the running root, so it is checked only when this VDB is that
/// root's (`with_idepend`). USE-dep brackets are not checked, so this can under-report.
fn unresolved_deps(
    installed: &[InstalledPackage],
    cleanlist: &[InstalledPackage],
    with_idepend: bool,
) -> Vec<(String, Cpv)> {
    let removed: HashSet<&Cpv> = cleanlist.iter().map(|p| p.cpv()).collect();
    let mut out = Vec::new();
    for pkg in installed.iter().filter(|p| !removed.contains(p.cpv())) {
        let mut missing = Vec::new();
        for entry in own_entries(pkg, with_idepend, false) {
            satisfied(&entry, installed, &mut missing);
        }
        out.extend(missing.into_iter().map(|m| (m, pkg.cpv().clone())));
    }
    out
}

/// [`unresolved_deps`] that block the clean: none for a targeted clean, which portage lets
/// proceed over missing dependencies (`allow_missing_deps`).
fn blocking_unresolved(
    targeted: bool,
    installed: &[InstalledPackage],
    cleanlist: &[InstalledPackage],
    with_idepend: bool,
) -> Vec<(String, Cpv)> {
    if targeted {
        return Vec::new();
    }
    unresolved_deps(installed, cleanlist, with_idepend)
}

/// Flatten a (already USE-evaluated) `DepEntry` tree into concrete atoms, dropping blockers
///
/// A `||` group walks every branch (see module doc: an accepted over-keep simplification,
/// not resolver-accurate choice).
fn collect_atoms(entries: &[DepEntry], out: &mut Vec<Dep>) {
    for e in entries {
        match e {
            DepEntry::Atom(dep) if dep.blocker.is_none() => out.push(dep.clone()),
            DepEntry::Atom(_) => {}
            DepEntry::AllOf(c)
            | DepEntry::AnyOf(c)
            | DepEntry::ExactlyOneOf(c)
            | DepEntry::AtMostOneOf(c) => collect_atoms(c, out),
            DepEntry::UseConditional { .. } => {} // evaluate_use already stripped these
        }
    }
}

/// Every installed package `dep` matches (CPN/version/slot only — see
/// module doc on USE-dep brackets).
fn matching<'a>(
    dep: &Dep,
    installed: &'a [InstalledPackage],
) -> impl Iterator<Item = &'a InstalledPackage> {
    installed
        .iter()
        .filter(move |p| dep.matches_cpv(p.cpv(), p.slot().ok().as_ref()))
}

/// Everything reachable from `world_atoms` (+ `exclude_atoms`), walking the *installed*
/// dependency graph only
///
/// When `target_atoms` is non-empty, every installed package not matching one of them is
/// folded into the reachable set first (real portage's `args_set`/`protected_set`: narrows
/// candidates without treating anything else as fair game). Returns the packages installed
/// but never reached.
fn compute_cleanlist(
    installed: &[InstalledPackage],
    world_atoms: &[Dep],
    exclude_atoms: &[Dep],
    target_atoms: &[Dep],
    with_bdeps: bool,
) -> Vec<InstalledPackage> {
    let mut kept: HashSet<Cpv> = HashSet::new();
    let mut queue: VecDeque<Cpv> = VecDeque::new();

    let keep = |pkg: &InstalledPackage, kept: &mut HashSet<Cpv>, queue: &mut VecDeque<Cpv>| {
        if kept.insert(pkg.cpv().clone()) {
            queue.push_back(pkg.cpv().clone());
        }
    };
    // A set or dependency atom pulls in one package, as the resolver would pick: the highest
    // installed match, so an unslotted atom does not keep every older slot.
    let best = |dep: &Dep| matching(dep, installed).max_by(|a, b| a.cpv().cmp(b.cpv()));

    for dep in world_atoms {
        if let Some(pkg) = best(dep) {
            keep(pkg, &mut kept, &mut queue);
        }
    }
    for dep in exclude_atoms {
        for pkg in matching(dep, installed) {
            keep(pkg, &mut kept, &mut queue);
        }
    }

    if !target_atoms.is_empty() {
        for pkg in installed {
            let is_target = target_atoms
                .iter()
                .any(|d| d.matches_cpv(pkg.cpv(), pkg.slot().ok().as_ref()));
            if !is_target {
                keep(pkg, &mut kept, &mut queue);
            }
        }
    }

    while let Some(cpv) = queue.pop_front() {
        let Some(pkg) = installed.iter().find(|p| *p.cpv() == cpv) else {
            continue;
        };
        for dep in own_atoms(pkg, with_bdeps) {
            if let Some(dep_pkg) = best(&dep) {
                keep(dep_pkg, &mut kept, &mut queue);
            }
        }
    }

    installed
        .iter()
        .filter(|p| !kept.contains(p.cpv()))
        .cloned()
        .collect()
}

/// Safe removal order within `cleanlist`: a dependent (something in
/// `cleanlist` that depends on another `cleanlist` member) is ordered
/// before the dependency it needs, via a Kahn's-algorithm topological sort.
///
/// Any leftover from a genuine dependency cycle within `cleanlist` (rare) is
/// appended in original order — safe either way, since preserve-libs still
/// protects any file still physically needed regardless of order.
fn removal_order(cleanlist: &[InstalledPackage], with_bdeps: bool) -> Vec<InstalledPackage> {
    let by_cpv: HashMap<Cpv, InstalledPackage> = cleanlist
        .iter()
        .map(|p| (p.cpv().clone(), p.clone()))
        .collect();
    let mut out_edges: HashMap<Cpv, Vec<Cpv>> = HashMap::new();
    let mut in_degree: HashMap<Cpv, usize> =
        cleanlist.iter().map(|p| (p.cpv().clone(), 0)).collect();

    for pkg in cleanlist {
        let mut targets = HashSet::new();
        for dep in own_atoms(pkg, with_bdeps) {
            for other in cleanlist {
                if other.cpv() != pkg.cpv()
                    && dep.matches_cpv(other.cpv(), other.slot().ok().as_ref())
                {
                    targets.insert(other.cpv().clone());
                }
            }
        }
        for t in &targets {
            *in_degree.entry(t.clone()).or_insert(0) += 1;
        }
        out_edges.insert(pkg.cpv().clone(), targets.into_iter().collect());
    }

    let mut queue: VecDeque<Cpv> = in_degree
        .iter()
        .filter(|&(_, d)| *d == 0)
        .map(|(c, _)| c.clone())
        .collect();
    let mut order: Vec<Cpv> = Vec::with_capacity(cleanlist.len());
    while let Some(cpv) = queue.pop_front() {
        if let Some(targets) = out_edges.get(&cpv) {
            for t in targets {
                if let Some(d) = in_degree.get_mut(t) {
                    *d -= 1;
                    if *d == 0 {
                        queue.push_back(t.clone());
                    }
                }
            }
        }
        order.push(cpv);
    }
    let ordered: HashSet<Cpv> = order.iter().cloned().collect();
    for pkg in cleanlist {
        if !ordered.contains(pkg.cpv()) {
            order.push(pkg.cpv().clone());
        }
    }

    order
        .into_iter()
        .filter_map(|cpv| by_cpv.get(&cpv).cloned())
        .collect()
}

/// The required-set seed of a targeted depclean: `@system` plus the sets `world_sets` names,
/// without the `world` file atoms, so a selected target is removable (and then deselected).
fn targeted_seed(
    config_root: Option<&camino::Utf8Path>,
    root: &camino::Utf8Path,
) -> Result<Vec<Dep>> {
    let mut atoms = crate::maint::world::resolve_set(config_root, root, "system")
        .context("depclean: cannot read @system")?;
    atoms.extend(
        crate::maint::world::resolve_set(config_root, root, "selected-sets")
            .context("depclean: cannot read @selected-sets")?,
    );
    Ok(atoms)
}

/// `-c`/`--depclean` / `em depclean [atoms]`: mirrors `emerge.rs::unmerge_atoms`'s
/// own structure (root/shell setup, `-p`/`--ask` gating, preserve-libs
/// reporting) but computes its own removal list instead of taking it from
/// the command line directly. Takes the target atom list and merge flags
/// explicitly — both `em -c` (`EmergeArgs`'s own fields) and `em depclean
/// <atoms>` (its own, identically-shaped fields) forward through here.
pub async fn run_with_targets(
    cli: &cli::Cli,
    raw_targets: &[String],
    merge_flags: &cli::MergeFlags,
) -> Result<()> {
    let vdb = open_cli_vdb(cli)?;
    let roots = cli.roots();
    let root = roots.merge_root().to_owned();

    // `@world` (`@selected ∪ @system`) — the seed of the required-set walk
    // above; `@system` is part of it on purpose here, unlike the display-only
    // `@selected` read in `query::depgraph`.
    let world_atoms = crate::maint::world::resolve_set(roots.config(), &root, "world")
        .context("depclean: cannot read @world")?;
    if world_atoms.is_empty() {
        bail!("@world is empty — refusing to depclean (see `man emerge`'s --depclean safety note)");
    }
    // Strict parse: a typo on the command line must not degrade a targeted
    // depclean into a full-system clean (empty target_atoms ⇒ unrestricted).
    let exclude_atoms = parse_atoms_strict(&merge_flags.exclude)?;
    let target_atoms = parse_atoms_strict(raw_targets)?;
    if !raw_targets.is_empty() && target_atoms.is_empty() {
        // Unreachable with strict parse (non-empty raw always yields non-empty
        // or Err), kept as a belt-and-braces guard against future refactors.
        bail!("depclean: no valid target atoms");
    }
    let with_bdeps = merge_flags.depclean_bdeps();
    let seed_atoms = if target_atoms.is_empty() {
        world_atoms
    } else {
        targeted_seed(roots.config(), &root)?
    };

    let installed: Vec<InstalledPackage> = vdb.packages().into_iter().collect();
    let cleanlist = compute_cleanlist(
        &installed,
        &seed_atoms,
        &exclude_atoms,
        &target_atoms,
        with_bdeps,
    );
    let unresolved = blocking_unresolved(
        !target_atoms.is_empty(),
        &installed,
        &cleanlist,
        root == "/",
    );
    if !unresolved.is_empty() {
        crate::style::error_line!(
            "Dependencies could not be completely resolved due to the following required \
             packages not being installed:"
        );
        for (dep, parent) in &unresolved {
            crate::style::ewarn_sub_bullet!("{dep} pulled in by: {parent}");
        }
        bail!("depclean: installed packages have unsatisfied dependencies; update @world first");
    }
    let cleanlist = removal_order(&cleanlist, with_bdeps);

    if cleanlist.is_empty() {
        println!(">>> Nothing to depclean.");
        return Ok(());
    }

    println!("\n>>> These are the packages that would be removed:\n");
    for pkg in &cleanlist {
        println!(" {pkg}");
    }
    println!("\n>>> {} package(s) would be removed.", cleanlist.len());

    let exclude: HashSet<Cpv> = cleanlist.iter().map(|p| p.cpv().clone()).collect();

    crate::emerge::run_unmerge_batch(cli, &vdb, &cleanlist, &exclude, "depclean", "Depcleaning")
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use portage_vdb::Vdb;

    // Write one fake installed package into `vdb_root/<cat>/<pf>/`:
    // `SLOT`, `USE`, `RDEPEND`, `PDEPEND` — everything `own_atoms`/
    // `compute_cleanlist` reads. Mirrors `preserve_libs::tests`'
    // `write_fake_package` technique.
    fn write_fake_package(
        vdb_root: &std::path::Path,
        cat: &str,
        pf: &str,
        rdepend: &str,
        pdepend: &str,
    ) {
        let dir = vdb_root.join(cat).join(pf);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SLOT"), "0").unwrap();
        std::fs::write(dir.join("USE"), "").unwrap();
        std::fs::write(dir.join("RDEPEND"), rdepend).unwrap();
        std::fs::write(dir.join("PDEPEND"), pdepend).unwrap();
    }

    fn open_vdb(dir: &std::path::Path) -> Vdb {
        let root = camino::Utf8PathBuf::try_from(dir.to_owned()).unwrap();
        Vdb::open(root.join("var/db/pkg")).unwrap()
    }

    #[test]
    fn keeps_world_reachable_chain_removes_true_orphan() {
        let tmp = tempfile::tempdir().unwrap();
        let vdb_root = tmp.path().join("var/db/pkg");

        write_fake_package(&vdb_root, "app-misc", "topapp-1.0", "dev-libs/needed", "");
        write_fake_package(&vdb_root, "dev-libs", "needed-1.0", "", "");
        write_fake_package(&vdb_root, "dev-libs", "orphan-1.0", "", "");

        let vdb = open_vdb(tmp.path());
        let installed: Vec<InstalledPackage> = vdb.packages().into_iter().collect();
        let world = vec![Dep::parse("app-misc/topapp").unwrap()];

        let cleanlist = compute_cleanlist(&installed, &world, &[], &[], false);

        assert_eq!(cleanlist.len(), 1);
        assert_eq!(cleanlist[0].cpv().cpn.to_string(), "dev-libs/orphan");
    }

    #[test]
    fn unslotted_atom_keeps_only_the_highest_slot() {
        let tmp = tempfile::tempdir().unwrap();
        let vdb_root = tmp.path().join("var/db/pkg");
        write_fake_package(&vdb_root, "dev-lang", "python-3.11.9", "", "");
        write_fake_package(&vdb_root, "dev-lang", "python-3.12.4", "", "");
        std::fs::write(vdb_root.join("dev-lang/python-3.11.9/SLOT"), "3.11").unwrap();
        std::fs::write(vdb_root.join("dev-lang/python-3.12.4/SLOT"), "3.12").unwrap();
        write_fake_package(
            &vdb_root,
            "app-misc",
            "tool-1.0",
            "dev-lang/python:3.11",
            "",
        );

        let vdb = open_vdb(tmp.path());
        let installed: Vec<InstalledPackage> = vdb.packages().into_iter().collect();
        let world = vec![Dep::parse("dev-lang/python").unwrap()];
        let cleanlist = compute_cleanlist(&installed, &world, &[], &[], false);
        let mut names: Vec<String> = cleanlist.iter().map(|p| p.cpv().to_string()).collect();
        names.sort();
        assert_eq!(names, ["app-misc/tool-1.0", "dev-lang/python-3.11.9"]);

        let world = vec![
            Dep::parse("dev-lang/python").unwrap(),
            Dep::parse("app-misc/tool").unwrap(),
        ];
        assert!(compute_cleanlist(&installed, &world, &[], &[], false).is_empty());
    }

    #[test]
    fn idepend_keeps_its_target() {
        let tmp = tempfile::tempdir().unwrap();
        let vdb_root = tmp.path().join("var/db/pkg");
        write_fake_package(&vdb_root, "app-misc", "topapp-1.0", "", "");
        std::fs::write(
            vdb_root.join("app-misc/topapp-1.0/IDEPEND"),
            "dev-util/tool",
        )
        .unwrap();
        write_fake_package(&vdb_root, "dev-util", "tool-1.0", "", "");

        let vdb = open_vdb(tmp.path());
        let installed: Vec<InstalledPackage> = vdb.packages().into_iter().collect();
        let world = vec![Dep::parse("app-misc/topapp").unwrap()];
        assert!(compute_cleanlist(&installed, &world, &[], &[], false).is_empty());
    }

    #[test]
    fn unresolved_deps_reports_kept_packages_only_at_group_level() {
        let tmp = tempfile::tempdir().unwrap();
        let vdb_root = tmp.path().join("var/db/pkg");
        write_fake_package(
            &vdb_root,
            "app-misc",
            "topapp-1.0",
            "|| ( dev-libs/gone dev-libs/there ) dev-libs/missing !dev-libs/blocked",
            "",
        );
        write_fake_package(&vdb_root, "dev-libs", "there-1.0", "", "");
        write_fake_package(
            &vdb_root,
            "dev-libs",
            "orphan-1.0",
            "dev-libs/alsomissing",
            "",
        );

        let vdb = open_vdb(tmp.path());
        let installed: Vec<InstalledPackage> = vdb.packages().into_iter().collect();
        let world = vec![Dep::parse("app-misc/topapp").unwrap()];
        let cleanlist = compute_cleanlist(&installed, &world, &[], &[], false);
        let got: Vec<String> = unresolved_deps(&installed, &cleanlist, false)
            .into_iter()
            .map(|(d, p)| format!("{d} {p}"))
            .collect();
        assert_eq!(got, ["dev-libs/missing app-misc/topapp-1.0"]);
    }

    #[test]
    fn targeted_clean_is_not_blocked_by_unrelated_missing_deps() {
        let tmp = tempfile::tempdir().unwrap();
        let vdb_root = tmp.path().join("var/db/pkg");
        write_fake_package(&vdb_root, "app-misc", "other-1.0", "dev-libs/missing", "");
        write_fake_package(&vdb_root, "dev-libs", "orphan-1.0", "", "");

        let vdb = open_vdb(tmp.path());
        let installed: Vec<InstalledPackage> = vdb.packages().into_iter().collect();
        let target = vec![Dep::parse("dev-libs/orphan").unwrap()];
        let cleanlist = compute_cleanlist(&installed, &[], &[], &target, false);
        assert_eq!(cleanlist.len(), 1);
        assert!(blocking_unresolved(true, &installed, &cleanlist, false).is_empty());
        let got: Vec<String> = blocking_unresolved(false, &installed, &cleanlist, false)
            .into_iter()
            .map(|(d, p)| format!("{d} {p}"))
            .collect();
        assert_eq!(got, ["dev-libs/missing app-misc/other-1.0"]);
    }

    #[test]
    fn unresolved_idepend_is_checked_only_on_the_running_root() {
        let tmp = tempfile::tempdir().unwrap();
        let vdb_root = tmp.path().join("var/db/pkg");
        write_fake_package(&vdb_root, "app-misc", "topapp-1.0", "", "");
        std::fs::write(
            vdb_root.join("app-misc/topapp-1.0/IDEPEND"),
            "dev-util/gone",
        )
        .unwrap();

        let vdb = open_vdb(tmp.path());
        let installed: Vec<InstalledPackage> = vdb.packages().into_iter().collect();
        assert!(unresolved_deps(&installed, &[], false).is_empty());
        assert_eq!(unresolved_deps(&installed, &[], true).len(), 1);
    }

    #[test]
    fn explicit_target_args_protect_everything_else() {
        let tmp = tempfile::tempdir().unwrap();
        let vdb_root = tmp.path().join("var/db/pkg");

        write_fake_package(&vdb_root, "app-misc", "topapp-1.0", "dev-libs/needed", "");
        write_fake_package(&vdb_root, "dev-libs", "needed-1.0", "", "");
        write_fake_package(&vdb_root, "dev-libs", "orphan-1.0", "", "");

        let vdb = open_vdb(tmp.path());
        let installed: Vec<InstalledPackage> = vdb.packages().into_iter().collect();

        // Empty world: only `dev-libs/needed` is named as a target. Since
        // `topapp` and `orphan` aren't targets, both get folded into the
        // protected set unconditionally — and `topapp`'s own RDEPEND then
        // re-discovers `dev-libs/needed`, so nothing is actually removed.
        let target = vec![Dep::parse("dev-libs/needed").unwrap()];
        let cleanlist = compute_cleanlist(&installed, &[], &[], &target, false);
        assert!(cleanlist.is_empty());

        // Targeting the real orphan instead: nothing protects it.
        let target = vec![Dep::parse("dev-libs/orphan").unwrap()];
        let cleanlist = compute_cleanlist(&installed, &[], &[], &target, false);
        assert_eq!(cleanlist.len(), 1);
        assert_eq!(cleanlist[0].cpv().cpn.to_string(), "dev-libs/orphan");
    }

    #[test]
    fn targeted_seed_keeps_system_and_world_sets_but_not_world_atoms() {
        let tmp = tempfile::tempdir().unwrap();
        let root = camino::Utf8PathBuf::try_from(tmp.path().to_owned()).unwrap();
        let write = |rel: &str, body: &str| {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        };
        write("etc/portage/make.profile/packages", "*sys-apps/base\n");
        write("etc/portage/sets/extra", "dev-libs/inset\n");
        write("var/lib/portage/world", "app-misc/top\n");
        write("var/lib/portage/world_sets", "@extra\n");

        let mut seed: Vec<String> = targeted_seed(Some(&root), &root)
            .unwrap()
            .iter()
            .map(|d| d.to_string())
            .collect();
        seed.sort();
        assert_eq!(seed, ["dev-libs/inset", "sys-apps/base"]);
    }

    #[test]
    fn exclude_atoms_are_protected_like_world() {
        let tmp = tempfile::tempdir().unwrap();
        let vdb_root = tmp.path().join("var/db/pkg");
        write_fake_package(&vdb_root, "dev-libs", "orphan-1.0", "", "");
        let vdb = open_vdb(tmp.path());
        let installed: Vec<InstalledPackage> = vdb.packages().into_iter().collect();

        let exclude = vec![Dep::parse("dev-libs/orphan").unwrap()];
        let cleanlist = compute_cleanlist(&installed, &[], &exclude, &[], false);
        assert!(cleanlist.is_empty());
    }

    #[test]
    fn parse_atoms_strict_rejects_invalid_tokens() {
        // Guard the critical safety property: invalid CLI atoms must not be
        // silently dropped (which would empty target_atoms and open full-system
        // depclean). parse_atoms_strict is what `run` uses.
        use crate::emerge::parse_atoms_strict;
        let bad = vec!["not a valid atom!!!".to_string()];
        assert!(parse_atoms_strict(&bad).is_err());
        let mixed = vec!["dev-libs/foo".to_string(), ">=broken".to_string()];
        assert!(parse_atoms_strict(&mixed).is_err());
        let good = vec!["dev-libs/foo".to_string(), ">=sys-apps/bar-1".to_string()];
        let parsed = parse_atoms_strict(&good).unwrap();
        assert_eq!(parsed.len(), 2);
    }

    #[test]
    fn removal_order_puts_dependents_before_dependencies() {
        let tmp = tempfile::tempdir().unwrap();
        let vdb_root = tmp.path().join("var/db/pkg");
        // a -> b -> c (a depends on b, b depends on c), all orphaned.
        write_fake_package(&vdb_root, "dev-libs", "a-1.0", "dev-libs/b", "");
        write_fake_package(&vdb_root, "dev-libs", "b-1.0", "dev-libs/c", "");
        write_fake_package(&vdb_root, "dev-libs", "c-1.0", "", "");

        let vdb = open_vdb(tmp.path());
        let installed: Vec<InstalledPackage> = vdb.packages().into_iter().collect();
        let ordered = removal_order(&installed, false);
        let names: Vec<String> = ordered.iter().map(|p| p.cpv().cpn.to_string()).collect();

        let pos_a = names.iter().position(|n| n == "dev-libs/a").unwrap();
        let pos_b = names.iter().position(|n| n == "dev-libs/b").unwrap();
        let pos_c = names.iter().position(|n| n == "dev-libs/c").unwrap();
        assert!(pos_a < pos_b, "a (dependent) should be removed before b");
        assert!(pos_b < pos_c, "b (dependent) should be removed before c");
    }
}
