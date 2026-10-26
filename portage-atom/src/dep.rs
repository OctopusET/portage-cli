use std::fmt;
use std::str::FromStr;

use gentoo_interner::{DefaultInterner, Interned};
use winnow::combinator::{alt, cut_err, opt, preceded};
use winnow::error::StrContext;
use winnow::prelude::*;

use crate::cpn::{Cpn, parse_cpn};
use crate::cpv::{Cpv, parse_cpv_with_glob};
use crate::error::{Error, Result};
use crate::slot::{Slot, SlotDep, parse_slot_dep};
use crate::use_dep::{UseDep, parse_use_deps};
use crate::version::{Operator, Version};

/// Package dependency blocker type
///
/// Blockers prevent conflicting packages from being installed simultaneously.
/// See [PMS 8.3.2](https://projects.gentoo.org/pms/9/pms.html#block-operator).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Blocker {
    /// `!` — weak blocker: the blocked package may be temporarily installed
    /// during a transition, but must be uninstalled before the operation completes
    Weak,
    /// `!!` — strong blocker: the blocked package must never be installed
    /// at the same time as this package
    Strong,
}

impl fmt::Display for Blocker {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Blocker::Weak => write!(f, "!"),
            Blocker::Strong => write!(f, "!!"),
        }
    }
}

/// Full dependency atom
///
/// Represents a complete dependency atom as it appears in ebuild `*DEPEND`
/// variables, e.g. `>=dev-lang/rust-1.75.0:0::gentoo[ssl]`.
///
/// The general form is `[!|!!][op]category/package[-ver][:slot][::repo][use]`.
///
/// See [PMS 8.3](https://projects.gentoo.org/pms/9/pms.html#package-dependency-specifications)
/// for the full specification.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "builder", derive(bon::Builder))]
pub struct Dep {
    /// The unversioned category/package name (e.g. `dev-lang/rust`)
    ///
    /// See [`Cpn`] and [PMS 3.1].
    ///
    /// [PMS 3.1]: https://projects.gentoo.org/pms/9/pms.html#restrictions-upon-names
    #[cfg_attr(feature = "builder", builder(start_fn))]
    pub cpn: Cpn,
    /// Optional blocker prefix
    ///
    /// `!` is a weak blocker; `!!` is a strong blocker.
    /// See [PMS 8.3.2].
    ///
    /// [PMS 8.3.2]: https://projects.gentoo.org/pms/9/pms.html#block-operator
    pub blocker: Option<Blocker>,
    /// Optional version comparison operator
    ///
    /// See [PMS 8.3.1].
    ///
    /// [PMS 8.3.1]: https://projects.gentoo.org/pms/9/pms.html#operators
    pub op: Option<Operator>,
    /// Optional version constraint
    ///
    /// See [PMS 8.3.1].
    ///
    /// [PMS 8.3.1]: https://projects.gentoo.org/pms/9/pms.html#operators
    pub version: Option<Version>,
    /// PMS glob suffix (`*`) for wildcard matching with `=` operator
    ///
    /// Per PMS 8.3.1: "if the version specified has an asterisk immediately
    /// following it, then only the given number of version components is used
    /// for comparison". Only valid with `op = Some(Equal)`.
    #[cfg_attr(feature = "builder", builder(default))]
    pub glob: bool,
    /// Binary package build id (`=cat/pkg-1.0-r1-3`), after any operator's version but never a glob
    ///
    /// Only `=` compares it (see [`Dep::pinned_build`]); no ebuild has one.
    pub build_id: Option<u64>,
    /// Optional slot dependency (the portion after `:`)
    ///
    /// See [PMS 8.3.3].
    ///
    /// [PMS 8.3.3]: https://projects.gentoo.org/pms/9/pms.html#slot-dependencies
    pub slot_dep: Option<SlotDep>,
    /// Optional USE flag constraints (the bracketed portion)
    ///
    /// See [PMS 8.3.4].
    ///
    /// [PMS 8.3.4]: https://projects.gentoo.org/pms/9/pms.html#style-and-style-use-dependencies
    pub use_deps: Option<Vec<UseDep>>,
    /// Optional repository name (after `::`, e.g. `gentoo`)
    ///
    /// See [PMS 8.3.5].
    ///
    /// [PMS 8.3.5]: https://projects.gentoo.org/pms/9/pms.html#repository-dependencies
    #[cfg_attr(feature = "builder", builder(into))]
    pub repo: Option<Interned<DefaultInterner>>,
}

