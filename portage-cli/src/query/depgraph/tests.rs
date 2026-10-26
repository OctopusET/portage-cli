//! Fixture solves through [`super::depgraph`]. The live `capture-plan.sh`
//! oracle only snapshots `--emptytree` on a real tree and never sets
//! `autounmask_widen`.

use std::collections::HashSet;

use camino::Utf8PathBuf;
use gentoo_core::Arch;
use portage_atom::interner::Interned;
use portage_atom::{Cpn, Cpv, Dep, Version};
use portage_atom_pubgrub::{MergeRoot, PortagePackage, UseFlagRequirement};
use portage_repo::RepoSet;

use super::targets::TargetAtom;
use super::{AutounmaskPersist, DepgraphOpts, apply_order_filters, depgraph};
use crate::cli::DepgraphFormat;

struct Pkg {
    cpv: &'static str,
    keywords: &'static str,
    depend: &'static str,
    rdepend: &'static str,
    iuse: &'static str,
    slot: &'static str,
    required_use: &'static str,
}

struct Fix {
    _dir: tempfile::TempDir,
    set: RepoSet,
    root: Utf8PathBuf,
    roots: portage_resolve::Roots,
}

struct Solve<'a> {
    atoms: &'a [TargetAtom],
    empty: bool,
    noreplace: bool,
    widen: bool,
    onlydeps: bool,
    update: bool,
    deep: bool,
    newuse: bool,
    changed_deps: bool,
    changed_slot: bool,
    complete_graph: bool,
    exclude: &'a [String],
    resume: HashSet<(MergeRoot, String)>,
    binpkg_preview: Option<&'a crate::binpkg::BinpkgPreview>,
    verbose: u8,
}

impl<'a> Solve<'a> {
    fn new(atoms: &'a [TargetAtom]) -> Self {
        Self {
            atoms,
            empty: false,
            noreplace: false,
            widen: false,
            onlydeps: false,
            update: false,
            deep: false,
            newuse: false,
            changed_deps: false,
            changed_slot: false,
            complete_graph: false,
            exclude: &[],
            resume: HashSet::new(),
            binpkg_preview: None,
            verbose: 0,
        }
    }
}

fn cache_text(pkg: &Pkg) -> String {
    let mut text = format!(
        "EAPI=8\nDESCRIPTION=t\nSLOT={}\nKEYWORDS={}\nDEPEND={}\nRDEPEND={}\n",
        pkg.slot, pkg.keywords, pkg.depend, pkg.rdepend
    );
    if !pkg.iuse.is_empty() {
        text.push_str(&format!("IUSE={}\n", pkg.iuse));
    }
    if !pkg.required_use.is_empty() {
        text.push_str(&format!("REQUIRED_USE={}\n", pkg.required_use));
    }
    text
}

fn plant_vdb(root: &Utf8PathBuf, cpv: &str) {
    plant_vdb_rdepend(root, cpv, None);
}

fn plant_installed(root: &Utf8PathBuf, cpv: &str, slot: &str, use_flags: &str, iuse: &str) {
    let cpv = Cpv::parse(cpv).unwrap();
    let dir = root
        .join("var/db/pkg")
        .join(cpv.cpn.category.as_str())
        .join(format!("{}-{}", cpv.cpn.package, cpv.version));
    std::fs::create_dir_all(dir.as_std_path()).unwrap();
    std::fs::write(dir.join("SLOT").as_std_path(), format!("{slot}\n")).unwrap();
    std::fs::write(dir.join("EAPI").as_std_path(), "8\n").unwrap();
    if !use_flags.is_empty() {
        std::fs::write(dir.join("USE").as_std_path(), format!("{use_flags}\n")).unwrap();
    }
    if !iuse.is_empty() {
        std::fs::write(dir.join("IUSE").as_std_path(), format!("{iuse}\n")).unwrap();
    }
}

fn plant_vdb_rdepend(root: &Utf8PathBuf, cpv: &str, rdepend: Option<&str>) {
    let cpv = Cpv::parse(cpv).unwrap();
    let dir = root
        .join("var/db/pkg")
        .join(cpv.cpn.category.as_str())
        .join(format!("{}-{}", cpv.cpn.package, cpv.version));
    std::fs::create_dir_all(dir.as_std_path()).unwrap();
    std::fs::write(dir.join("SLOT").as_std_path(), "0\n").unwrap();
    std::fs::write(dir.join("EAPI").as_std_path(), "8\n").unwrap();
    if let Some(rdepend) = rdepend {
        std::fs::write(dir.join("RDEPEND").as_std_path(), format!("{rdepend}\n")).unwrap();
    }
}

fn fixture(pkgs: &[Pkg], installed: &[&str]) -> Fix {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    let root = dir.path().join("root");
    std::fs::create_dir_all(repo.join("metadata")).unwrap();
    std::fs::create_dir_all(repo.join("profiles/base")).unwrap();
    std::fs::write(repo.join("metadata/layout.conf"), "masters =\n").unwrap();
    std::fs::write(repo.join("profiles/repo_name"), "fixture\n").unwrap();
    std::fs::write(
        repo.join("profiles/base/make.defaults"),
        "ARCH=\"amd64\"\nACCEPT_KEYWORDS=\"amd64\"\n",
    )
    .unwrap();
    for pkg in pkgs {
        let cpv = Cpv::parse(pkg.cpv).unwrap();
        let cache = repo
            .join("metadata/md5-cache")
            .join(cpv.cpn.category.as_str());
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(
            cache.join(format!("{}-{}", cpv.cpn.package, cpv.version)),
            cache_text(pkg),
        )
        .unwrap();
    }

    let root_path = Utf8PathBuf::try_from(root.clone()).unwrap();
    std::fs::create_dir_all(root.join("etc/portage")).unwrap();
    std::fs::write(
        root.join("etc/portage/make.conf"),
        "ARCH=\"amd64\"\nACCEPT_KEYWORDS=\"amd64\"\n",
    )
    .unwrap();
    std::os::unix::fs::symlink(
        repo.join("profiles/base"),
        root.join("etc/portage/make.profile"),
    )
    .unwrap();
    for cpv in installed {
        plant_vdb(&root_path, cpv);
    }

    let opened = portage_repo::Repository::builder()
        .in_memory_cache()
        .open(&repo)
        .unwrap();
    let roots = portage_resolve::Roots::default()
        .with_config(Some(root_path.clone()))
        .with_base(Some(root_path.clone()))
        .with_target(Some(root_path.clone()))
        .with_broot(Some(root_path.clone()));
    Fix {
        _dir: dir,
        set: RepoSet::single(opened),
        root: root_path,
        roots,
    }
}

