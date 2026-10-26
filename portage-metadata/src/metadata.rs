use std::collections::HashSet;

use crate::interner::{DefaultInterner, Interned, Interner};
use portage_atom::{DepEntry, LazyDepList, Slot};

use crate::eapi::Eapi;
use crate::iuse::IUse;
use crate::keyword::Keyword;
use crate::license::LicenseExpr;
use crate::phase::Phase;
use crate::required_use::RequiredUseExpr;
use crate::restrict::RestrictExpr;
use crate::src_uri::{LazySrcUriList, SrcUriEntry};

/// Metadata for a single ebuild, as produced by the metadata cache
///
/// Contains all the PMS-defined metadata variables that a package manager
/// extracts from an ebuild. Mandatory fields (`eapi`, `description`, `slot`)
/// are always present; optional fields use `Option` or `Vec`.
///
/// See [PMS 7.2](https://projects.gentoo.org/pms/9/pms.html#mandatory-ebuilddefined-variables).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EbuildMetadata<I = DefaultInterner>
where
    I: Interner,
{
    /// EAPI version
    ///
    /// See [PMS 7.3.1](https://projects.gentoo.org/pms/9/pms.html#eapi).
    pub eapi: Eapi,

    /// Package description (mandatory)
    ///
    /// See [PMS 7.2](https://projects.gentoo.org/pms/9/pms.html#mandatory-ebuilddefined-variables).
    pub description: String,

    /// Package slot (mandatory)
    ///
    /// See [PMS 7.2](https://projects.gentoo.org/pms/9/pms.html#mandatory-ebuilddefined-variables).
    pub slot: Slot,

    /// Homepage URL(s)
    pub homepage: Vec<String>,

    /// Source URI expression
    ///
    /// A [`LazySrcUriList`]: only planned/selected packages ever need this
    /// parsed (download-size accounting, `em search`), not the whole tree.
    pub src_uri: LazySrcUriList,

    /// License expression
    pub license: Option<LicenseExpr>,

    /// Architecture keywords
    pub keywords: Vec<Keyword<I>>,

    /// USE flags declared by the ebuild
    pub iuse: Vec<IUse<I>>,

    /// REQUIRED_USE expression (EAPI 4+)
    pub required_use: Option<RequiredUseExpr>,

    /// RESTRICT entries
    pub restrict: Vec<RestrictExpr>,

    /// PROPERTIES entries
    pub properties: Vec<RestrictExpr>,

    /// Build-time dependencies (`DEPEND`)
    ///
    /// A [`LazyDepList`]: only a small fraction of a repo's ebuilds ever
    /// have their dependencies examined by a resolve's solver, so the raw
    /// text is parsed on first access rather than eagerly for every ebuild
    /// `load_repos()` reads.
    ///
    /// See [PMS 8.1](https://projects.gentoo.org/pms/9/pms.html#dependency-classes).
    pub depend: LazyDepList,

    /// Runtime dependencies (`RDEPEND`)
    pub rdepend: LazyDepList,

    /// Build-host dependencies (`BDEPEND`, EAPI 7+)
    pub bdepend: LazyDepList,

    /// Post-merge dependencies (`PDEPEND`)
    pub pdepend: LazyDepList,

    /// Install-time dependencies (`IDEPEND`, EAPI 8)
    pub idepend: LazyDepList,

    /// Eclasses directly listed in the ebuild's `inherit` statement
    ///
    /// Stored as `INHERIT=` in the md5-dict cache format.  This is a portage
    /// auxdb extension; it is not specified by PMS.
    ///
    /// See [PMS 10.1](https://projects.gentoo.org/pms/latest/pms.html#the-inherit-command).
    pub inherit: Vec<String>,

    /// All transitively inherited eclass names (direct + nested)
    ///
    /// Corresponds to the [`INHERITED`](https://projects.gentoo.org/pms/latest/pms.html#magic-ebuild-defined-variables)
    /// ebuild variable (PMS 7.4).  In the md5-dict cache format (PMS 14.3)
    /// this key is excluded; the names are derived from `_eclasses_` instead.
    ///
    /// See [PMS 10.1](https://projects.gentoo.org/pms/latest/pms.html#the-inherit-command)
    /// and [PMS 14.3](https://projects.gentoo.org/pms/latest/pms.html#md5-dict-cache-file-format).
    ///
    /// Interned, sharing keys with [`crate::cache::CacheEntry::eclasses`] —
    /// see that field's doc for why.
    pub inherited: Vec<Interned<I>>,

    /// Defined phase functions
    pub defined_phases: Vec<Phase>,

    /// Values that failed to parse; portage masks such a package instead of hiding it
    ///
    /// An unsupported `EAPI` is recorded here too, with every other field left empty.
    pub invalid: Vec<InvalidField>,
}

/// A metadata value that failed validation, kept verbatim so a cache write round-trips it
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidField {
    /// The metadata key, e.g. `LICENSE`
    pub key: String,
    /// The value as read
    pub raw: String,
    /// Why it was rejected
    pub message: String,
}

