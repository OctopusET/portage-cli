//! The metadata a merged package records in the VDB, reduced the way portage stores it
//!
//! `*DEPEND`, `LICENSE`, `RESTRICT` and `PROPERTIES` are written with their USE
//! conditionals evaluated, redundant groups flattened and `:=` bound to the
//! installed provider's slot; `RDEPEND` also gains an implicit `>=libc`.

use std::collections::{BTreeMap, HashSet};

use camino::{Utf8Path, Utf8PathBuf};
use portage_atom::{Cpn, Cpv, Dep, Slot, SlotDep, SlotOperator, UseDep, UseDepKind};
use portage_metadata::Eapi;
use portage_repo::EbuildEnv;
use portage_vdb::Vdb;

use crate::elfscan::ElfScan;

/// Reduce `env`'s raw fields against its own USE and bind `:=` deps
///
/// `target` is the merge root's VDB, `running` the build root's (`BDEPEND`).
pub(super) fn reduce_env(env: &mut EbuildEnv, target: &Vdb, running: &Vdb) {
    let use_flags: HashSet<&str> = env.use_flags.iter().map(String::as_str).collect();
    let plain = |field: &mut Option<String>| {
        *field = field
            .take()
            .and_then(|raw| reduce(&raw, &use_flags, &|t| t.to_owned()));
    };
    plain(&mut env.license);
    plain(&mut env.restrict);
    plain(&mut env.properties);

    let has_bdepend = env.eapi.parse::<Eapi>().is_ok_and(|e| e.has_bdepend());
    let depend_vdbs: &[&Vdb] = if has_bdepend {
        &[target]
    } else {
        &[target, running]
    };
    let deps = |field: &mut Option<String>, vdbs: &[&Vdb]| {
        *field = field
            .take()
            .and_then(|raw| reduce(&raw, &use_flags, &|t| reduce_atom(t, &use_flags, vdbs)));
    };
    deps(&mut env.depend, depend_vdbs);
    deps(&mut env.bdepend, &[running]);
    deps(&mut env.rdepend, &[target]);
    deps(&mut env.pdepend, &[target]);
    deps(&mut env.idepend, &[target]);
}

/// Take the reduced fields and the build settings from a binpkg's metadata, as on its build host
pub(super) fn take_recorded(env: &mut EbuildEnv, metadata: &BTreeMap<String, String>) {
    let fields = [
        ("CBUILD", &mut env.cbuild),
        ("CC", &mut env.cc),
        ("CFLAGS", &mut env.cflags),
        ("CHOST", &mut env.chost),
        ("CXX", &mut env.cxx),
        ("CXXFLAGS", &mut env.cxxflags),
        ("FEATURES", &mut env.features),
        ("LDFLAGS", &mut env.ldflags),
        ("RUSTFLAGS", &mut env.rustflags),
        ("LICENSE", &mut env.license),
        ("RESTRICT", &mut env.restrict),
        ("PROPERTIES", &mut env.properties),
        ("DEPEND", &mut env.depend),
        ("BDEPEND", &mut env.bdepend),
        ("RDEPEND", &mut env.rdepend),
        ("PDEPEND", &mut env.pdepend),
        ("IDEPEND", &mut env.idepend),
    ];
    for (key, field) in fields {
        *field = metadata.get(key).filter(|v| !v.is_empty()).cloned();
    }
}

/// Where `SIZE` waits between `src_install` and the merge, as portage's `build-info/SIZE`
fn size_file(work_root: &Utf8Path) -> Utf8PathBuf {
    work_root.join("build-info/SIZE")
}

/// Record the `SIZE` of the image as `src_install` left it, before strip and compression
///
/// As portage counts it: regular files, a hardlinked inode once.
pub(super) fn record_size(work_root: &Utf8Path) -> anyhow::Result<()> {
    use anyhow::Context;
    let mut seen = HashSet::new();
    let size = image_size(work_root.join("image").as_std_path(), &mut seen);
    let file = size_file(work_root);
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {dir}"))?;
    }
    std::fs::write(&file, format!("{size}\n")).with_context(|| format!("writing {file}"))
}