fn solve(fix: &Fix, opts: Solve<'_>) -> super::DepgraphOutcome {
    try_solve(fix, opts).unwrap()
}

fn try_solve(fix: &Fix, opts: Solve<'_>) -> anyhow::Result<super::DepgraphOutcome> {
    let arch = Arch::intern("amd64");
    let world: &[Dep] = &[];
    let extra: &[(
        portage_atom::ConfigAtom,
        Vec<portage_atom_pubgrub::UseOverride>,
    )] = &[];
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(depgraph(DepgraphOpts {
        set: fix.set.clone(),
        atoms: opts.atoms,
        world_additions: world,
        arch: &arch,
        format: DepgraphFormat::Pretty,
        verbose: opts.verbose,
        empty: opts.empty,
        autounmask_write: false,
        autounmask_persist: AutounmaskPersist::Never,
        ask: false,
        autosolve_use: false,
        autounmask_widen: opts.widen,
        roots: &fix.roots,
        host_merge_root: &fix.root,
        onlydeps: opts.onlydeps,
        with_bdeps: false,
        root_deps_rdeps: false,
        deep: opts.deep,
        update: opts.update,
        newuse: opts.newuse,
        changed_use: false,
        noreplace: opts.noreplace,
        changed_deps: opts.changed_deps,
        changed_slot: opts.changed_slot,
        nodeps: false,
        extra_use_override: None,
        extra_package_use: extra,
        pending_config: None,
        binpkg_preview: opts.binpkg_preview,
        exclude: opts.exclude,
        resume_completed: opts.resume,
        complete_graph: opts.complete_graph,
        quiet: true,
        display: Default::default(),
    }))
}

fn names(outcome: &super::DepgraphOutcome) -> Vec<String> {
    outcome.plan.iter().map(|p| p.cpv.to_string()).collect()
}

fn pair() -> Vec<Pkg> {
    vec![
        Pkg {
            cpv: "app-misc/foo-1",
            keywords: "amd64",
            depend: "app-misc/bar",
            rdepend: "",
            iuse: "",
            slot: "0",
            required_use: "",
        },
        Pkg {
            cpv: "app-misc/bar-1",
            keywords: "amd64",
            depend: "",
            rdepend: "",
            iuse: "",
            slot: "0",
            required_use: "",
        },
    ]
}

/// `bar-1` is installed. `consumer-1` is installed with `RDEPEND="<app-misc/bar-2"`.
/// `consumer-2` accepts `bar-2`, so a repair round can upgrade the consumer
/// instead of leaving the pin broken.
fn pinned_consumer() -> Vec<Pkg> {
    vec![
        Pkg {
            cpv: "app-misc/bar-1",
            keywords: "amd64",
            depend: "",
            rdepend: "",
            iuse: "",
            slot: "0",
            required_use: "",
        },
        Pkg {
            cpv: "app-misc/bar-2",
            keywords: "amd64",
            depend: "",
            rdepend: "",
            iuse: "",
            slot: "0",
            required_use: "",
        },
        Pkg {
            cpv: "app-misc/consumer-1",
            keywords: "amd64",
            depend: "<app-misc/bar-2",
            rdepend: "",
            iuse: "",
            slot: "0",
            required_use: "",
        },
        Pkg {
            cpv: "app-misc/consumer-2",
            keywords: "amd64",
            depend: ">=app-misc/bar-2",
            rdepend: "",
            iuse: "",
            slot: "0",
            required_use: "",
        },
    ]
}

fn masked_dep() -> Vec<Pkg> {
    vec![
        Pkg {
            cpv: "app-misc/qux-1",
            keywords: "amd64",
            depend: "app-misc/masked",
            rdepend: "",
            iuse: "",
            slot: "0",
            required_use: "",
        },
        Pkg {
            cpv: "app-misc/masked-1",
            keywords: "~amd64",
            depend: "",
            rdepend: "",
            iuse: "",
            slot: "0",
            required_use: "",
        },
    ]
}

#[test]
fn emptytree_orders_the_dependency_before_its_parent() {
    let fix = fixture(&pair(), &[]);
    let atoms = [TargetAtom::explicit("app-misc/foo")];
    let mut opts = Solve::new(&atoms);
    opts.empty = true;
    let outcome = solve(&fix, opts);
    let planned = names(&outcome);
    let bar = planned.iter().position(|n| n == "app-misc/bar-1").unwrap();
    let foo = planned.iter().position(|n| n == "app-misc/foo-1").unwrap();
    assert!(bar < foo);
    assert!(outcome.build_blockers[foo].contains(&bar));
    assert_eq!(outcome.exit_code, 0);
}

#[test]
fn emptytree_still_plans_a_package_that_is_already_installed() {
    let fix = fixture(&pair(), &["app-misc/bar-1"]);
    let atoms = [TargetAtom::explicit("app-misc/foo")];
    let mut opts = Solve::new(&atoms);
    opts.empty = true;
    let planned = names(&solve(&fix, opts));
    assert!(planned.iter().any(|n| n == "app-misc/bar-1"));
    assert!(planned.iter().any(|n| n == "app-misc/foo-1"));
}

#[test]
fn emptytree_rebuilds_an_installed_dependency_under_an_offset_root() {
    let mut pkgs = pair();
    pkgs[0].rdepend = "app-misc/lib:*";
    for (cpv, slot) in [
        ("app-misc/bar-2", "0"),
        ("app-misc/lib-1", "1"),
        ("app-misc/lib-2", "2"),
    ] {
        pkgs.push(Pkg {
            cpv,
            keywords: "amd64",
            depend: "",
            rdepend: "",
            iuse: "",
            slot,
            required_use: "",
        });
    }
    let mut fix = fixture(&pkgs, &["app-misc/foo-1", "app-misc/bar-1"]);
    plant_installed(&fix.root, "app-misc/lib-1", "1", "", "");
    let broot = Utf8PathBuf::try_from(fix._dir.path().join("broot")).unwrap();
    std::fs::create_dir_all(broot.as_std_path()).unwrap();
    fix.roots = fix.roots.clone().with_broot(Some(broot));
    let atoms = [TargetAtom::explicit("app-misc/foo")];
    let mut opts = Solve::new(&atoms);
    opts.empty = true;
    let outcome = solve(&fix, opts);
    let mut target: Vec<String> = outcome
        .plan
        .iter()
        .filter(|p| p.merge_root == MergeRoot::Target)
        .map(|p| p.cpv.to_string())
        .collect();
    target.sort();
    assert_eq!(
        target,
        ["app-misc/bar-2", "app-misc/foo-1", "app-misc/lib-2"]
    );
}