impl<I: Interner> EbuildMetadata<I> {
    /// The raw `EAPI` when this package manager does not support it
    pub fn unsupported_eapi(&self) -> Option<&str> {
        self.invalid
            .iter()
            .find(|f| f.key == "EAPI")
            .map(|f| f.raw.as_str())
    }

    /// Every USE flag a `flag?` conditional names, with its key, in the dependency classes,
    /// `SRC_URI`, `LICENSE`, `PROPERTIES` and `RESTRICT`, plus every flag in `REQUIRED_USE`
    ///
    /// Forces the lazy fields' parse.
    pub fn conditional_flags(&self) -> Vec<(&'static str, &str)> {
        fn deps<'a>(
            key: &'static str,
            list: &'a [DepEntry],
            out: &mut Vec<(&'static str, &'a str)>,
        ) {
            for e in list {
                match e {
                    DepEntry::Atom(_) => {}
                    DepEntry::UseConditional { flag, children, .. } => {
                        out.push((key, flag.as_str()));
                        deps(key, children, out);
                    }
                    DepEntry::AllOf(c)
                    | DepEntry::AnyOf(c)
                    | DepEntry::ExactlyOneOf(c)
                    | DepEntry::AtMostOneOf(c) => deps(key, c, out),
                }
            }
        }
        fn src_uri<'a>(list: &'a [SrcUriEntry], out: &mut Vec<(&'static str, &'a str)>) {
            for e in list {
                match e {
                    SrcUriEntry::UseConditional { flag, entries, .. } => {
                        out.push(("SRC_URI", flag));
                        src_uri(entries, out);
                    }
                    SrcUriEntry::Group(entries) => src_uri(entries, out),
                    SrcUriEntry::Uri { .. } | SrcUriEntry::Renamed { .. } => {}
                }
            }
        }
        fn license<'a>(e: &'a LicenseExpr, out: &mut Vec<(&'static str, &'a str)>) {
            match e {
                LicenseExpr::License(_) => {}
                LicenseExpr::UseConditional { flag, entries, .. } => {
                    out.push(("LICENSE", flag));
                    entries.iter().for_each(|e| license(e, out));
                }
                LicenseExpr::AnyOf(entries) | LicenseExpr::All(entries) => {
                    entries.iter().for_each(|e| license(e, out));
                }
            }
        }
        fn restrict<'a>(
            key: &'static str,
            list: &'a [RestrictExpr],
            out: &mut Vec<(&'static str, &'a str)>,
        ) {
            for e in list {
                if let RestrictExpr::UseConditional { flag, entries, .. } = e {
                    out.push((key, flag));
                    restrict(key, entries, out);
                }
            }
        }
        fn required<'a>(e: &'a RequiredUseExpr, out: &mut Vec<(&'static str, &'a str)>) {
            match e {
                RequiredUseExpr::Flag { name, .. } => out.push(("REQUIRED_USE", name)),
                RequiredUseExpr::UseConditional { flag, entries, .. } => {
                    out.push(("REQUIRED_USE", flag));
                    entries.iter().for_each(|e| required(e, out));
                }
                RequiredUseExpr::AnyOf(entries)
                | RequiredUseExpr::ExactlyOne(entries)
                | RequiredUseExpr::AtMostOne(entries)
                | RequiredUseExpr::All(entries) => entries.iter().for_each(|e| required(e, out)),
            }
        }

        let mut out = Vec::new();
        deps("DEPEND", self.depend.list(), &mut out);
        deps("RDEPEND", self.rdepend.list(), &mut out);
        deps("BDEPEND", self.bdepend.list(), &mut out);
        deps("PDEPEND", self.pdepend.list(), &mut out);
        deps("IDEPEND", self.idepend.list(), &mut out);
        src_uri(self.src_uri.list(), &mut out);
        if let Some(l) = &self.license {
            license(l, &mut out);
        }
        restrict("PROPERTIES", &self.properties, &mut out);
        restrict("RESTRICT", &self.restrict, &mut out);
        if let Some(r) = &self.required_use {
            required(r, &mut out);
        }
        out
    }
}

impl<I: Interner + Clone> EbuildMetadata<I> {
    /// Whether this ebuild is *live* — tracks upstream HEAD rather than a
    /// release
    ///
    /// Gentoo's convention: a `PROPERTIES="live"` token (git-r3 et al), or
    /// the caller checks the `*9999` version-shape separately. Only
    /// unconditional tokens count; `flag? ( live )` means live only under
    /// that flag and reads as not-live here.
    pub fn is_live(&self) -> bool {
        fn unconditional_live(entries: &[RestrictExpr]) -> bool {
            entries
                .iter()
                .any(|e| matches!(e, RestrictExpr::Token(t) if t == "live"))
        }
        unconditional_live(&self.properties)
    }

