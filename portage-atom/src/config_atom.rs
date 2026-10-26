//! Atoms of user `/etc/portage/package.*` files, which add Portage's extended wildcard
//! syntax (`*/*`, `cat/*`, `*/pkg`, `=cat/pkg-*ver*`) to the PMS [`Dep`] form

use std::cmp::Reverse;
use std::collections::HashMap;
use std::fmt;

use crate::interner::{DefaultInterner, Interned};
use crate::{Cpn, Cpv, Dep, Error, Operator, Result, Slot, SlotDep, Version};

/// A `package.*` atom whose category and package may contain `*` globs
///
/// Not PMS: Portage's extended syntax for user configuration. It carries no
/// version operator except the `=cat/pkg-*substr*` form, which matches any
/// version whose string contains `substr`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ExtendedAtom {
    category: Box<str>,
    package: Box<str>,
    version: Option<Box<str>>,
    slot: Option<Slot>,
    repo: Option<Interned<DefaultInterner>>,
}

impl ExtendedAtom {
    /// Parse an extended atom; one without a `*` in its name is rejected, being a plain [`Dep`]
    pub fn parse(input: &str) -> Result<Self> {
        let invalid = || Error::InvalidDep(format!("{input}: not an extended atom"));
        let (rest, repo) = match input.split_once("::") {
            Some((rest, repo)) if valid_repo(repo) => (rest, Some(Interned::intern(repo))),
            Some(_) => return Err(invalid()),
            None => (input, None),
        };
        let (rest, slot) = match rest.split_once(':') {
            Some((rest, slot)) => (rest, Some(Slot::parse(slot).map_err(|_| invalid())?)),
            None => (rest, None),
        };
        let (rest, version) = match rest.strip_prefix('=') {
            Some(rest) => {
                let (name, ver) = rest.rsplit_once("-*").ok_or_else(invalid)?;
                let ver = ver.strip_suffix('*').ok_or_else(invalid)?;
                if ver.is_empty() || !ver.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
                    return Err(invalid());
                }
                (name, Some(ver))
            }
            None => (rest, None),
        };
        let (category, package) = rest.split_once('/').ok_or_else(invalid)?;
        let name_ok = |s: &str, extra: &[u8]| {
            let mut bytes = s.bytes();
            bytes
                .next()
                .is_some_and(|b| b.is_ascii_alphanumeric() || b"_+*".contains(&b))
                && bytes.all(|b| {
                    b.is_ascii_alphanumeric() || b"_+*-".contains(&b) || extra.contains(&b)
                })
        };
        if !name_ok(category, b".")
            || !name_ok(package, b"")
            || rest.contains("**")
            || (version.is_none() && !rest.contains('*'))
            || (version.is_none() && ends_in_version(package))
        {
            return Err(invalid());
        }
        Ok(Self {
            category: category.into(),
            package: package.into(),
            version: version.map(Into::into),
            slot,
            repo,
        })
    }

    /// Whether this atom matches `cpv`; an unknown `slot` or `repo` does not refute it
    pub fn matches(
        &self,
        cpv: &Cpv,
        slot: Option<&Slot>,
        repo: Option<Interned<DefaultInterner>>,
    ) -> bool {
        self.matches_cpn(&cpv.cpn)
            && self
                .version
                .as_deref()
                .is_none_or(|v| cpv.version.to_string().contains(v))
            && slot_matches(self.slot.as_ref(), slot)
            && repo_matches(self.repo, repo)
    }

    /// Whether the category and package globs match `cpn`
    pub fn matches_cpn(&self, cpn: &Cpn) -> bool {
        glob_match(&self.category, &cpn.category) && glob_match(&self.package, &cpn.package)
    }

    /// `*/*` with no slot, repo or version: Portage folds such a line into the global setting
    pub fn is_match_all(&self) -> bool {
        &*self.category == "*"
            && &*self.package == "*"
            && self.version.is_none()
            && self.slot.is_none()
            && self.repo.is_none()
    }
}

impl fmt::Display for ExtendedAtom {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if self.version.is_some() {
            write!(f, "=")?;
        }
        write!(f, "{}/{}", self.category, self.package)?;
        if let Some(v) = &self.version {
            write!(f, "-*{v}*")?;
        }
        if let Some(slot) = &self.slot {
            write!(f, ":{slot}")?;
        }
        if let Some(repo) = &self.repo {
            write!(f, "::{repo}")?;
        }
        Ok(())
    }
}

/// The atom of a user `package.*` line: a PMS [`Dep`] or an [`ExtendedAtom`]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ConfigAtom {
    /// A PMS atom
    Dep(Dep),
    /// A wildcard atom
    Extended(ExtendedAtom),
}