#[test]
fn exclude_removes_the_dependency_after_the_solve() {
    let fix = fixture(&pair(), &[]);
    let atoms = [TargetAtom::explicit("app-misc/foo")];
    let excluded = ["app-misc/bar".to_string()];
    let mut opts = Solve::new(&atoms);
    opts.empty = true;
    opts.exclude = &excluded;
    let planned = names(&solve(&fix, opts));
    assert_eq!(planned, ["app-misc/foo-1"]);
}

#[test]
fn resume_omits_a_package_already_completed() {
    let fix = fixture(&pair(), &[]);
    let atoms = [TargetAtom::explicit("app-misc/foo")];
    let mut opts = Solve::new(&atoms);
    opts.empty = true;
    opts.resume
        .insert((MergeRoot::Target, "app-misc/bar-1".to_string()));
    let planned = names(&solve(&fix, opts));
    assert_eq!(planned, ["app-misc/foo-1"]);
}

#[test]
fn noreplace_leaves_an_installed_target_out_of_the_plan() {
    let fix = fixture(&pair(), &["app-misc/foo-1", "app-misc/bar-1"]);
    let atoms = [TargetAtom::explicit("app-misc/foo")];
    let mut opts = Solve::new(&atoms);
    opts.noreplace = true;
    assert!(names(&solve(&fix, opts)).is_empty());
}

#[test]
fn an_installed_target_is_reinstalled_when_the_resolve_is_not_selective() {
    let fix = fixture(&pair(), &["app-misc/foo-1", "app-misc/bar-1"]);
    let atoms = [TargetAtom::explicit("app-misc/foo")];
    let outcome = solve(&fix, Solve::new(&atoms));
    assert_eq!(names(&outcome), ["app-misc/foo-1"]);
    assert!(outcome.plan[0].reinstall);
}

#[test]
fn onlydeps_plans_the_dependency_and_omits_the_target() {
    let fix = fixture(&pair(), &[]);
    let atoms = [TargetAtom::explicit("app-misc/foo")];
    let mut opts = Solve::new(&atoms);
    opts.empty = true;
    opts.onlydeps = true;
    assert_eq!(names(&solve(&fix, opts)), ["app-misc/bar-1"]);
}

/// `flagged-1` has `IUSE=foo` (off), `flagged-2` has no `foo`, `defaulted-1` has `IUSE=+foo`
fn flagged() -> Vec<Pkg> {
    let pkg = |cpv, iuse| Pkg {
        cpv,
        keywords: "amd64",
        depend: "",
        iuse,
        required_use: "",
        rdepend: "",
        slot: "0",
    };
    vec![
        pkg("app-misc/flagged-1", "foo"),
        pkg("app-misc/flagged-2", ""),
        pkg("app-misc/defaulted-1", "+foo"),
    ]
}

#[test]
fn a_target_use_requirement_picks_a_version_with_the_flag_and_asks_for_it() {
    let fix = fixture(&flagged(), &[]);
    let atoms = [TargetAtom::explicit("app-misc/flagged[foo]")];
    let outcome = solve(&fix, Solve::new(&atoms));
    assert_eq!(names(&outcome), ["app-misc/flagged-1"]);
    assert_eq!(
        outcome.exit_code, 1,
        "foo is off, so a USE change is required"
    );
}

#[test]
fn a_target_use_requirement_already_met_needs_no_change() {
    let fix = fixture(&flagged(), &[]);
    let atoms = [TargetAtom::explicit("app-misc/defaulted[foo]")];
    let outcome = solve(&fix, Solve::new(&atoms));
    assert_eq!(names(&outcome), ["app-misc/defaulted-1"]);
    assert_eq!(outcome.exit_code, 0);
}

#[test]
fn a_target_use_requirement_no_version_has_is_unsatisfiable() {
    let fix = fixture(&flagged(), &[]);
    let atoms = [TargetAtom::explicit("app-misc/flagged[nosuchflag]")];
    let Err(err) = try_solve(&fix, Solve::new(&atoms)) else {
        panic!("a flag no version has must not resolve");
    };
    let message = err
        .downcast_ref::<crate::error::UnsatisfiableTarget>()
        .map(|e| e.0.clone())
        .unwrap_or_default();
    assert!(message.contains("Missing IUSE: nosuchflag"), "{err:#}");
}

#[test]
fn a_keyword_dropped_dependency_is_not_installable() {
    let fix = fixture(&masked_dep(), &[]);
    let atoms = [TargetAtom::explicit("app-misc/qux")];
    let outcome = solve(&fix, Solve::new(&atoms));
    let planned = names(&outcome);
    assert!(
        planned.iter().any(|n| n == "app-misc/qux-1"),
        "plan was {planned:?}"
    );
    assert!(
        !planned.iter().any(|n| n == "app-misc/masked-1"),
        "plan was {planned:?}"
    );
    assert_eq!(outcome.exit_code, 1);
}

#[test]
fn complete_graph_pulls_the_installed_package_whose_pin_the_update_breaks() {
    let fix = fixture(&pinned_consumer(), &[]);
    plant_vdb(&fix.root, "app-misc/bar-1");
    plant_vdb_rdepend(&fix.root, "app-misc/consumer-1", Some("<app-misc/bar-2"));
    let atoms = [TargetAtom::explicit("app-misc/bar")];

    let mut update = Solve::new(&atoms);
    update.update = true;
    update.deep = true;
    assert_eq!(names(&solve(&fix, update)), ["app-misc/bar-2"]);

    let mut repair = Solve::new(&atoms);
    repair.update = true;
    repair.deep = true;
    repair.complete_graph = true;
    let planned = names(&solve(&fix, repair));
    let bar = planned
        .iter()
        .position(|n| n == "app-misc/bar-2")
        .unwrap_or_else(|| panic!("plan was {planned:?}"));
    let consumer = planned
        .iter()
        .position(|n| n == "app-misc/consumer-2")
        .unwrap_or_else(|| panic!("plan was {planned:?}"));
    assert!(bar < consumer, "plan was {planned:?}");
}