    /// Whether any DEPEND-class field or `SRC_URI` fell back to empty
    /// because its raw text failed to parse, rather than genuinely having
    /// no content, or a DEPEND-class atom breaks the EAPI ([`Self::eapi_violation`])
    ///
    /// Forces the parse of every lazy field to answer — only meant for
    /// callers already paying that cost (writing a cache entry back out,
    /// or re-validating an already-suspect entry), never the bulk
    /// resolve-time read path this laziness exists for.
    pub fn has_parse_failure(&self) -> bool {
        self.depend.list();
        self.rdepend.list();
        self.bdepend.list();
        self.pdepend.list();
        self.idepend.list();
        self.src_uri.list();
        self.depend.parse_failed()
            || self.rdepend.parse_failed()
            || self.bdepend.parse_failed()
            || self.pdepend.parse_failed()
            || self.idepend.parse_failed()
            || self.src_uri.parse_failed()
            || self.eapi_violation().is_some()
    }

    /// The first DEPEND-class atom that uses syntax this ebuild's EAPI lacks, as `"NAME: reason"`
    ///
    /// Portage masks such an ebuild as invalid metadata, so [`Self::has_parse_failure`] counts it.
    pub fn eapi_violation(&self) -> Option<String> {
        let fields = [
            ("DEPEND", &self.depend),
            ("RDEPEND", &self.rdepend),
            ("BDEPEND", &self.bdepend),
            ("PDEPEND", &self.pdepend),
            ("IDEPEND", &self.idepend),
        ];
        for (name, field) in fields {
            let mut found = None;
            DepEntry::walk_atoms(field.list(), &mut |dep| {
                if found.is_none()
                    && let Some(reason) = self.eapi.atom_violation(dep)
                {
                    found = Some(format!(
                        "{name}: {reason} are not allowed in EAPI {}: '{dep}'",
                        self.eapi
                    ));
                }
            });
            if found.is_some() {
                return found;
            }
        }
        None
    }

    /// Which lazy fields failed to parse and why, `"NAME: message"` per
    /// field joined with `"; "` — for a caller that already knows
    /// [`Self::has_parse_failure`] is `true` and wants a real diagnostic
    /// instead of a generic message. Forces the same six fields
    /// `has_parse_failure` does (already memoized if that ran first), and
    /// includes [`Self::eapi_violation`].
    pub fn parse_failure_summary(&self) -> String {
        let mut parts = Vec::new();
        macro_rules! check {
            ($name:literal, $field:expr) => {{
                $field.list();
                if $field.parse_failed() {
                    parts.push(format!(
                        "{}: {}",
                        $name,
                        $field.parse_error().unwrap_or("unknown parse error")
                    ));
                }
            }};
        }
        check!("DEPEND", self.depend);
        check!("RDEPEND", self.rdepend);
        check!("BDEPEND", self.bdepend);
        check!("PDEPEND", self.pdepend);
        check!("IDEPEND", self.idepend);
        check!("SRC_URI", self.src_uri);
        parts.extend(self.eapi_violation());
        parts.join("; ")
    }

    /// Return a copy with duplicate top-level dep entries removed (first occurrence wins)
    ///
    /// Portage and portage-repo accumulate eclass contributions by appending
    /// `E_*` values after sourcing, while the ebuild may already have expanded
    /// the same eclass variable inline (e.g. `REQUIRED_USE="${PYTHON_REQUIRED_USE}
    /// ..."`). The result is that the same constraint appears twice. pkgcraft
    /// deduplicates during its own regen; this method normalises to that form.
    pub fn dedup(&self) -> Self {
        let mut result = self.clone();
        dedup_dep(result.depend.make_mut());
        dedup_dep(result.rdepend.make_mut());
        dedup_dep(result.bdepend.make_mut());
        dedup_dep(result.pdepend.make_mut());
        dedup_dep(result.idepend.make_mut());
        if let Some(ref ru) = self.required_use {
            result.required_use = Some(ru.dedup());
        }
        if let Some(ref lic) = self.license {
            result.license = Some(lic.dedup());
        }
        result
    }
}

fn dedup_dep(entries: &mut Vec<DepEntry>) {
    let mut seen: HashSet<DepEntry> = HashSet::new();
    entries.retain(|e| seen.insert(e.clone()));
    for entry in entries.iter_mut() {
        match entry {
            DepEntry::UseConditional { children, .. } => dedup_dep(children),
            DepEntry::AllOf(children) => dedup_dep(children),
            DepEntry::AnyOf(children) => dedup_dep(children),
            DepEntry::ExactlyOneOf(children) => dedup_dep(children),
            DepEntry::AtMostOneOf(children) => dedup_dep(children),
            DepEntry::Atom(_) => {}
        }
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;
    use crate::cache::CacheEntry;

    fn cache_with_properties(properties: &str) -> EbuildMetadata {
        CacheEntry::parse(&format!(
            "EAPI=8\nSLOT=0\nPROPERTIES={properties}\nDESCRIPTION=t\n_md5_=x\n"
        ))
        .unwrap()
        .metadata
    }

    #[test]
    fn is_live_detects_unconditional_property() {
        assert!(cache_with_properties("live").is_live());
        assert!(cache_with_properties("mirror live").is_live());
    }

    #[test]
    fn is_live_ignores_conditional_and_absent_properties() {
        assert!(!cache_with_properties("live? ( live )").is_live());
        assert!(!cache_with_properties("").is_live());
    }
}