impl ConfigAtom {
    /// Parse as a [`Dep`], falling back to the extended syntax; the error is the [`Dep`] one
    pub fn parse(input: &str) -> Result<Self> {
        match Dep::parse(input) {
            Ok(dep) => Ok(Self::Dep(dep)),
            Err(e) => ExtendedAtom::parse(input)
                .map(Self::Extended)
                .map_err(|_| e),
        }
    }

    /// The single package this atom names, `None` for a wildcard
    pub fn cpn(&self) -> Option<&Cpn> {
        match self {
            Self::Dep(dep) => Some(&dep.cpn),
            Self::Extended(_) => None,
        }
    }

    /// The `::repo` qualifier
    pub fn repo(&self) -> Option<Interned<DefaultInterner>> {
        match self {
            Self::Dep(dep) => dep.repo,
            Self::Extended(e) => e.repo,
        }
    }

    /// This atom with its `::repo` qualifier dropped
    pub fn without_repo(&self) -> Self {
        let mut out = self.clone();
        match &mut out {
            Self::Dep(dep) => dep.repo = None,
            Self::Extended(e) => e.repo = None,
        }
        out
    }

    /// Whether this atom matches `cpv`; an unknown `slot` or `repo` does not refute it
    pub fn matches(
        &self,
        cpv: &Cpv,
        slot: Option<&Slot>,
        repo: Option<Interned<DefaultInterner>>,
    ) -> bool {
        match self {
            Self::Dep(dep) => dep.matches_cpv(cpv, slot) && repo_matches(dep.repo, repo),
            Self::Extended(e) => e.matches(cpv, slot, repo),
        }
    }

    /// Whether this atom names `cpn` (for a wildcard, whether its globs match it)
    pub fn matches_cpn(&self, cpn: &Cpn) -> bool {
        match self {
            Self::Dep(dep) => dep.cpn == *cpn,
            Self::Extended(e) => e.matches_cpn(cpn),
        }
    }

    /// `*/*` with nothing else; see [`ExtendedAtom::is_match_all`]
    pub fn is_match_all(&self) -> bool {
        matches!(self, Self::Extended(e) if e.is_match_all())
    }

    /// Portage's atom specificity rank (`best_match_to_list`): higher overrides lower
    ///
    /// `=cpv` 6, `~cpv` 5, `=cpv*` 4, `:slot` 3, ranged 2, bare 1; wildcards rank
    /// below any plain atom: `=*substr*` 0, with a slot -1, else -2.
    pub fn specificity(&self) -> i8 {
        match self {
            Self::Dep(dep) => {
                let op = match (dep.op, dep.glob) {
                    (Some(Operator::Equal), false) => 6,
                    (Some(Operator::Approximate), _) => 5,
                    (Some(Operator::Equal), true) => 4,
                    (Some(_), _) => 2,
                    (None, _) => 1,
                };
                let slotted = matches!(dep.slot_dep, Some(SlotDep::Slot { slot: Some(_), .. }));
                if slotted { op.max(3) } else { op }
            }
            Self::Extended(e) if e.version.is_some() => 0,
            Self::Extended(e) if e.slot.is_some() => -1,
            Self::Extended(_) => -2,
        }
    }
}

impl From<Dep> for ConfigAtom {
    fn from(dep: Dep) -> Self {
        Self::Dep(dep)
    }
}

impl fmt::Display for ConfigAtom {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Dep(dep) => dep.fmt(f),
            Self::Extended(e) => e.fmt(f),
        }
    }
}

/// Reorder one config file's entries so that folding them in order applies the most
/// specific atom last, as Portage's `ordered_by_atom_specificity` does
///
/// Lines of one atom stay together in file order; between distinct atoms of equal
/// rank the one written first applies last. Portage's version-distance tie-break
/// between two ranged atoms is not reproduced.
pub fn order_by_specificity<T>(entries: Vec<(ConfigAtom, T)>) -> Vec<(ConfigAtom, T)> {
    let keys: Vec<(i8, Reverse<usize>)> = {
        let mut first: HashMap<&ConfigAtom, usize> = HashMap::new();
        entries
            .iter()
            .enumerate()
            .map(|(i, (atom, _))| (atom.specificity(), Reverse(*first.entry(atom).or_insert(i))))
            .collect()
    };
    let mut tagged: Vec<_> = keys.into_iter().zip(entries).collect();
    tagged.sort_by_key(|(key, _)| *key);
    tagged.into_iter().map(|(_, entry)| entry).collect()
}