#[test]
fn widen_schedules_a_testing_keyword_dependency_the_strict_solve_drops() {
    let fix = fixture(&masked_dep(), &[]);
    let atoms = [TargetAtom::explicit("app-misc/qux")];
    let strict = names(&solve(&fix, Solve::new(&atoms)));
    assert!(
        !strict.iter().any(|n| n == "app-misc/masked-1"),
        "strict plan was {strict:?}"
    );

    let mut opts = Solve::new(&atoms);
    opts.widen = true;
    let widened = names(&solve(&fix, opts));
    assert!(
        widened.iter().any(|n| n == "app-misc/masked-1"),
        "widened plan was {widened:?}"
    );
    let masked = widened
        .iter()
        .position(|n| n == "app-misc/masked-1")
        .unwrap();
    let qux = widened.iter().position(|n| n == "app-misc/qux-1").unwrap();
    assert!(masked < qux);
}

#[test]
fn a_missing_explicit_atom_is_fatal() {
    let fix = fixture(&pair(), &[]);
    let atoms = [TargetAtom::explicit("app-misc/missing")];
    let arch = Arch::intern("amd64");
    let world: &[Dep] = &[];
    let extra: &[(
        portage_atom::ConfigAtom,
        Vec<portage_atom_pubgrub::UseOverride>,
    )] = &[];
    let result = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(depgraph(DepgraphOpts {
            set: fix.set.clone(),
            atoms: &atoms,
            world_additions: world,
            arch: &arch,
            format: DepgraphFormat::Pretty,
            verbose: 0,
            empty: false,
            autounmask_write: false,
            autounmask_persist: AutounmaskPersist::Never,
            ask: false,
            autosolve_use: false,
            autounmask_widen: false,
            roots: &fix.roots,
            host_merge_root: &fix.root,
            onlydeps: false,
            with_bdeps: false,
            root_deps_rdeps: false,
            deep: false,
            update: false,
            newuse: false,
            changed_use: false,
            noreplace: false,
            changed_deps: false,
            changed_slot: false,
            nodeps: false,
            extra_use_override: None,
            extra_package_use: extra,
            pending_config: None,
            binpkg_preview: None,
            exclude: &[],
            resume_completed: HashSet::new(),
            complete_graph: false,
            quiet: true,
            display: Default::default(),
        }));
    let Err(err) = result else {
        panic!("a missing explicit atom must abort the resolve");
    };
    let msg = format!("{err:#}");
    assert!(msg.contains("app-misc/missing"), "{msg}");
}

#[test]
fn a_missing_world_member_does_not_abort() {
    let fix = fixture(&pair(), &[]);
    let atoms = [TargetAtom {
        atom: "app-misc/missing".to_string(),
        origin: super::targets::TargetOrigin::Set("world".to_string()),
    }];
    let outcome = solve(&fix, Solve::new(&atoms));
    assert!(outcome.plan.is_empty());
    assert_eq!(outcome.exit_code, 0);
}

#[test]
fn apply_order_filters_reappends_a_reinstall_missing_from_the_order() {
    let cpn = Cpn::parse("app-misc/re").unwrap();
    let pkg = PortagePackage::slotted(cpn, Interned::intern("0"));
    let ver = Version::parse("1").unwrap();
    let req = UseFlagRequirement {
        package: pkg.clone(),
        version: ver.clone(),
        upgrade_to: None,
        required_enabled: Vec::new(),
        required_disabled: Vec::new(),
        required_by: Vec::new(),
    };
    let (order, excluded, resumed) =
        apply_order_filters(Vec::new(), &[&req], &[], &HashSet::new(), false);
    assert_eq!(order, vec![(pkg, ver)]);
    assert_eq!((excluded, resumed), (0, 0));
}

#[test]
fn apply_order_filters_counts_exclude_and_resume_and_drops_host_rows() {
    let target =
        PortagePackage::slotted(Cpn::parse("app-misc/foo").unwrap(), Interned::intern("0"));
    let host = PortagePackage::slotted_at(
        Cpn::parse("app-misc/tool").unwrap(),
        Interned::intern("0"),
        MergeRoot::Host,
    );
    let ver = Version::parse("1").unwrap();
    let order = vec![(target.clone(), ver.clone()), (host, ver.clone())];
    let exclude = [Dep::parse("app-misc/foo").unwrap()];
    let mut resume = HashSet::new();
    resume.insert((MergeRoot::Host, "app-misc/tool-1".to_string()));
    let (kept, excluded, resumed) = apply_order_filters(order, &[], &exclude, &resume, true);
    assert!(kept.is_empty());
    assert_eq!((excluded, resumed), (1, 1));
}

fn masked_names(outcome: &super::DepgraphOutcome) -> Vec<String> {
    outcome
        .masked_installed
        .iter()
        .map(|(cpv, _)| cpv.to_string())
        .collect()
}

fn mask(fix: &Fix, atom: &str) {
    let conf = fix.root.join("etc/portage/package.mask");
    std::fs::write(conf.as_std_path(), format!("{atom}\n")).unwrap();
}

fn kept_bar(bar_keywords: &'static str) -> Vec<Pkg> {
    vec![
        Pkg {
            cpv: "app-misc/consumer-1",
            keywords: "amd64",
            depend: "app-misc/bar",
            rdepend: "",
            iuse: "",
            slot: "0",
            required_use: "",
        },
        Pkg {
            cpv: "app-misc/bar-1",
            keywords: bar_keywords,
            depend: "",
            rdepend: "",
            iuse: "",
            slot: "0",
            required_use: "",
        },
    ]
}

#[test]
fn a_package_masked_installed_package_is_reported_when_nothing_replaces_it() {
    let fix = fixture(&kept_bar("amd64"), &[]);
    plant_installed(&fix.root, "app-misc/bar-1", "0", "", "");
    mask(&fix, "=app-misc/bar-1");
    let atoms = [TargetAtom::explicit("app-misc/consumer")];
    let outcome = solve(&fix, Solve::new(&atoms));
    assert_eq!(names(&outcome), ["app-misc/consumer-1"]);
    assert_eq!(masked_names(&outcome), ["app-misc/bar-1"]);
    // The kept package is still a dropped masked dep, so autounmask fails
    // the plan. The warning itself is not what sets the exit code.
    assert_eq!(outcome.exit_code, 1);
}