/// The `SIZE` [`record_size`] wrote for this build, if any
pub(super) fn recorded_size(work_root: &Utf8Path) -> Option<u64> {
    std::fs::read_to_string(size_file(work_root))
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn image_size(dir: &std::path::Path, seen: &mut HashSet<(u64, u64)>) -> u64 {
    use std::os::unix::fs::MetadataExt;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut size = 0;
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if meta.is_dir() {
            size += image_size(&entry.path(), seen);
        } else if meta.is_file() && seen.insert((meta.dev(), meta.ino())) {
            size += meta.len();
        }
    }
    size
}

/// Append `>=<libc>` to `RDEPEND` of a package that links anything (has `NEEDED.ELF.2`)
pub(super) fn inject_libc_dep(
    rdepend: &mut Option<String>,
    cpv: &Cpv,
    elf: &ElfScan,
    target: &Vdb,
) {
    if elf.needed_elf2.is_empty() {
        return;
    }
    if let Some(libc) = libc_dep(target, cpv) {
        *rdepend = Some(match rdepend.take() {
            Some(r) => format!("{r} {libc}"),
            None => libc,
        });
    }
}

/// `>=<cpv>` for each installed provider of `virtual/libc`, unless `cpv` is one
fn libc_dep(vdb: &Vdb, cpv: &Cpv) -> Option<String> {
    let providers = libc_providers(vdb);
    if providers.is_empty() || providers.iter().any(|p| p.cpn == cpv.cpn) {
        return None;
    }
    let atoms: Vec<String> = providers.iter().map(|p| format!(">={p}")).collect();
    Some(atoms.join(" "))
}

/// The packages providing the installed `virtual/libc`
pub(crate) fn libc_cpns(vdb: &Vdb) -> Vec<Cpn> {
    libc_providers(vdb).into_iter().map(|p| p.cpn).collect()
}

fn libc_providers(vdb: &Vdb) -> Vec<Cpv> {
    let Some(rdepend) = Dep::parse("virtual/libc").ok().and_then(|virtual_libc| {
        installed_matches(vdb, &virtual_libc)
            .into_iter()
            .max_by(|a, b| a.cpv().cmp(b.cpv()))?
            .rdepend()
            .ok()
            .flatten()
    }) else {
        return Vec::new();
    };
    let mut providers = Vec::new();
    portage_atom::DepEntry::walk_atoms(&rdepend, &mut |dep| {
        if dep.blocker.is_none()
            && let Some(p) = installed_matches(vdb, dep)
                .into_iter()
                .min_by(|a, b| a.cpv().cmp(b.cpv()))
        {
            providers.push(p.cpv().clone());
        }
    });
    providers
}

/// Whether a binpkg's recorded dependency string still matches the ebuild's, as
/// `--binpkg-changed-deps` compares them
///
/// Both are reduced with the binpkg's USE; `:=` bindings and top-level `libc` deps don't count.
pub(crate) fn same_deps(
    built: &str,
    ebuild: &str,
    use_flags: &HashSet<&str>,
    libc: &[Cpn],
) -> bool {
    let compared = |raw: &str| {
        let mut tokens = raw.split_whitespace().peekable();
        let Some(nodes) = parse(&mut tokens, false).filter(|_| tokens.peek().is_none()) else {
            return raw.split_whitespace().collect::<Vec<_>>().join(" ");
        };
        let mut out = Vec::new();
        reduce_into(
            nodes,
            use_flags,
            &|t| unbound_atom(t, use_flags),
            false,
            &mut out,
        );
        out.retain(|node| match node {
            Reduced::Token(t) => Dep::parse(t).map_or(true, |d| !libc.contains(&d.cpn)),
            _ => true,
        });
        render(&out)
    };
    compared(built) == compared(ebuild)
}

/// `token` with its conditional USE deps evaluated and any `:slot=` binding cut to a bare `:=`
fn unbound_atom(token: &str, use_flags: &HashSet<&str>) -> String {
    let Ok(mut dep) = Dep::parse(token) else {
        return token.to_owned();
    };
    evaluate_use_deps(&mut dep, use_flags);
    if let Some(SlotDep::Slot {
        op: Some(SlotOperator::Equal),
        ..
    }) = dep.slot_dep
    {
        dep.slot_dep = Some(SlotDep::Operator(SlotOperator::Equal));
    }
    dep.to_string()
}

