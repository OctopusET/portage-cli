//! `--changed-deps` / `--changed-slot`: whether an installed package's recorded
//! metadata differs from the same version's ebuild

use std::collections::HashSet;

use portage_atom::interner::{DefaultInterner, Interned};
use portage_atom::{Cpn, DepEntry, SlotDep, SlotOperator};
use portage_metadata::EbuildMetadata;

use crate::installed::VdbEntry;

/// Whether `installed`'s dependencies differ from `ebuild`'s, both evaluated under the installed USE
///
/// `with_bdeps` compares DEPEND and BDEPEND too, not just the runtime classes. A `:SLOT/SUB=`
/// binding counts as plain `:=`, and a top-level atom on a `libc` provider is ignored, since
/// the package manager records both at merge time.
pub fn deps_changed(
    installed: &VdbEntry,
    ebuild: &EbuildMetadata,
    with_bdeps: bool,
    libc: &HashSet<Cpn>,
) -> bool {
    let active: HashSet<Interned<DefaultInterner>> = installed.active_use.iter().copied().collect();
    let vdb = &installed.classes;
    let mut pairs = vec![
        (&vdb.idepend, &ebuild.idepend),
        (&vdb.pdepend, &ebuild.pdepend),
        (&vdb.rdepend, &ebuild.rdepend),
    ];
    if with_bdeps {
        pairs.push((&vdb.bdepend, &ebuild.bdepend));
        pairs.push((&vdb.depend, &ebuild.depend));
    }
    let empty_groups_match = ebuild.eapi.empty_any_of_matches();
    let reduce = |entries: &[DepEntry]| {
        let evaluated = DepEntry::evaluate_use_groups(entries, &active, empty_groups_match);
        let mut out = Vec::new();
        normalize(&evaluated, false, &mut out);
        out.retain(|e| !matches!(e, DepEntry::Atom(d) if libc.contains(&d.cpn)));
        out
    };
    pairs
        .into_iter()
        .any(|(built, tree)| reduce(built) != reduce(tree.list()))
}

/// The packages providing `virtual/libc` on this root, per its installed RDEPEND
pub fn libc_providers(installed: &[VdbEntry]) -> HashSet<Cpn> {
    let mut out = HashSet::new();
    for e in installed {
        if e.cpn.category.as_str() == "virtual" && e.cpn.package.as_str() == "libc" {
            let active: HashSet<Interned<DefaultInterner>> = e.active_use.iter().copied().collect();
            let rdepend = DepEntry::evaluate_use(&e.classes.rdepend, &active);
            DepEntry::walk_atoms(&rdepend, &mut |d| {
                if d.blocker.is_none() {
                    out.insert(d.cpn);
                }
            });
        }
    }
    out
}

/// Whether `installed`'s SLOT or sub-slot differs from `ebuild`'s
pub fn slot_changed(installed: &VdbEntry, ebuild: &EbuildMetadata) -> bool {
    let Some(slot) = installed.slot else {
        return false;
    };
    let tree = &ebuild.slot;
    (tree.slot, tree.subslot.unwrap_or(tree.slot)) != (slot, installed.subslot.unwrap_or(slot))
}

/// The shape a recorded dependency string has: all-of groups inlined (kept as an `||`
/// alternative), a one-alternative `||` replaced by that alternative, slot bindings dropped
fn normalize(entries: &[DepEntry], under_any_of: bool, out: &mut Vec<DepEntry>) {
    for entry in entries {
        match entry {
            DepEntry::Atom(dep) => {
                let mut dep = dep.clone();
                if let Some(SlotDep::Slot {
                    slot: Some(_),
                    op: Some(SlotOperator::Equal),
                }) = dep.slot_dep
                {
                    dep.slot_dep = Some(SlotDep::Operator(SlotOperator::Equal));
                }
                out.push(DepEntry::Atom(dep));
            }
            DepEntry::AllOf(children) => normalize(children, under_any_of, out),
            DepEntry::AnyOf(alts) => {
                let mut alts: Vec<DepEntry> = alts.iter().map(normalize_alternative).collect();
                if alts.len() == 1 {
                    match alts.remove(0) {
                        DepEntry::AllOf(c) if !under_any_of => out.extend(c),
                        alt => out.push(alt),
                    }
                } else {
                    out.push(DepEntry::AnyOf(alts));
                }
            }
            other => out.push(other.clone()),
        }
    }
}

fn normalize_alternative(alt: &DepEntry) -> DepEntry {
    let mut out = Vec::new();
    match alt {
        DepEntry::AllOf(children) => {
            normalize(children, true, &mut out);
            DepEntry::AllOf(out)
        }
        other => {
            normalize(std::slice::from_ref(other), true, &mut out);
            out.pop().unwrap_or(DepEntry::AllOf(Vec::new()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use portage_atom::Version;
    use portage_atom_pubgrub::PackageDeps;
    use portage_metadata::CacheEntry;

    fn installed(rdepend: &str, use_: &[&str]) -> VdbEntry {
        VdbEntry {
            cpn: Cpn::parse("app-misc/foo").unwrap(),
            slot: Some(Interned::intern("0")),
            subslot: None,
            repo: None,
            version: Version::parse("1").unwrap(),
            active_use: use_.iter().copied().map(Interned::intern).collect(),
            iuse: Vec::new(),
            eapi: portage_metadata::Eapi::Eight,
            deps: Vec::new(),
            classes: PackageDeps::new(
                Vec::new(),
                DepEntry::parse(rdepend).unwrap(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
            ),
        }
    }

    fn ebuild(fields: &str) -> EbuildMetadata {
        CacheEntry::parse(&format!("EAPI=8\nSLOT=0\nDESCRIPTION=t\n{fields}"))
            .unwrap()
            .metadata
    }

    #[test]
    fn a_changed_rdepend_is_changed_deps() {
        let tree = ebuild("RDEPEND=dev-libs/a dev-libs/b\n");
        assert!(deps_changed(
            &installed("dev-libs/a", &[]),
            &tree,
            false,
            &HashSet::new()
        ));
    }

    #[test]
    fn a_slot_binding_or_unselected_branch_is_not_a_change() {
        let tree = ebuild("RDEPEND=dev-libs/a:= ssl? ( dev-libs/openssl ) ( dev-libs/c )\n");
        let vdb = installed("dev-libs/a:0/1.2= dev-libs/c", &[]);
        assert!(!deps_changed(&vdb, &tree, false, &HashSet::new()));
        let vdb = installed("dev-libs/a:0/1.2= dev-libs/c", &["ssl"]);
        assert!(deps_changed(&vdb, &tree, false, &HashSet::new()));
    }

    #[test]
    fn an_injected_libc_dep_and_unrequested_build_deps_are_ignored() {
        let tree = ebuild("RDEPEND=dev-libs/a\nDEPEND=dev-libs/new\n");
        let vdb = installed("dev-libs/a >=sys-libs/glibc-2.41", &[]);
        let libc = HashSet::from([Cpn::parse("sys-libs/glibc").unwrap()]);
        assert!(!deps_changed(&vdb, &tree, false, &libc));
        assert!(deps_changed(&vdb, &tree, true, &libc));
    }

    #[test]
    fn a_new_sub_slot_is_changed_slot() {
        let vdb = installed("", &[]);
        assert!(!slot_changed(&vdb, &ebuild("")));
        let tree = CacheEntry::parse("EAPI=8\nSLOT=0/2\nDESCRIPTION=t\n")
            .unwrap()
            .metadata;
        assert!(slot_changed(&vdb, &tree));
    }
}