#[test]
fn an_installed_package_is_not_masked_by_its_keywords() {
    let fix = fixture(&kept_bar("~amd64"), &[]);
    plant_installed(&fix.root, "app-misc/bar-1", "0", "", "");
    let atoms = [TargetAtom::explicit("app-misc/consumer")];
    let outcome = solve(&fix, Solve::new(&atoms));
    assert_eq!(names(&outcome), ["app-misc/consumer-1"]);
    assert!(
        masked_names(&outcome).is_empty(),
        "masked was {:?}",
        masked_names(&outcome)
    );
    assert_eq!(outcome.exit_code, 0);
}

#[test]
fn a_keyword_masked_installed_target_is_not_reinstalled() {
    let fix = fixture(&kept_bar("~amd64"), &[]);
    plant_installed(&fix.root, "app-misc/bar-1", "0", "", "");
    let atoms = [TargetAtom::explicit("app-misc/bar")];
    let Err(err) = try_solve(&fix, Solve::new(&atoms)) else {
        panic!("a keyword-masked target must not be planned");
    };
    assert!(format!("{err:#}").contains("app-misc/bar"), "{err:#}");
}

#[test]
fn newuse_does_not_rebuild_a_keyword_masked_installed_package() {
    let mut pkgs = kept_bar("~amd64");
    pkgs[1].iuse = "+x";
    let fix = fixture(&pkgs, &[]);
    plant_installed(&fix.root, "app-misc/bar-1", "0", "", "x");
    let atoms = [TargetAtom::explicit("app-misc/bar")];
    let mut opts = Solve::new(&atoms);
    opts.newuse = true;
    let outcome = solve(&fix, opts);
    assert!(names(&outcome).is_empty(), "planned {:?}", names(&outcome));
    assert!(
        masked_names(&outcome).is_empty(),
        "masked was {:?}",
        masked_names(&outcome)
    );
}

#[test]
fn a_keyword_masked_package_installed_only_on_the_target_is_not_built_for_broot() {
    let mut pkgs = kept_bar("~amd64");
    // `cache_text` has no BDEPEND field; this appends the line.
    pkgs[0].depend = "app-misc/bar\nBDEPEND=app-misc/bar";
    let mut fix = fixture(&pkgs, &[]);
    plant_installed(&fix.root, "app-misc/bar-1", "0", "", "");
    let broot = Utf8PathBuf::try_from(fix._dir.path().join("broot")).unwrap();
    std::fs::create_dir_all(broot.as_std_path()).unwrap();
    fix.roots = fix.roots.clone().with_broot(Some(broot));
    let atoms = [TargetAtom::explicit("app-misc/consumer")];
    let outcome = solve(&fix, Solve::new(&atoms));
    assert_eq!(names(&outcome), ["app-misc/consumer-1"]);
    assert_eq!(outcome.exit_code, 1);
}

#[test]
fn a_masked_installed_package_is_not_reported_when_its_slot_is_upgraded() {
    let pkgs = vec![
        Pkg {
            cpv: "app-misc/other-1",
            keywords: "amd64",
            depend: ">=app-misc/bar-2",
            rdepend: "",
            iuse: "",
            slot: "0",
            required_use: "",
        },
        Pkg {
            cpv: "app-misc/bar-1",
            keywords: "amd64",
            depend: "",
            rdepend: "",
            iuse: "",
            slot: "0",
            required_use: "",
        },
        Pkg {
            cpv: "app-misc/bar-2",
            keywords: "amd64",
            depend: "",
            rdepend: "",
            iuse: "",
            slot: "0",
            required_use: "",
        },
    ];
    let fix = fixture(&pkgs, &[]);
    plant_installed(&fix.root, "app-misc/bar-1", "0", "", "");
    mask(&fix, "=app-misc/bar-1");
    // The first atom matches only the masked installed version and is dropped.
    let atoms = [
        TargetAtom::explicit("<app-misc/bar-2"),
        TargetAtom::explicit("app-misc/other"),
    ];
    let mut opts = Solve::new(&atoms);
    opts.noreplace = true;
    let outcome = solve(&fix, opts);
    assert_eq!(names(&outcome), ["app-misc/bar-2", "app-misc/other-1"]);
    assert!(
        masked_names(&outcome).is_empty(),
        "masked was {:?}",
        masked_names(&outcome)
    );
}

#[test]
fn a_masked_installed_package_in_another_slot_is_not_reported() {
    let pkgs = vec![
        Pkg {
            cpv: "app-misc/consumer-1",
            keywords: "amd64",
            depend: "app-misc/bar:0",
            rdepend: "",
            iuse: "",
            slot: "0",
            required_use: "",
        },
        Pkg {
            cpv: "app-misc/bar-1",
            keywords: "~amd64",
            depend: "",
            rdepend: "",
            iuse: "",
            slot: "1",
            required_use: "",
        },
    ];
    let fix = fixture(&pkgs, &[]);
    plant_installed(&fix.root, "app-misc/bar-1", "0", "", "");
    let atoms = [TargetAtom::explicit("app-misc/consumer")];
    let outcome = solve(&fix, Solve::new(&atoms));
    assert_eq!(names(&outcome), ["app-misc/consumer-1"]);
    assert!(
        masked_names(&outcome).is_empty(),
        "masked was {:?}",
        masked_names(&outcome)
    );
    assert_eq!(outcome.exit_code, 0);
}

#[test]
fn deep_without_update_keeps_the_installed_slot_of_an_any_slot_dep() {
    let pkgs = vec![
        Pkg {
            cpv: "app-misc/foo-1",
            keywords: "amd64",
            depend: "",
            rdepend: "app-misc/lib:*",
            iuse: "",
            slot: "0",
            required_use: "",
        },
        Pkg {
            cpv: "app-misc/lib-1",
            keywords: "amd64",
            depend: "",
            rdepend: "",
            iuse: "",
            slot: "1",
            required_use: "",
        },
        Pkg {
            cpv: "app-misc/lib-2",
            keywords: "amd64",
            depend: "",
            rdepend: "",
            iuse: "",
            slot: "2",
            required_use: "",
        },
    ];
    let fix = fixture(&pkgs, &[]);
    plant_installed(&fix.root, "app-misc/lib-1", "1", "", "");
    let atoms = [TargetAtom::explicit("app-misc/foo")];
    let mut deep = Solve::new(&atoms);
    deep.deep = true;
    assert_eq!(names(&solve(&fix, deep)), ["app-misc/foo-1"]);

    let mut update = Solve::new(&atoms);
    update.deep = true;
    update.update = true;
    assert_eq!(
        names(&solve(&fix, update)),
        ["app-misc/lib-2", "app-misc/foo-1"]
    );
}