fn installed_matches(vdb: &Vdb, dep: &Dep) -> Vec<portage_vdb::InstalledPackage> {
    let Some(category) = vdb.category(dep.category()) else {
        return Vec::new();
    };
    category
        .packages()
        .collect_vec()
        .into_iter()
        .filter(|p| p.cpn() == &dep.cpn && dep.matches_cpv(p.cpv(), p.slot().ok().as_ref()))
        .collect()
}

/// Evaluate an atom's conditional USE deps and bind a `:=` to the best installed match
///
/// Tokens that need neither are returned verbatim, so their spelling survives.
fn reduce_atom(token: &str, use_flags: &HashSet<&str>, vdbs: &[&Vdb]) -> String {
    let Ok(mut dep) = Dep::parse(token) else {
        return token.to_owned();
    };
    let conditional = dep.use_deps.as_ref().is_some_and(|u| {
        u.iter()
            .any(|d| !matches!(d.kind, UseDepKind::Enabled | UseDepKind::Disabled))
    });
    let slot_equal = matches!(
        dep.slot_dep,
        Some(SlotDep::Operator(SlotOperator::Equal))
            | Some(SlotDep::Slot {
                op: Some(SlotOperator::Equal),
                ..
            })
    );
    if !conditional && !slot_equal {
        return token.to_owned();
    }
    evaluate_use_deps(&mut dep, use_flags);
    if slot_equal
        && let Some(slot) = vdbs.iter().find_map(|vdb| {
            installed_matches(vdb, &dep)
                .into_iter()
                .max_by(|a, b| a.cpv().cmp(b.cpv()))
                .and_then(|p| p.slot().ok())
        })
    {
        let subslot = slot.subslot.unwrap_or(slot.slot);
        dep.slot_dep = Some(SlotDep::Slot {
            slot: Some(Slot::with_subslot(slot.slot.as_str(), subslot.as_str())),
            op: Some(SlotOperator::Equal),
        });
    }
    dep.to_string()
}

fn evaluate_use_deps(dep: &mut Dep, use_flags: &HashSet<&str>) {
    if let Some(use_deps) = dep.use_deps.take() {
        let evaluated: Vec<UseDep> = use_deps
            .into_iter()
            .filter_map(|d| evaluate_use_dep(d, use_flags.contains(d.flag.as_str())))
            .collect();
        dep.use_deps = (!evaluated.is_empty()).then_some(evaluated);
    }
}

/// PMS 8.3.4: a conditional USE dep becomes plain `[flag]`/`[-flag]` or drops out
fn evaluate_use_dep(dep: UseDep, parent: bool) -> Option<UseDep> {
    let enabled = match dep.kind {
        UseDepKind::Enabled | UseDepKind::Disabled => return Some(dep),
        UseDepKind::Conditional => parent.then_some(true)?,
        UseDepKind::ConditionalInverse => (!parent).then_some(false)?,
        UseDepKind::Equal => parent,
        UseDepKind::EqualInverse => !parent,
    };
    let kind = if enabled {
        UseDepKind::Enabled
    } else {
        UseDepKind::Disabled
    };
    Some(UseDep { kind, ..dep })
}

/// One parsed node of a dependency-style string
enum Node<'a> {
    Token(&'a str),
    /// `( … )`, `|| ( … )` or `flag? ( … )`, keyed by the token before the paren
    Group(Option<&'a str>, Vec<Node<'a>>),
}

/// A reduced node: only plain tokens, all-of groups and `||` groups remain
enum Reduced {
    Token(String),
    AllOf(Vec<Reduced>),
    AnyOf(Vec<Reduced>),
}