impl Dep {
    /// Whether an installed/available `cpv` (with its `slot`, when known)
    /// satisfies this atom's name, version operator, and named-slot
    /// constraints
    ///
    /// `slot` is the candidate's own [`Slot`], not a raw `SLOT` string: an
    /// installed `0/1` is in slot `0`, so an atom's `:0` must match it.
    /// Passing the unsplit text compares `"0"` against `"0/1"` and matches
    /// nothing, which is the shape of a bug this signature exists to
    /// prevent.
    ///
    /// A caller that knows only the slot name uses [`Slot::from_name`]; its
    /// `None` sub-slot means *unrecorded*, so a sub-slot constraint is only
    /// enforced when both sides carry one.
    ///
    /// USE-dep brackets, blockers, `::repo` and the build id are *not* evaluated here:
    /// this answers the `has_version`-style question "does a matching
    /// version exist", per [PMS 8.3.1]/[8.3.3].
    ///
    /// [PMS 8.3.1]: https://projects.gentoo.org/pms/9/pms.html#operators
    /// [8.3.3]: https://projects.gentoo.org/pms/9/pms.html#slot-deps
    pub fn matches_cpv(&self, cpv: &Cpv, slot: Option<&Slot>) -> bool {
        if self.cpn != cpv.cpn {
            return false;
        }
        if let Some(crate::SlotDep::Slot { slot: Some(s), .. }) = &self.slot_dep
            && let Some(cand) = slot
        {
            if s.slot != cand.slot {
                return false;
            }
            // Only when both sides name one: a candidate built by
            // [`Slot::from_name`] has no sub-slot *recorded*, which is not the
            // same as having none, and must not refute `:0/1`.
            if let (Some(want), Some(have)) = (s.subslot, cand.subslot)
                && want != have
            {
                return false;
            }
        }
        let (Some(op), Some(want)) = (self.op, &self.version) else {
            return self.version.is_none();
        };
        let cand = &cpv.version;
        cand.matches_operator(op, self.glob, want)
    }

    /// Create a minimal dependency from a [`Cpn`]
    ///
    /// All optional fields default to `None`.
    pub fn new(cpn: Cpn) -> Self {
        Dep {
            cpn,
            blocker: None,
            op: None,
            version: None,
            glob: false,
            build_id: None,
            slot_dep: None,
            use_deps: None,
            repo: None,
        }
    }

    /// Parse a full dependency atom string
    ///
    /// Returns an error if the string does not conform to the PMS format.
    pub fn parse(input: &str) -> Result<Self> {
        parse_dep
            .parse(input)
            .map_err(|e| Error::InvalidDep(format!("{}: {}", input, e)))
    }

    /// Try to create from a string
    ///
    /// Alias for [`Dep::parse`].
    pub fn try_new(s: &str) -> Result<Self> {
        Self::parse(s)
    }

    /// The category portion of the atom (e.g. `dev-lang`)
    pub fn category(&self) -> &str {
        &self.cpn.category
    }

    /// The package name portion of the atom (e.g. `rust`)
    pub fn package(&self) -> &str {
        &self.cpn.package
    }

    /// Build a [`Cpv`] from this dependency, if it carries a version
    pub fn cpv(&self) -> Option<Cpv> {
        self.version.as_ref().map(|v| Cpv::new(self.cpn, v.clone()))
    }

    /// The `BUILD_ID` this atom selects: portage compares a build id only under `=`
    pub fn pinned_build(&self) -> Option<u64> {
        self.build_id
            .filter(|_| self.op == Some(crate::Operator::Equal))
    }
}

/// Wire form is the atom string ([`Dep::parse`]/[`Display`](fmt::Display)),
/// never the interned/parsed representation, which is process-local.
#[cfg(feature = "serde")]
impl serde::Serialize for Dep {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for Dep {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let s = <std::borrow::Cow<'de, str> as serde::Deserialize>::deserialize(deserializer)?;
        Self::parse(&s).map_err(serde::de::Error::custom)
    }
}

