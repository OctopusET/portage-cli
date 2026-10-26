use std::fmt;

use portage_atom::{Operator, Revision, Suffix, SuffixKind, Version};
use pubgrub::VersionSet;
use version_ranges::Ranges;

/// A PubGrub `VersionSet` backed by PMS version ranges
///
/// Maps PMS dependency operators (`>=`, `~`, `=*`, etc.) to `Ranges<Version>`,
/// enabling set algebra (intersection, complement, union) on version constraints.
///
/// See [PMS 8.3.1](https://projects.gentoo.org/pms/9/pms.html#version_specs).
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct PortageVersionSet(Ranges<Version>);

impl PortageVersionSet {
    /// Create a version set matching all versions
    pub fn any() -> Self {
        Self(Ranges::full())
    }

    /// Returns `true` if this set matches all versions
    pub fn is_full(&self) -> bool {
        self.0 == Ranges::full()
    }

    /// Returns the inner ranges
    pub fn ranges(&self) -> &Ranges<Version> {
        &self.0
    }

    /// Returns `true` if the given version is in this set
    pub fn contains(&self, v: &Version) -> bool {
        use pubgrub::VersionSet;
        <Self as VersionSet>::contains(self, v)
    }

    /// Convert a PMS operator, glob flag, and version to a version set
    ///
    /// See [PMS 8.3.1](https://projects.gentoo.org/pms/9/pms.html#version_specs).
    pub fn from_operator(op: Operator, glob: bool, v: Version) -> Self {
        match op {
            Operator::GreaterOrEqual => Self(Ranges::higher_than(v)),
            Operator::Greater => Self(Ranges::strictly_higher_than(v)),
            Operator::LessOrEqual => Self(Ranges::lower_than(v)),
            Operator::Less => Self(Ranges::strictly_lower_than(v)),
            Operator::Equal if glob => Self(glob_ranges(v)),
            Operator::Equal => Self(Ranges::singleton(v)),
            Operator::Approximate => {
                let (base, bumped) = approximate_bounds(&v);
                Self(Ranges::between(base, bumped))
            }
        }
    }
}

impl VersionSet for PortageVersionSet {
    type V = Version;

    fn empty() -> Self {
        Self(Ranges::empty())
    }

    fn singleton(v: Self::V) -> Self {
        Self(Ranges::singleton(v))
    }

    fn complement(&self) -> Self {
        Self(self.0.complement())
    }

    fn intersection(&self, other: &Self) -> Self {
        Self(self.0.intersection(&other.0))
    }

    fn contains(&self, v: &Self::V) -> bool {
        self.0.contains(v)
    }

    fn full() -> Self {
        Self(Ranges::full())
    }

    fn union(&self, other: &Self) -> Self {
        Self(self.0.union(&other.0))
    }

    fn is_disjoint(&self, other: &Self) -> bool {
        self.0.is_disjoint(&other.0)
    }

    fn subset_of(&self, other: &Self) -> bool {
        self.0.subset_of(&other.0)
    }
}