/// Evaluate `raw`'s USE conditionals and flatten it as portage records it
///
/// `None` when nothing is left. A string that does not parse is kept as-is.
fn reduce(raw: &str, use_flags: &HashSet<&str>, map: &dyn Fn(&str) -> String) -> Option<String> {
    let mut tokens = raw.split_whitespace().peekable();
    let Some(nodes) = parse(&mut tokens, false).filter(|_| tokens.peek().is_none()) else {
        let normalized = raw.split_whitespace().collect::<Vec<_>>().join(" ");
        return (!normalized.is_empty()).then_some(normalized);
    };
    let mut out = Vec::new();
    reduce_into(nodes, use_flags, map, false, &mut out);
    (!out.is_empty()).then(|| render(&out))
}

fn parse<'a>(
    tokens: &mut std::iter::Peekable<impl Iterator<Item = &'a str>>,
    nested: bool,
) -> Option<Vec<Node<'a>>> {
    let mut nodes = Vec::new();
    while let Some(token) = tokens.next() {
        match token {
            ")" => return nested.then_some(nodes),
            "(" => nodes.push(Node::Group(None, parse(tokens, true)?)),
            t if t == "||" || t.ends_with('?') => {
                if tokens.next()? != "(" {
                    return None;
                }
                nodes.push(Node::Group(Some(t), parse(tokens, true)?));
            }
            t => nodes.push(Node::Token(t)),
        }
    }
    (!nested).then_some(nodes)
}

/// `in_any_of`: `nodes` are the direct choices of an `||`, where groups keep their parens
fn reduce_into(
    nodes: Vec<Node<'_>>,
    use_flags: &HashSet<&str>,
    map: &dyn Fn(&str) -> String,
    in_any_of: bool,
    out: &mut Vec<Reduced>,
) {
    for node in nodes {
        let (head, children) = match node {
            Node::Token(t) => {
                out.push(Reduced::Token(map(t)));
                continue;
            }
            Node::Group(head, children) => (head, children),
        };
        let mut inner = Vec::new();
        if head == Some("||") {
            reduce_into(children, use_flags, map, true, &mut inner);
            match inner.len() {
                0 => {}
                1 => match inner.pop() {
                    Some(Reduced::AllOf(all)) if !in_any_of => out.extend(all),
                    Some(single) => out.push(single),
                    None => {}
                },
                _ if in_any_of => out.extend(inner),
                _ => out.push(Reduced::AnyOf(inner)),
            }
            continue;
        }
        if let Some(cond) = head {
            let flag = cond.trim_end_matches('?');
            let active = match flag.strip_prefix('!') {
                Some(f) => !use_flags.contains(f),
                None => use_flags.contains(flag),
            };
            if !active {
                continue;
            }
        }
        if !in_any_of {
            reduce_into(children, use_flags, map, false, out);
            continue;
        }
        reduce_into(children, use_flags, map, false, &mut inner);
        if inner.len() > 1 {
            out.push(Reduced::AllOf(inner));
        } else if let Some(Reduced::AnyOf(choices)) = inner.last_mut() {
            out.append(choices);
        } else {
            out.extend(inner);
        }
    }
}