fn slot_matches(want: Option<&Slot>, have: Option<&Slot>) -> bool {
    let (Some(want), Some(have)) = (want, have) else {
        return true;
    };
    want.slot == have.slot
        && match (want.subslot, have.subslot) {
            (Some(w), Some(h)) => w == h,
            _ => true,
        }
}

fn repo_matches(
    want: Option<Interned<DefaultInterner>>,
    have: Option<Interned<DefaultInterner>>,
) -> bool {
    match (want, have) {
        (Some(w), Some(h)) => w == h,
        _ => true,
    }
}

fn valid_repo(repo: &str) -> bool {
    let mut bytes = repo.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn ends_in_version(package: &str) -> bool {
    package
        .match_indices('-')
        .any(|(i, _)| Version::parse(&package[i + 1..]).is_ok())
}

/// `*` matches any run of characters, everything else itself
fn glob_match(pattern: &str, text: &str) -> bool {
    let (p, t) = (pattern.as_bytes(), text.as_bytes());
    let (mut pi, mut ti) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && p[pi] == b'*' {
            star = Some((pi, ti));
            pi += 1;
        } else if pi < p.len() && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&b| b == b'*')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cpv(s: &str) -> Cpv {
        Cpv::parse(s).unwrap()
    }

    fn atom(s: &str) -> ConfigAtom {
        ConfigAtom::parse(s).unwrap()
    }

    #[test]
    fn parses_wildcards_and_rejects_non_extended_forms() {
        for ok in [
            "*/*",
            "dev-libs/*",
            "*/foo",
            "dev-*/lib*",
            "*/*::gentoo",
            "dev-libs/*:2",
            "=dev-libs/foo-*12*",
        ] {
            assert!(
                matches!(atom(ok), ConfigAtom::Extended(_)),
                "{ok} should be extended"
            );
            assert_eq!(atom(ok).to_string(), ok);
        }
        assert!(matches!(atom("dev-libs/foo"), ConfigAtom::Dep(_)));
        for bad in [
            "**/*",
            "*/foo-1.0",
            "=dev-libs/foo-*1.2*",
            "dev-libs/*[ssl]",
            "*",
        ] {
            assert!(ConfigAtom::parse(bad).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn wildcard_matching() {
        let foo = cpv("dev-libs/foo-1.12.3");
        let gentoo = Some(Interned::intern("gentoo"));
        let other = Some(Interned::intern("other"));
        assert!(atom("*/*").matches(&foo, None, gentoo));
        assert!(atom("dev-libs/*").matches(&foo, None, None));
        assert!(!atom("dev-libs/*").matches(&cpv("sys-libs/foo-1"), None, None));
        assert!(atom("*/foo").matches(&foo, None, None));
        assert!(!atom("*/foo").matches(&cpv("dev-libs/foobar-1"), None, None));
        assert!(atom("dev-*/f*o").matches(&foo, None, None));
        assert!(atom("=dev-libs/foo-*12*").matches(&foo, None, None));
        assert!(!atom("=dev-libs/foo-*13*").matches(&foo, None, None));
        assert!(atom("*/*::gentoo").matches(&foo, None, gentoo));
        assert!(!atom("*/*::gentoo").matches(&foo, None, other));
        assert!(atom("*/*:2").matches(&foo, Some(&Slot::new("2")), None));
        assert!(!atom("*/*:2").matches(&foo, Some(&Slot::new("3")), None));
        assert!(atom("*/*").is_match_all());
        assert!(!atom("*/*::gentoo").is_match_all());
    }

    #[test]
    fn plain_atom_repo_is_checked_when_known() {
        let foo = cpv("dev-libs/foo-1");
        let a = atom("dev-libs/foo::overlay");
        assert!(a.matches(&foo, None, Some(Interned::intern("overlay"))));
        assert!(!a.matches(&foo, None, Some(Interned::intern("gentoo"))));
        assert!(a.matches(&foo, None, None));
    }

    #[test]
    fn ordering_applies_the_most_specific_atom_last() {
        let entries = vec![
            (atom("=dev-libs/foo-1"), "exact"),
            (atom("dev-libs/foo"), "bare"),
            (atom("dev-libs/foo:1"), "slot"),
            (atom("*/*"), "all"),
            (atom("dev-libs/*"), "cat"),
            (atom("dev-libs/foo"), "bare2"),
            (atom(">=dev-libs/foo-1"), "range"),
        ];
        let order: Vec<_> = order_by_specificity(entries)
            .into_iter()
            .map(|(_, v)| v)
            .collect();
        // Equal-rank `*/*` and `dev-libs/*`: the earlier line applies last, as in Portage.
        assert_eq!(
            order,
            ["cat", "all", "bare", "bare2", "range", "slot", "exact"]
        );
    }
}