#[test]
fn deep_descends_into_a_kept_package_through_its_vdb_deps_once_its_ebuild_is_gone() {
    let mut pkgs = pair();
    pkgs[0].depend = "";
    pkgs[0].rdepend = "app-misc/bar";
    pkgs.push(Pkg {
        cpv: "app-misc/baz-1",
        keywords: "amd64",
        depend: "",
        rdepend: "",
        iuse: "",
        slot: "0",
        required_use: "",
    });
    let fix = fixture(&pkgs, &[]);
    plant_vdb_rdepend(&fix.root, "app-misc/bar-0", Some("app-misc/baz"));
    let atoms = [TargetAtom::explicit("app-misc/foo")];
    assert_eq!(names(&solve(&fix, Solve::new(&atoms))), ["app-misc/foo-1"]);
    let mut deep = Solve::new(&atoms);
    deep.deep = true;
    assert_eq!(
        names(&solve(&fix, deep)),
        ["app-misc/baz-1", "app-misc/foo-1"]
    );
}

#[test]
fn deep_descends_into_a_kept_package_with_the_use_it_was_built_with() {
    let pkgs = vec![
        Pkg {
            cpv: "app-misc/app-1",
            keywords: "amd64",
            depend: "",
            rdepend: "net-misc/curl",
            iuse: "",
            slot: "0",
            required_use: "",
        },
        Pkg {
            cpv: "net-misc/curl-1",
            keywords: "amd64",
            depend: "",
            rdepend: "http2? ( net-libs/nghttp2 )",
            iuse: "+http2",
            slot: "0",
            required_use: "",
        },
        Pkg {
            cpv: "net-libs/nghttp2-1",
            keywords: "amd64",
            depend: "",
            rdepend: "",
            iuse: "",
            slot: "0",
            required_use: "",
        },
    ];
    let fix = fixture(&pkgs, &[]);
    plant_installed(&fix.root, "net-misc/curl-1", "0", "", "http2");
    let atoms = [TargetAtom::explicit("app-misc/app")];
    let mut deep = Solve::new(&atoms);
    deep.deep = true;
    assert_eq!(names(&solve(&fix, deep)), ["app-misc/app-1"]);
}

fn pkg(cpv: &'static str, slot: &'static str, rdepend: &'static str) -> Pkg {
    Pkg {
        cpv,
        keywords: "amd64",
        depend: "",
        rdepend,
        iuse: "",
        slot,
        required_use: "",
    }
}

fn plant_vdb_slot(root: &Utf8PathBuf, cpv: &str, slot: &str, rdepend: &str) {
    plant_installed(root, cpv, slot, "", "");
    let cpv = Cpv::parse(cpv).unwrap();
    let dir = root
        .join("var/db/pkg")
        .join(cpv.cpn.category.as_str())
        .join(format!("{}-{}", cpv.cpn.package, cpv.version));
    std::fs::write(dir.join("RDEPEND").as_std_path(), format!("{rdepend}\n")).unwrap();
}

/// `lib` moves from subslot 1 to 2 under `-u lib`; installed `consumer-1` bound `lib:0/1=`.
fn slot_op_update(consumers: &[Pkg], exclude: &[String]) -> Vec<String> {
    let mut pkgs = vec![
        pkg("dev-libs/lib-1", "0/1", ""),
        pkg("dev-libs/lib-2", "0/2", ""),
    ];
    pkgs.extend(consumers.iter().map(|c| pkg(c.cpv, c.slot, c.rdepend)));
    let fix = fixture(&pkgs, &[]);
    plant_vdb_slot(&fix.root, "dev-libs/lib-1", "0/1", "");
    plant_vdb_slot(&fix.root, "app-misc/consumer-1", "0", "dev-libs/lib:0/1=");
    let atoms = [TargetAtom::explicit("dev-libs/lib")];
    let mut opts = Solve::new(&atoms);
    opts.update = true;
    opts.exclude = exclude;
    names(&solve(&fix, opts))
}

#[test]
fn slot_op_rebuild_follows_its_trigger() {
    let consumer = [pkg("app-misc/consumer-1", "0", "dev-libs/lib:=")];
    assert_eq!(
        slot_op_update(&consumer, &[]),
        ["dev-libs/lib-2", "app-misc/consumer-1"]
    );
}

#[test]
fn slot_op_rebuild_solves_the_consumer_upgrade() {
    let plan = slot_op_update(&consumer_upgrade_with_new_dep(), &[]);
    let at = |name: &str| plan.iter().position(|p| p == name).unwrap();
    assert_eq!(plan.len(), 3);
    assert!(at("dev-libs/lib-2") < at("app-misc/consumer-2"));
    assert!(at("dev-libs/newdep-1") < at("app-misc/consumer-2"));
}

/// `consumer-2` needs `dev-libs/newdep`, which only a solved (not spliced) upgrade pulls in.
fn consumer_upgrade_with_new_dep() -> [Pkg; 3] {
    [
        pkg("app-misc/consumer-1", "0", "dev-libs/lib:="),
        pkg(
            "app-misc/consumer-2",
            "0",
            ">=dev-libs/lib-2:= dev-libs/newdep",
        ),
        pkg("dev-libs/newdep-1", "0", ""),
    ]
}

#[test]
fn slot_op_rebuild_honours_exclude() {
    let exclude = ["app-misc/consumer".to_string()];
    assert_eq!(
        slot_op_update(&consumer_upgrade_with_new_dep(), &exclude),
        ["dev-libs/lib-2"]
    );
}

#[test]
fn slot_op_rebuild_skips_a_consumer_with_no_visible_version() {
    assert_eq!(slot_op_update(&[], &[]), ["dev-libs/lib-2"]);
}

/// Installed `top-1` keeps `consumer-1` (bound `lib:0/1=`) in the `-uD top` graph.
fn deep_update_of_top(
    extra: &[Pkg],
    top_rdepend: &'static str,
    plant: impl Fn(&Utf8PathBuf),
    deep: bool,
) -> Vec<String> {
    let mut pkgs = vec![
        pkg("app-misc/top-1", "0", top_rdepend),
        pkg("app-misc/consumer-1", "0", "dev-lang/lua:="),
    ];
    pkgs.extend(extra.iter().map(|p| pkg(p.cpv, p.slot, p.rdepend)));
    let fix = fixture(&pkgs, &[]);
    plant_vdb_slot(&fix.root, "app-misc/top-1", "0", top_rdepend);
    plant(&fix.root);
    let atoms = [TargetAtom::explicit("app-misc/top")];
    let mut opts = Solve::new(&atoms);
    opts.update = true;
    opts.deep = deep;
    names(&solve(&fix, opts))
}