fn render(nodes: &[Reduced]) -> String {
    let parts: Vec<String> = nodes
        .iter()
        .map(|n| match n {
            Reduced::Token(t) => t.clone(),
            Reduced::AllOf(c) => format!("( {} )", render(c)),
            Reduced::AnyOf(c) => format!("|| ( {} )", render(c)),
        })
        .collect();
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(raw: &str, use_flags: &[&str]) -> Option<String> {
        let use_flags: HashSet<&str> = use_flags.iter().copied().collect();
        reduce(raw, &use_flags, &|t| t.to_owned())
    }

    #[test]
    fn conditionals_and_all_of_groups_flatten_outside_any_of() {
        assert_eq!(
            plain(
                "a\n\tpam? ( b ) selinux? ( c ) ( d ( e ) ) !x? ( f )",
                &["pam"]
            )
            .as_deref(),
            Some("a b d e f")
        );
        assert_eq!(plain("x? ( a )", &[]), None);
    }

    #[test]
    fn any_of_keeps_groups_and_collapses_single_choices() {
        assert_eq!(
            plain("|| ( ( a b ) c ) || ( d ) || ( x? ( e ) f )", &[]).as_deref(),
            Some("|| ( ( a b ) c ) d f")
        );
        assert_eq!(
            plain("|| ( a || ( b c ) ) || ( p? ( g h ) i )", &["p"]).as_deref(),
            Some("|| ( a b c ) || ( ( g h ) i )")
        );
        assert_eq!(plain("|| ( ( a b ) )", &[]).as_deref(), Some("a b"));
        assert_eq!(
            plain("|| ( p? ( || ( a b ) ) c ) || ( ( || ( d e ) ) f )", &["p"]).as_deref(),
            Some("|| ( a b c ) || ( d e f )")
        );
    }

    #[test]
    fn unparsable_string_is_kept() {
        assert_eq!(plain("a ( b", &[]).as_deref(), Some("a ( b"));
    }

    fn vdb_with(dir: &std::path::Path, pkgs: &[(&str, &str, &str)]) -> Vdb {
        for (cat, pf, slot) in pkgs {
            let d = dir.join(cat).join(pf);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("SLOT"), format!("{slot}\n")).unwrap();
        }
        Vdb::open(camino::Utf8Path::from_path(dir).unwrap()).unwrap()
    }

    #[test]
    fn atoms_bind_slot_operator_and_evaluate_use_deps() {
        let tmp = tempfile::tempdir().unwrap();
        let vdb = vdb_with(
            tmp.path(),
            &[
                ("sys-libs", "ncurses-6.5", "0/6"),
                ("dev-libs", "foo-1", "2"),
                ("dev-libs", "foo-3", "3/3.1"),
            ],
        );
        let use_flags: HashSet<&str> = ["ssl"].into_iter().collect();
        let atom = |t| reduce_atom(t, &use_flags, &[&vdb]);
        assert_eq!(
            atom(">=sys-libs/ncurses-5.2:="),
            ">=sys-libs/ncurses-5.2:0/6="
        );
        assert_eq!(atom("dev-libs/foo:2="), "dev-libs/foo:2/2=");
        assert_eq!(atom("dev-libs/foo:="), "dev-libs/foo:3/3.1=");
        assert_eq!(atom("dev-libs/bar:="), "dev-libs/bar:=");
        assert_eq!(
            atom("dev-libs/bar[ssl?,gtk?,!nls?,doc=,!qt=,-x]"),
            "dev-libs/bar[ssl,-nls,-doc,qt,-x]"
        );
        assert_eq!(atom("dev-libs/bar[gtk?]"), "dev-libs/bar");
        assert_eq!(atom("<dev-libs/bar-1.0_rc1"), "<dev-libs/bar-1.0_rc1");
    }

    #[test]
    fn libc_dep_names_the_virtuals_installed_provider_except_for_itself() {
        let tmp = tempfile::tempdir().unwrap();
        let vdb = vdb_with(
            tmp.path(),
            &[
                ("virtual", "libc-1-r1", "0"),
                ("sys-libs", "glibc-2.43-r4", "2.2"),
            ],
        );
        std::fs::write(
            tmp.path().join("virtual/libc-1-r1/RDEPEND"),
            "sys-libs/glibc:2.2\n",
        )
        .unwrap();
        let cpv = |s| Cpv::parse(s).unwrap();
        assert_eq!(
            libc_dep(&vdb, &cpv("app-text/tree-2.2.1")).as_deref(),
            Some(">=sys-libs/glibc-2.43-r4")
        );
        assert_eq!(libc_dep(&vdb, &cpv("sys-libs/glibc-2.44")), None);
    }

    #[test]
    fn changed_deps_ignore_slot_bindings_and_libc_but_not_new_atoms() {
        let use_flags: HashSet<&str> = ["ssl"].into();
        let libc = [Cpn::parse("sys-libs/glibc").unwrap()];
        let same = |built: &str, ebuild: &str| same_deps(built, ebuild, &use_flags, &libc);
        assert!(same(
            "dev-libs/a:0/1= dev-libs/b[ssl] >=sys-libs/glibc-2.40",
            "dev-libs/a:= ssl? ( dev-libs/b[ssl?] ) !ssl? ( dev-libs/c )",
        ));
        assert!(!same("dev-libs/a:0/1=", "dev-libs/a:= dev-libs/b"));
        assert!(!same("dev-libs/a", "ssl? ( >=dev-libs/a-2 )"));
    }
}