impl fmt::Display for Dep {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if let Some(blocker) = &self.blocker {
            write!(f, "{}", blocker)?;
        }

        if let Some(op) = self.op {
            write!(f, "{}", op)?;
        }

        write!(f, "{}", self.cpn)?;

        if let Some(version) = &self.version {
            write!(f, "-")?;
            version.fmt_version(f)?;
            if self.glob {
                write!(f, "*")?;
            }
        }

        if let Some(build_id) = self.build_id {
            write!(f, "-{build_id}")?;
        }

        if let Some(slot) = &self.slot_dep {
            write!(f, ":{}", slot)?;
        }

        if let Some(repo) = &self.repo {
            write!(f, "::{}", repo)?;
        }

        if let Some(use_deps) = &self.use_deps
            && !use_deps.is_empty()
        {
            write!(f, "[")?;
            for (i, dep) in use_deps.iter().enumerate() {
                if i > 0 {
                    write!(f, ",")?;
                }
                write!(f, "{}", dep)?;
            }
            write!(f, "]")?;
        }

        Ok(())
    }
}

impl FromStr for Dep {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        Self::parse(s)
    }
}

// Winnow parsers

/// Parse blocker prefix
fn parse_blocker(input: &mut &str) -> ModalResult<Blocker> {
    alt(("!!".value(Blocker::Strong), "!".value(Blocker::Weak))).parse_next(input)
}

/// Parse repository name (alphanumeric, _, -, +)
fn parse_repo(input: &mut &str) -> ModalResult<Interned<DefaultInterner>> {
    use crate::parsers::parse_ident_base;

    parse_ident_base
        .map(|s: &str| Interned::intern(s))
        .parse_next(input)
}

/// `cat/pkg-<ver>-<build id>`, the build id an integer without a leading zero
fn parse_cpv_build_id(input: &mut &str) -> ModalResult<(Cpv, u64)> {
    use crate::cpn::parse_category;
    use crate::parsers::parse_ident_with_dot_star;
    use winnow::combinator::fail;

    let checkpoint = *input;
    let token = (parse_category, '/', parse_ident_with_dot_star)
        .take()
        .parse_next(input)?;
    let parsed = token.rsplit_once('-').and_then(|(cpv, id)| {
        let canonical =
            id.bytes().all(|b| b.is_ascii_digit()) && !(id.len() > 1 && id.starts_with('0'));
        Some((Cpv::parse(cpv).ok()?, canonical.then(|| id.parse().ok())??))
    });
    match parsed {
        Some(parsed) => Ok(parsed),
        None => {
            *input = checkpoint;
            fail.parse_next(input)
        }
    }
}