#[test]
fn deep_update_rebuilds_a_consumer_of_an_installed_provider_with_another_subslot() {
    let lua = [pkg("dev-lang/lua-5.1.6", "5.1/6", "")];
    let plant = |root: &Utf8PathBuf| {
        plant_vdb_slot(root, "dev-lang/lua-5.1.6", "5.1/6", "");
        plant_vdb_slot(root, "app-misc/consumer-1", "0", "dev-lang/lua:5.1/5=");
    };
    assert_eq!(
        deep_update_of_top(&lua, "app-misc/consumer", plant, true),
        ["app-misc/consumer-1"]
    );
    assert!(deep_update_of_top(&lua, "app-misc/consumer", plant, false).is_empty());
}

#[test]
fn deep_update_rebuilds_a_consumer_for_a_new_slot() {
    let lua = [
        pkg("dev-lang/lua-5.1.5", "5.1", ""),
        pkg("dev-lang/lua-5.4.6", "5.4", ""),
    ];
    let plant = |root: &Utf8PathBuf| {
        plant_vdb_slot(root, "dev-lang/lua-5.1.5", "5.1", "");
        plant_vdb_slot(root, "app-misc/consumer-1", "0", "dev-lang/lua:5.1/5.1=");
    };
    for top_rdepend in ["app-misc/consumer dev-lang/lua:5.4", "app-misc/consumer"] {
        assert_eq!(
            deep_update_of_top(&lua, top_rdepend, plant, true),
            ["dev-lang/lua-5.4.6", "app-misc/consumer-1"],
            "{top_rdepend}"
        );
    }
}

#[test]
fn changed_deps_rebuilds_only_an_installed_package_whose_deps_changed() {
    let pkgs = vec![
        Pkg {
            cpv: "app-misc/bar-1",
            keywords: "amd64",
            depend: "",
            rdepend: "app-misc/baz",
            iuse: "",
            slot: "0",
            required_use: "",
        },
        Pkg {
            cpv: "app-misc/baz-1",
            keywords: "amd64",
            depend: "",
            rdepend: "",
            iuse: "",
            slot: "0",
            required_use: "",
        },
    ];
    let atoms = [TargetAtom::explicit("app-misc/bar")];
    for (recorded, planned) in [
        (None, vec!["app-misc/bar-1"]),
        (Some("app-misc/baz"), vec![]),
    ] {
        let fix = fixture(&pkgs, &["app-misc/baz-1"]);
        plant_vdb_rdepend(&fix.root, "app-misc/bar-1", recorded);
        let mut opts = Solve::new(&atoms);
        opts.changed_deps = true;
        assert_eq!(
            names(&solve(&fix, opts)),
            planned,
            "VDB RDEPEND {recorded:?}"
        );
    }
}

#[test]
fn a_glob_target_matches_only_versions_with_that_prefix() {
    let pkgs = [
        pkg("app-misc/foo-1.09", "0", ""),
        pkg("app-misc/foo-1.095", "0", ""),
    ];
    let fix = fixture(&pkgs, &[]);
    let atoms = [TargetAtom::explicit("=app-misc/foo-1.09*")];
    assert_eq!(
        names(&solve(&fix, Solve::new(&atoms))),
        ["app-misc/foo-1.09"]
    );
}

#[test]
fn changed_slot_rebuilds_the_installed_version_once() {
    for tree_slot in ["0/2", "1"] {
        let pkgs = [pkg("app-misc/bar-1", tree_slot, "")];
        let fix = fixture(&pkgs, &[]);
        plant_vdb_slot(&fix.root, "app-misc/bar-1", "0", "");
        let atoms = [TargetAtom::explicit("app-misc/bar")];
        let mut opts = Solve::new(&atoms);
        opts.changed_slot = true;
        assert_eq!(
            names(&solve(&fix, opts)),
            ["app-misc/bar-1"],
            "SLOT {tree_slot}"
        );
        let mut opts = Solve::new(&atoms);
        opts.noreplace = true;
        assert!(names(&solve(&fix, opts)).is_empty(), "SLOT {tree_slot}");
    }
}

#[test]
fn changed_deps_rebuilds_a_dependency_under_deep_update() {
    let pkgs = [
        pkg("app-misc/top-1", "0", "app-misc/dep"),
        pkg("app-misc/dep-1", "0", "app-misc/new"),
        pkg("app-misc/new-1", "0", ""),
    ];
    let fix = fixture(&pkgs, &["app-misc/new-1"]);
    plant_vdb_slot(&fix.root, "app-misc/top-1", "0", "app-misc/dep");
    plant_vdb_slot(&fix.root, "app-misc/dep-1", "0", "");
    let atoms = [TargetAtom::explicit("app-misc/top")];
    let mut opts = Solve::new(&atoms);
    opts.update = true;
    opts.deep = true;
    opts.changed_deps = true;
    assert_eq!(names(&solve(&fix, opts)), ["app-misc/dep-1"]);
}

/// A local `PKGDIR` index holding `app-misc/bar-1` builds 1 and 3
fn bar_binpkgs(dir: &tempfile::TempDir) -> portage_binpkg::BinpkgIndex {
    let entry = |id: u32| {
        format!("CPV: app-misc/bar-1\nPATH: app-misc/bar/bar-1-{id}.gpkg.tar\nBUILD_ID: {id}\n")
    };
    let text = format!("VERSION: 0\n\n{}\n{}", entry(1), entry(3));
    std::fs::write(dir.path().join("Packages"), text).unwrap();
    portage_binpkg::BinpkgIndex::open(dir.path()).unwrap()
}

#[test]
fn a_build_id_target_pins_its_binary_package() {
    let fix = fixture(&pair(), &[]);
    let pkgdir = tempfile::tempdir().unwrap();
    bar_binpkgs(&pkgdir);
    let flags = crate::cli::MergeFlags {
        usepkg: true,
        ..Default::default()
    };
    let policy = crate::binpkg::ReusePolicy::new(&flags, false, false);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let preview = rt.block_on(crate::binpkg::BinpkgPreview::for_test(
        pkgdir.path(),
        policy,
        &fix.roots,
    ));
    drop(rt);
    let atoms = [TargetAtom::explicit("=app-misc/bar-1-3")];
    let outcome = solve(
        &fix,
        Solve {
            binpkg_preview: Some(&preview),
            ..Solve::new(&atoms)
        },
    );
    assert_eq!(names(&outcome), ["app-misc/bar-1"]);
    assert_eq!(outcome.plan[0].build_id, Some(3));
}