impl fmt::Display for PortageVersionSet {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Compute the lower and upper bounds for the `~` (approximate) operator
///
/// PMS 8.3.1: "~ V" matches versions identical to V when revision is ignored.
///
/// PMS version ordering (Algorithm 3.1): numeric → letter → suffixes → revision.
/// Suffix ordering: _alpha < _beta < _pre < _rc < (none) < _p.
///
/// So `_p` sorts after all revisions of the base version:
/// `1.2.3 < 1.2.3-r1 < ... < 1.2.3_p < 1.2.3_p-r1 < ... < 1.2.4`
///
/// The range `[V_rev0, V_p)` captures exactly the base version and all its
/// revisions, excluding `_p` patchlevels and other suffix variants.
///
/// For suffixed versions the same trick works by appending `_p`:
/// `~1.2.3_alpha1` → `[1.2.3_alpha1, 1.2.3_alpha1_p)` — the appended `_p`
/// sorts after all revisions of `1.2.3_alpha1` but before any version whose
/// suffixes differ from `_alpha1`.
fn approximate_bounds(v: &Version) -> (Version, Version) {
    let mut base = v.clone();
    base.revision = Revision::default();

    let mut upper = base.clone();
    upper.revision = Revision::default();
    upper.suffixes.push(Suffix {
        kind: SuffixKind::P,
        version: None,
    });

    (base, upper)
}

/// `=V*` as a range agreeing with [`Version::glob_matches`]
///
/// PMS order has no least version above a prefix (`1_alpha_alpha` < `1_alpha`), so each bound
/// is taken one `_alpha` below its version ([`alpha_below`]); deeper `_alpha` chains fall outside.
/// Leading zeros of a later component also leak: `=1.09*` admits `1.095`.
fn glob_ranges(v: Version) -> Ranges<Version> {
    if v.revision.0 != 0 {
        return Ranges::singleton(v);
    }
    let upper = glob_upper(&v);
    Ranges::between(alpha_below(v), upper)
}

fn alpha_below(mut v: Version) -> Version {
    v.raw = None;
    v.suffixes.push(Suffix {
        kind: SuffixKind::Alpha,
        version: None,
    });
    v
}

/// The first version past every glob match of `v`
fn glob_upper(v: &Version) -> Version {
    let mut next = v.clone();
    next.raw = None;
    let Some(last) = next.suffixes.last_mut() else {
        if next.letter.is_some() {
            return suffix_family_upper(next);
        }
        let i = next.numbers.len() - 1;
        next.numbers[i] += 1;
        let width = next.digits[i].len();
        next.digits[i] = format!("{:0width$}", next.numbers[i]).into();
        return alpha_below(next);
    };
    let next_kind = match (last.kind, last.version) {
        (_, Some(n)) => {
            last.version = Some(n + 1);
            return alpha_below(next);
        }
        (SuffixKind::P, None) => {
            next.suffixes.pop();
            return suffix_family_upper(next);
        }
        (SuffixKind::Rc, None) => {
            next.suffixes.pop();
            return next;
        }
        (SuffixKind::Alpha, None) => SuffixKind::Beta,
        (SuffixKind::Beta, None) => SuffixKind::Pre,
        (SuffixKind::Pre, None) => SuffixKind::Rc,
    };
    last.kind = next_kind;
    alpha_below(next)
}

/// The first version past `v` with any further suffixes
fn suffix_family_upper(mut v: Version) -> Version {
    if let Some(last) = v.suffixes.last_mut() {
        last.version = Some(last.version.unwrap_or(0) + 1);
    } else {
        match v.letter {
            None => v.letter = Some('a'),
            Some('z') => {
                v.letter = None;
                v.numbers.push(0);
                v.digits.push("0".into());
            }
            Some(c) => v.letter = char::from_u32(c as u32 + 1),
        }
    }
    alpha_below(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn greater_or_equal() {
        let v = Version::parse("1.2.3").unwrap();
        let vs = PortageVersionSet::from_operator(Operator::GreaterOrEqual, false, v.clone());
        assert!(vs.contains(&Version::parse("1.2.3").unwrap()));
        assert!(vs.contains(&Version::parse("2.0.0").unwrap()));
        assert!(!vs.contains(&Version::parse("1.2.2").unwrap()));
    }

    #[test]
    fn less() {
        let v = Version::parse("1.2.3").unwrap();
        let vs = PortageVersionSet::from_operator(Operator::Less, false, v);
        assert!(!vs.contains(&Version::parse("1.2.3").unwrap()));
        assert!(vs.contains(&Version::parse("1.2.2").unwrap()));
        assert!(vs.contains(&Version::parse("0.9.9").unwrap()));
    }

    #[test]
    fn exact() {
        let v = Version::parse("1.2.3").unwrap();
        let vs = PortageVersionSet::from_operator(Operator::Equal, false, v.clone());
        assert!(vs.contains(&v));
        assert!(!vs.contains(&Version::parse("1.2.4").unwrap()));
        assert!(!vs.contains(&Version::parse("1.2.2").unwrap()));
    }

    #[test]
    fn glob_agrees_with_glob_matches() {
        let candidates = [
            "1",
            "1.1.9",
            "1.2_alpha",
            "1.2_alpha2",
            "1.2_beta",
            "1.2_beta3_p1",
            "1.2_pre",
            "1.2_rc",
            "1.2_rc1",
            "1.2_rc1_p2",
            "1.2_rc1-r3",
            "1.2_rc2",
            "1.2_rc10",
            "1.2",
            "1.2-r1",
            "1.2-r2",
            "1.2-r10",
            "1.2_p",
            "1.2_p3",
            "1.2_p3_rc",
            "1.2a",
            "1.2a_rc1",
            "1.2b",
            "1.2z",
            "1.2z_p1",
            "1.2.0",
            "1.2.0_alpha",
            "1.2.5",
            "1.20",
            "1.3_alpha",
            "1.3",
            "2",
            "2_rc1",
            "20",
            "02.1",
            "2023.01",
            "2023.01.4",
            "2023.02",
            "2023.1",
        ];
        let patterns = [
            "1.2",
            "1.2_rc",
            "1.2_rc1",
            "1.2-r1",
            "1.2_p",
            "1.2_p3",
            "1.2_alpha",
            "1.2_beta",
            "1.2a",
            "1.2z",
            "2",
            "1",
            "2023.01",
        ];
        for pat in patterns {
            let pattern = Version::parse(pat).unwrap();
            let vs = PortageVersionSet::from_operator(Operator::Equal, true, pattern.clone());
            for cand in candidates {
                let c = Version::parse(cand).unwrap();
                assert_eq!(
                    vs.contains(&c),
                    c.glob_matches(&pattern),
                    "{cand} against ={pat}*"
                );
            }
        }
    }

    #[test]
    fn approximate() {
        let v = Version::parse("1.2.3-r5").unwrap();
        let vs = PortageVersionSet::from_operator(Operator::Approximate, false, v);
        assert!(vs.contains(&Version::parse("1.2.3").unwrap()));
        assert!(vs.contains(&Version::parse("1.2.3-r0").unwrap()));
        assert!(vs.contains(&Version::parse("1.2.3-r5").unwrap()));
        assert!(!vs.contains(&Version::parse("1.2.4").unwrap()));
        assert!(!vs.contains(&Version::parse("1.2.2").unwrap()));
        assert!(!vs.contains(&Version::parse("1.2.3_p").unwrap()));
        assert!(!vs.contains(&Version::parse("1.2.3_p1").unwrap()));
        assert!(!vs.contains(&Version::parse("1.2.3_alpha").unwrap()));
    }

    #[test]
    fn approximate_suffixed() {
        let v = Version::parse("1.2.3_alpha1").unwrap();
        let vs = PortageVersionSet::from_operator(Operator::Approximate, false, v);
        assert!(vs.contains(&Version::parse("1.2.3_alpha1").unwrap()));
        assert!(vs.contains(&Version::parse("1.2.3_alpha1-r0").unwrap()));
        assert!(vs.contains(&Version::parse("1.2.3_alpha1-r5").unwrap()));
        assert!(!vs.contains(&Version::parse("1.2.3").unwrap()));
        assert!(!vs.contains(&Version::parse("1.2.3-r1").unwrap()));
        assert!(!vs.contains(&Version::parse("1.2.3_alpha2").unwrap()));
        assert!(!vs.contains(&Version::parse("1.2.3_beta1").unwrap()));
        assert!(!vs.contains(&Version::parse("1.2.3_p1").unwrap()));
        assert!(!vs.contains(&Version::parse("1.2.4").unwrap()));
    }

    #[test]
    fn approximate_multi_suffix() {
        let v = Version::parse("1.2.3_pre1_p2").unwrap();
        let vs = PortageVersionSet::from_operator(Operator::Approximate, false, v);
        assert!(vs.contains(&Version::parse("1.2.3_pre1_p2").unwrap()));
        assert!(vs.contains(&Version::parse("1.2.3_pre1_p2-r1").unwrap()));
        assert!(!vs.contains(&Version::parse("1.2.3_pre1_p3").unwrap()));
        assert!(!vs.contains(&Version::parse("1.2.3").unwrap()));
        assert!(!vs.contains(&Version::parse("1.2.4").unwrap()));
    }

    #[test]
    fn intersection() {
        let v1 = PortageVersionSet::from_operator(
            Operator::GreaterOrEqual,
            false,
            Version::parse("1.0").unwrap(),
        );
        let v2 =
            PortageVersionSet::from_operator(Operator::Less, false, Version::parse("2.0").unwrap());
        let inter = v1.intersection(&v2);
        assert!(inter.contains(&Version::parse("1.5").unwrap()));
        assert!(!inter.contains(&Version::parse("0.9").unwrap()));
        assert!(!inter.contains(&Version::parse("2.0").unwrap()));
    }

    #[test]
    fn complement() {
        let vs = PortageVersionSet::from_operator(
            Operator::GreaterOrEqual,
            false,
            Version::parse("2.0").unwrap(),
        );
        let comp = vs.complement();
        assert!(comp.contains(&Version::parse("1.9").unwrap()));
        assert!(!comp.contains(&Version::parse("2.0").unwrap()));
    }

    #[test]
    fn strictly_greater() {
        let vs = PortageVersionSet::from_operator(
            Operator::Greater,
            false,
            Version::parse("3.0.0").unwrap(),
        );
        assert!(!vs.contains(&Version::parse("3.0.0").unwrap()));
        assert!(vs.contains(&Version::parse("3.0.1").unwrap()));
        assert!(!vs.contains(&Version::parse("2.9.9").unwrap()));
    }

    #[test]
    fn less_or_equal() {
        let vs = PortageVersionSet::from_operator(
            Operator::LessOrEqual,
            false,
            Version::parse("2.0.0").unwrap(),
        );
        assert!(vs.contains(&Version::parse("2.0.0").unwrap()));
        assert!(vs.contains(&Version::parse("1.9.9").unwrap()));
        assert!(!vs.contains(&Version::parse("2.0.1").unwrap()));
    }

    #[test]
    fn union_covers_both_ranges() {
        let a = PortageVersionSet::from_operator(
            Operator::GreaterOrEqual,
            false,
            Version::parse("2.0").unwrap(),
        );
        let b = PortageVersionSet::from_operator(
            Operator::LessOrEqual,
            false,
            Version::parse("1.0").unwrap(),
        );
        let u = a.union(&b);
        assert!(u.contains(&Version::parse("0.5").unwrap()));
        assert!(u.contains(&Version::parse("2.5").unwrap()));
        assert!(!u.contains(&Version::parse("1.5").unwrap()));
    }

    #[test]
    fn subset_of_wider_range() {
        let narrow = PortageVersionSet::from_operator(
            Operator::GreaterOrEqual,
            false,
            Version::parse("2.0").unwrap(),
        );
        let wide = PortageVersionSet::from_operator(
            Operator::GreaterOrEqual,
            false,
            Version::parse("1.0").unwrap(),
        );
        assert!(narrow.subset_of(&wide));
        assert!(!wide.subset_of(&narrow));
    }

    #[test]
    fn is_disjoint_non_overlapping() {
        let a = PortageVersionSet::from_operator(
            Operator::GreaterOrEqual,
            false,
            Version::parse("2.0").unwrap(),
        );
        let b =
            PortageVersionSet::from_operator(Operator::Less, false, Version::parse("1.0").unwrap());
        assert!(a.is_disjoint(&b));
    }

    #[test]
    fn is_disjoint_overlapping() {
        let a = PortageVersionSet::from_operator(
            Operator::GreaterOrEqual,
            false,
            Version::parse("1.0").unwrap(),
        );
        let b = PortageVersionSet::from_operator(
            Operator::LessOrEqual,
            false,
            Version::parse("2.0").unwrap(),
        );
        assert!(!a.is_disjoint(&b));
    }

    #[test]
    fn any_is_full() {
        let vs = PortageVersionSet::any();
        assert!(vs.is_full());
    }

    #[test]
    fn constrained_is_not_full() {
        let vs = PortageVersionSet::from_operator(
            Operator::GreaterOrEqual,
            false,
            Version::parse("1.0").unwrap(),
        );
        assert!(!vs.is_full());
    }

    #[test]
    fn display_does_not_panic() {
        let vs = PortageVersionSet::from_operator(
            Operator::GreaterOrEqual,
            false,
            Version::parse("1.0").unwrap(),
        );
        let s = vs.to_string();
        assert!(!s.is_empty());
    }
}