/// Parse full dependency atom
///
/// Handles: [!|!!][op]cat/pkg[-ver][:slot][::repo][use]
pub(crate) fn parse_dep(input: &mut &str) -> ModalResult<Dep> {
    use crate::version::{Operator, parse_operator};
    use winnow::combinator::fail;

    let blocker = opt(parse_blocker).parse_next(input)?;

    let operator = opt(parse_operator).parse_next(input)?;

    let build_id = if operator.is_some() {
        opt(parse_cpv_build_id).parse_next(input)?
    } else {
        None
    };
    let (cpn, version, glob, build_id) = if let Some((cpv, id)) = build_id {
        (cpv.cpn, Some(cpv.version), false, Some(id))
    } else if operator.is_some() {
        let (cpv, glob) = cut_err(parse_cpv_with_glob)
            .context(StrContext::Label("versioned atom"))
            .parse_next(input)?;
        (cpv.cpn, Some(cpv.version), glob, None)
    } else {
        // A version needs an operator (PMS 8.3.1); without one, `cat/pkg-1` is a package name, and
        // `parse_package` rejects it for ending in a version.
        (parse_cpn.parse_next(input)?, None, false, None)
    };

    if let Some(op) = operator {
        if version.is_none() {
            return fail.parse_next(input);
        }
        if glob && op != Operator::Equal {
            return fail.parse_next(input);
        }
    }

    let slot_dep = opt(preceded(':', parse_slot_dep)).parse_next(input)?;

    let repo = opt(preceded("::", parse_repo)).parse_next(input)?;

    let use_deps = opt(parse_use_deps).parse_next(input)?;

    Ok(Dep {
        cpn,
        blocker,
        op: operator,
        version,
        glob,
        build_id,
        slot_dep,
        use_deps,
        repo,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slot::SlotOperator;
    use crate::version::{Operator, SuffixKind};

    #[test]
    #[cfg(feature = "serde")]
    fn dep_serde_round_trips_through_its_atom_form() {
        for atom in [
            "sys-devel/gcc:16",
            ">=sys-libs/glibc-2.42",
            "=sys-devel/gcc-16.2*",
        ] {
            let dep = Dep::parse(atom).unwrap();
            let json = serde_json::to_string(&dep).unwrap();
            assert_eq!(json, format!("\"{atom}\""));
            assert_eq!(serde_json::from_str::<Dep>(&json).unwrap(), dep);
        }
        assert!(serde_json::from_str::<Dep>("\"not an atom\"").is_err());
    }

    #[test]
    fn test_dep_simple() {
        let dep = Dep::parse("dev-lang/rust").unwrap();
        assert_eq!(dep.category(), "dev-lang");
        assert_eq!(dep.package(), "rust");
        assert!(dep.version.is_none());
        assert!(dep.blocker.is_none());
        assert_eq!(dep.to_string(), "dev-lang/rust");
    }

    #[test]
    fn a_version_without_an_operator_is_not_an_atom() {
        for atom in [
            "virtual/awk-1",
            "sys-libs/zlib-1.3.2-r1",
            "cat/pkg-18446744073709551616",
            "virtual/awk-1[foo]",
        ] {
            assert!(Dep::parse(atom).is_err(), "{atom}");
        }
        assert!(Dep::parse("dev-lang/python-exec").is_ok());
        assert!(Dep::parse("=virtual/awk-1").is_ok());
    }

    #[test]
    fn test_dep_versioned() {
        let dep = Dep::parse(">=dev-lang/rust-1.75.0").unwrap();
        assert!(dep.version.is_some());
        let version = dep.version.as_ref().unwrap();
        assert_eq!(version.numbers[0], 1);
        assert_eq!(version.numbers[1], 75);
        assert_eq!(dep.to_string(), ">=dev-lang/rust-1.75.0");
    }

    #[test]
    fn test_dep_with_slot() {
        let dep = Dep::parse("dev-lang/rust:0").unwrap();
        assert!(dep.slot_dep.is_some());
        assert_eq!(dep.to_string(), "dev-lang/rust:0");
    }

    #[test]
    fn test_dep_with_use_deps() {
        let dep = Dep::parse("dev-lang/rust[llvm_targets_AMDGPU]").unwrap();
        assert!(dep.use_deps.is_some());
        let use_deps = dep.use_deps.as_ref().unwrap();
        assert_eq!(use_deps.len(), 1);
        assert_eq!(dep.to_string(), "dev-lang/rust[llvm_targets_AMDGPU]");
    }

    #[test]
    fn test_dep_with_blocker() {
        let dep = Dep::parse("!dev-lang/rust").unwrap();
        assert_eq!(dep.blocker, Some(Blocker::Weak));
        assert_eq!(dep.to_string(), "!dev-lang/rust");

        let dep = Dep::parse("!!dev-lang/rust").unwrap();
        assert_eq!(dep.blocker, Some(Blocker::Strong));
        assert_eq!(dep.to_string(), "!!dev-lang/rust");
    }

    #[test]
    fn test_dep_with_repo() {
        let dep = Dep::parse("dev-lang/rust::gentoo").unwrap();
        assert_eq!(dep.repo, Some(Interned::intern("gentoo")));
        assert_eq!(dep.to_string(), "dev-lang/rust::gentoo");
    }

    #[test]
    fn repo_comes_between_slot_and_use_deps() {
        let dep = Dep::parse("cat/pkg:1::gentoo[foo]").unwrap();
        assert!(
            matches!(&dep.slot_dep, Some(SlotDep::Slot { slot: Some(s), .. }) if s.slot == "1")
        );
        assert_eq!(dep.repo, Some(Interned::intern("gentoo")));
        assert_eq!(dep.use_deps.as_ref().unwrap().len(), 1);
        assert_eq!(dep.to_string(), "cat/pkg:1::gentoo[foo]");
        assert!(Dep::parse("cat/pkg[foo]::gentoo").is_err());
    }

    #[test]
    fn build_id_follows_the_version_of_an_equal_atom() {
        let dep = Dep::parse("=cat/pkg-1.0-r1-3").unwrap();
        assert_eq!(dep.build_id, Some(3));
        assert_eq!(dep.version.as_ref().unwrap().to_string(), "1.0-r1");
        assert_eq!(dep.to_string(), "=cat/pkg-1.0-r1-3");
        let dep = Dep::parse("=app-misc/foo-1-30:0::gentoo").unwrap();
        assert_eq!((dep.package(), dep.build_id), ("foo", Some(30)));
        assert_eq!(dep.to_string(), "=app-misc/foo-1-30:0::gentoo");
        assert_eq!(Dep::parse("=cat/pkg2-3").unwrap().build_id, None);
        assert_eq!(dep.pinned_build(), Some(30));
        for atom in [">=cat/pkg-1.0-3", "~cat/pkg-1.0-3"] {
            let dep = Dep::parse(atom).unwrap();
            assert_eq!(
                (dep.build_id, dep.pinned_build()),
                (Some(3), None),
                "{atom}"
            );
            assert_eq!(dep.version.as_ref().unwrap().to_string(), "1.0");
            assert_eq!(dep.to_string(), atom);
        }
        for atom in ["=cat/pkg-1.0-03", "=cat/pkg-1.0-3*", "cat/pkg-1.0-3"] {
            assert!(Dep::parse(atom).is_err(), "{atom}");
        }
    }

    #[test]
    fn test_dep_complex() {
        let dep = Dep::parse(">=dev-lang/rust-1.75.0:0/1.75::gentoo[llvm_targets_AMDGPU,-debug]")
            .unwrap();
        assert!(dep.version.is_some());
        assert!(dep.slot_dep.is_some());
        assert!(dep.use_deps.is_some());
        assert_eq!(dep.repo, Some(Interned::intern("gentoo")));
    }

    #[test]
    fn test_dep_with_all_features() {
        // Test a complex dependency using all PMS features
        let dep_str = "!!>=dev-lang/rust-1.75.0_rc1:0/1.75=::gentoo[ssl(-),-debug,python?]";
        let dep = Dep::parse(dep_str).unwrap();

        assert_eq!(dep.blocker, Some(Blocker::Strong));
        assert_eq!(dep.op, Some(Operator::GreaterOrEqual));
        assert!(dep.version.is_some());
        let version = dep.version.as_ref().unwrap();
        assert_eq!(version.numbers[0], 1);
        assert_eq!(version.suffixes[0].kind, SuffixKind::Rc);

        assert!(dep.slot_dep.is_some());
        let slot_dep = dep.slot_dep.as_ref().unwrap();
        match slot_dep {
            SlotDep::Slot {
                slot: Some(s),
                op: Some(o),
            } => {
                assert_eq!(s.slot, "0");
                assert_eq!(s.subslot, Some(Interned::intern("1.75")));
                assert_eq!(*o, SlotOperator::Equal);
            }
            _ => panic!("Unexpected slot dep format"),
        }

        assert!(dep.use_deps.is_some());
        let use_deps = dep.use_deps.as_ref().unwrap();
        assert_eq!(use_deps.len(), 3);

        assert_eq!(dep.repo, Some(Interned::intern("gentoo")));
    }

    #[test]
    #[cfg(feature = "builder")]
    fn test_dep_builder_simple() {
        let cpn = Cpn::new("dev-lang", "rust");
        let dep = Dep::builder(cpn).build();
        assert_eq!(dep.category(), "dev-lang");
        assert_eq!(dep.package(), "rust");
        assert!(dep.version.is_none());
        assert!(dep.blocker.is_none());
        assert_eq!(dep.to_string(), "dev-lang/rust");
    }

    #[test]
    #[cfg(feature = "builder")]
    fn test_dep_builder_versioned() {
        let cpn = Cpn::new("dev-lang", "rust");
        let version = Version::new(&[1, 75, 0]);
        let dep = Dep::builder(cpn)
            .op(Operator::GreaterOrEqual)
            .version(version)
            .build();
        assert!(dep.version.is_some());
        assert_eq!(dep.to_string(), ">=dev-lang/rust-1.75.0");
    }

    #[test]
    #[cfg(feature = "builder")]
    fn test_dep_builder_with_blocker_and_repo() {
        let cpn = Cpn::new("dev-libs", "old");
        let dep = Dep::builder(cpn)
            .blocker(Blocker::Strong)
            .repo("gentoo")
            .build();
        assert_eq!(dep.blocker, Some(Blocker::Strong));
        assert_eq!(dep.repo, Some(Interned::intern("gentoo")));
        assert_eq!(dep.to_string(), "!!dev-libs/old::gentoo");
    }

    #[test]
    #[cfg(feature = "builder")]
    fn test_dep_builder_roundtrip() {
        let original = Dep::parse(">=dev-lang/rust-1.75.0:0::gentoo[ssl,-debug]").unwrap();

        let built = Dep::builder(original.cpn)
            .op(Operator::GreaterOrEqual)
            .version(original.version.clone().unwrap())
            .slot_dep(original.slot_dep.clone().unwrap())
            .use_deps(original.use_deps.clone().unwrap())
            .repo("gentoo")
            .build();

        assert_eq!(original.to_string(), built.to_string());
    }

    // --- PMS 8.3.1 operator tests ---

    #[test]
    fn test_all_operators() {
        let cases = [
            ("<dev-libs/a-1.0", Operator::Less),
            ("<=dev-libs/a-1.0", Operator::LessOrEqual),
            ("=dev-libs/a-1.0", Operator::Equal),
            ("~dev-libs/a-1.0", Operator::Approximate),
            (">=dev-libs/a-1.0", Operator::GreaterOrEqual),
            (">dev-libs/a-1.0", Operator::Greater),
        ];
        for (input, expected_op) in cases {
            let dep = Dep::parse(input).unwrap();
            assert_eq!(dep.op, Some(expected_op), "operator mismatch for: {input}");
            assert_eq!(dep.to_string(), input, "round-trip failed for: {input}");
        }
    }

    #[test]
    fn test_approximate_operator() {
        let dep = Dep::parse("~dev-lang/rust-1.75.0").unwrap();
        assert_eq!(dep.op, Some(Operator::Approximate));
        assert_eq!(
            dep.version.as_ref().unwrap().numbers.as_slice(),
            vec![1, 75, 0].as_slice()
        );
    }

    #[test]
    fn test_glob_with_equal_only() {
        // PMS 8.3.1: "An asterisk used with any other operator is illegal"
        assert!(Dep::parse("=dev-libs/a-1*").is_ok());
        assert!(Dep::parse(">=dev-libs/a-1*").is_err());
        assert!(Dep::parse("<dev-libs/a-1*").is_err());
        assert!(Dep::parse(">dev-libs/a-1*").is_err());
        assert!(Dep::parse("<=dev-libs/a-1*").is_err());
        assert!(Dep::parse("~dev-libs/a-1*").is_err());
    }

    #[test]
    fn test_dep_display_round_trip() {
        let inputs = [
            "dev-lang/rust",
            "!dev-libs/old",
            "!!dev-libs/old",
            ">=dev-lang/rust-1.75.0",
            "=dev-libs/a-11*",
            "dev-lang/rust:0",
            "dev-lang/rust:0/1.75",
            "dev-lang/rust[ssl]",
            "dev-lang/rust::gentoo",
            ">=dev-lang/rust-1.75.0:0::gentoo[ssl]",
        ];
        for input in inputs {
            let dep = Dep::parse(input).unwrap();
            assert_eq!(dep.to_string(), input, "round-trip failed for: {input}");
        }
    }

    // H1: slot-equals operator combined with use deps — the exact forms that
    // appear in texlive-core's RDEPEND and caused missing transitive deps.
    #[test]
    fn slot_equals_with_use_deps() {
        // `:=[use1,use2]` — slot equals operator + use constraints
        let dep = Dep::parse(">=media-libs/harfbuzz-1.4.5:=[icu,graphite]").unwrap();
        assert_eq!(dep.op, Some(Operator::GreaterOrEqual));
        match dep.slot_dep.as_ref().unwrap() {
            SlotDep::Operator(SlotOperator::Equal) => {}
            other => panic!("expected SlotDep::Operator(Equal), got {other:?}"),
        }
        let use_deps = dep.use_deps.as_ref().unwrap();
        assert_eq!(use_deps.len(), 2);
        assert_eq!(
            dep.to_string(),
            ">=media-libs/harfbuzz-1.4.5:=[icu,graphite]"
        );
    }

    #[test]
    fn slot_number_equals_operator() {
        // `:0=` — explicit slot + equals operator (libpng style)
        let dep = Dep::parse(">=media-libs/libpng-1.2.43-r2:0=").unwrap();
        assert_eq!(dep.op, Some(Operator::GreaterOrEqual));
        match dep.slot_dep.as_ref().unwrap() {
            SlotDep::Slot {
                slot: Some(s),
                op: Some(SlotOperator::Equal),
            } => assert_eq!(s.slot.as_str(), "0"),
            other => panic!("expected slotted equals, got {other:?}"),
        }
        assert_eq!(dep.to_string(), ">=media-libs/libpng-1.2.43-r2:0=");
    }

    #[test]
    fn slot_equals_alone() {
        // simple `:=` with no use deps (zlib, zziplib, kpathsea style)
        for atom in [
            "virtual/zlib:=",
            "dev-libs/zziplib:=",
            ">=dev-libs/kpathsea-6.4.0:=",
        ] {
            let dep = Dep::parse(atom).unwrap();
            match dep.slot_dep.as_ref().unwrap() {
                SlotDep::Operator(SlotOperator::Equal) => {}
                other => panic!("{atom}: expected SlotDep::Operator(Equal), got {other:?}"),
            }
            assert_eq!(dep.to_string(), atom, "round-trip failed for {atom}");
        }
    }

    #[test]
    fn slot_number_only() {
        // `:2` — specific slot, no operator (freetype style)
        let dep = Dep::parse("media-libs/freetype:2").unwrap();
        match dep.slot_dep.as_ref().unwrap() {
            SlotDep::Slot {
                slot: Some(s),
                op: None,
            } => assert_eq!(s.slot.as_str(), "2"),
            other => panic!("expected slotted no-op, got {other:?}"),
        }
        assert_eq!(dep.to_string(), "media-libs/freetype:2");
    }

    #[test]
    fn use_dep_without_slot() {
        // `pkg[use]` — use constraint, no slot (gd style)
        let dep = Dep::parse("media-libs/gd[png]").unwrap();
        assert!(dep.slot_dep.is_none());
        let use_deps = dep.use_deps.as_ref().unwrap();
        assert_eq!(use_deps.len(), 1);
        assert_eq!(dep.to_string(), "media-libs/gd[png]");
    }

    #[test]
    fn texlive_core_rdepend_atoms_parse() {
        // Every atom form appearing in app-text/texlive-core-2024-r2 RDEPEND.
        // If any of these fail to parse, the entire cache entry is silently dropped
        // and all transitive deps vanish — which is Hypothesis H1.
        let atoms = [
            "sci-libs/mpfi",
            "virtual/zlib:=",
            ">=media-libs/harfbuzz-1.4.5:=[icu,graphite]",
            ">=media-libs/libpng-1.2.43-r2:0=",
            "media-libs/gd[png]",
            "media-gfx/graphite2:=",
            "media-gfx/potrace:=",
            ">=x11-libs/cairo-1.12",
            ">=x11-libs/pixman-0.18",
            "dev-libs/zziplib:=",
            "app-text/libpaper:=",
            "dev-libs/gmp:=",
            "dev-libs/mpfr:=",
            ">=dev-libs/ptexenc-1.4.6",
            "media-libs/freetype:2",
            ">=dev-libs/icu-50:=",
            ">=dev-libs/kpathsea-6.4.0:=",
            "virtual/perl-Getopt-Long",
            "dev-perl/File-HomeDir",
            "dev-perl/Log-Dispatch",
            "dev-perl/Unicode-LineBreak",
            "dev-perl/YAML-Tiny",
            // USE-conditional deps tested at DepEntry level, not Dep level
        ];
        for atom in atoms {
            Dep::parse(atom).unwrap_or_else(|e| panic!("failed to parse '{atom}': {e}"));
        }
    }
}