#[test]
fn a_build_id_needs_that_binary_package() {
    let pkgdir = tempfile::tempdir().unwrap();
    let index = bar_binpkgs(&pkgdir);
    let cpv = Cpv::parse("app-misc/bar-1").unwrap();
    let atom = "=app-misc/bar-1-3";
    assert!(super::require_binpkg_build(Some(&index), atom, &cpv, 3).is_ok());
    assert!(super::require_binpkg_build(Some(&index), atom, &cpv, 2).is_err());
    assert!(super::require_binpkg_build(None, atom, &cpv, 3).is_err());
}

/// The `-pv` plan of `app-test/foo-1.0` (ebuild `IUSE=nls`) under `args`, with a `PKGDIR` entry
/// built with `CFLAGS=-march=x86-64-v3` and `binpkg`'s `IUSE`/`USE`
fn binpkg_plan(
    args: &[&str],
    binpkg: &str,
    cflags: &str,
    use_mask: &str,
) -> super::DepgraphOutcome {
    let pkgs = [Pkg {
        cpv: "app-test/foo-1.0",
        keywords: "amd64",
        depend: "",
        rdepend: "",
        iuse: "nls",
        slot: "0",
        required_use: "",
    }];
    let fix = fixture(&pkgs, &[]);
    let base = fix.root.parent().unwrap();
    std::fs::write(base.join("repo/profiles/base/use.mask"), use_mask).unwrap();
    std::fs::write(
        fix.root.join("etc/portage/make.conf"),
        format!(
            "ARCH=\"amd64\"\nACCEPT_KEYWORDS=\"amd64\"\nCHOST=\"x86_64-pc-linux-gnu\"\n\
             CFLAGS=\"{cflags}\"\n"
        ),
    )
    .unwrap();
    let pkgdir = base.join("pkgdir");
    std::fs::create_dir_all(&pkgdir).unwrap();
    std::fs::write(
        pkgdir.join("Packages"),
        format!(
            "VERSION: 0\n\nCPV: app-test/foo-1.0\nCHOST: x86_64-pc-linux-gnu\nCFLAGS: -march=x86-64-v3\n\
             {binpkg}PATH: app-test/foo/foo-1.0-1.gpkg.tar\n"
        ),
    )
    .unwrap();
    let cli = crate::cli::parse_cli(&[&["em", "emerge"], args, &["app-test/foo"]].concat());
    let f = cli.depgraph_flags();
    let policy = crate::binpkg::ReusePolicy::new(&cli.merge_flags(), f.newuse, f.changed_use);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let preview = rt.block_on(crate::binpkg::BinpkgPreview::for_test(
        pkgdir.as_std_path(),
        policy,
        &fix.roots,
    ));
    drop(rt);
    let atoms = [TargetAtom::explicit("app-test/foo")];
    let mut opts = Solve::new(&atoms);
    opts.binpkg_preview = Some(&preview);
    opts.verbose = 1;
    let outcome = solve(&fix, opts);
    assert_eq!(outcome.rows.len(), 1);
    outcome
}

#[test]
fn the_preview_takes_a_keyed_binpkg_only_when_the_build_env_key_matches() {
    let plan = binpkg_plan(&["-k"], "IUSE: nls\n", "-march=x86-64-v3", "");
    assert!(plan.rows[0].starts_with("[binary"), "{}", plan.rows[0]);
    let plan = binpkg_plan(&["-k"], "IUSE: nls\n", "-march=x86-64-v2", "");
    assert!(plan.rows[0].starts_with("[ebuild"), "{}", plan.rows[0]);
}

#[test]
fn a_flag_dropped_from_iuse_does_not_reject_the_binpkg_when_the_profile_masks_it() {
    let plan = binpkg_plan(&["-k"], "IUSE: nls foo\n", "-march=x86-64-v3", "foo\n");
    assert!(plan.rows[0].starts_with("[binary"), "{}", plan.rows[0]);
    let facts = plan.plan[0].facts.as_ref().unwrap();
    assert!(facts.forced.contains("foo"), "the merge's exempt set");
    let plan = binpkg_plan(&["-k"], "IUSE: nls foo\n", "-march=x86-64-v3", "");
    assert!(plan.rows[0].starts_with("[ebuild"), "{}", plan.rows[0]);
}

#[test]
fn usepkgonly_shows_a_binpkg_with_the_use_it_was_built_with() {
    let plan = binpkg_plan(&["-K"], "IUSE: nls\nUSE: nls\n", "-march=x86-64-v3", "");
    assert!(plan.rows[0].starts_with("[binary"), "{}", plan.rows[0]);
    assert!(plan.rows[0].contains("USE=\"nls\""), "{}", plan.rows[0]);
}

#[test]
fn only_the_first_required_use_failure_is_reported() {
    let pkg = |cpv| Pkg {
        cpv,
        keywords: "amd64",
        depend: "",
        rdepend: "",
        iuse: "x",
        slot: "0",
        required_use: "x",
    };
    let fix = fixture(&[pkg("app-misc/a-1"), pkg("app-misc/b-1")], &[]);
    let atoms = [
        TargetAtom::explicit("app-misc/a"),
        TargetAtom::explicit("app-misc/b"),
    ];
    let outcome = solve(&fix, Solve::new(&atoms));
    assert_eq!(outcome.required_use_reported.len(), 1);
    assert_eq!(outcome.exit_code, 1);
}

#[test]
fn a_required_use_failure_is_reported_on_the_target_before_its_deps() {
    let pkg = |cpv, depend| Pkg {
        cpv,
        keywords: "amd64",
        depend,
        iuse: "x",
        slot: "0",
        required_use: "x",
    };
    let fix = fixture(
        &[pkg("app-misc/a-1", "app-misc/b"), pkg("app-misc/b-1", "")],
        &[],
    );
    let atoms = [TargetAtom::explicit("app-misc/a")];
    let outcome = solve(&fix, Solve::new(&atoms));
    assert_eq!(
        outcome.required_use_reported,
        ["app-misc/a-1".parse::<Cpv>().unwrap()]
    );
    assert_eq!(outcome.exit_code, 1);
}
