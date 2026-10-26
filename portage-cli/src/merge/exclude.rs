//! `--buildpkg-exclude` atoms, in the less strict form emerge accepts for them

use anyhow::{Context, Result, bail};
use portage_atom::{Cpv, Dep, Slot};

/// One `--buildpkg-exclude` word
///
/// A name without a category matches any category. `*` in the category or name is a wildcard,
/// which takes an optional slot but no operator. A `::repo` is accepted and not matched.
#[derive(Debug, Clone)]
pub(crate) enum AtomPattern {
    /// A PMS atom
    Atom(Dep),
    /// A category and name that may hold `*`
    Glob {
        category: String,
        package: String,
        slot: Option<Slot>,
    },
}

impl AtomPattern {
    /// Parse one word, as emerge's `_find_bad_atoms(less_strict=True)` accepts it
    pub(crate) fn parse(word: &str) -> Result<Self> {
        let (without_repo, _) = word.split_once("::").unwrap_or((word, ""));
        let (name, slot) = match without_repo.split_once(':') {
            Some((name, slot)) => (name, Some(slot)),
            None => (without_repo, None),
        };
        let operator = name.starts_with(['<', '>', '=', '~', '!']);
        if operator || (name.contains('/') && !name.contains('*')) {
            let dep = Dep::parse(word).with_context(|| format!("invalid atom '{word}'"))?;
            return Ok(Self::Atom(dep));
        }
        let (category, package) = name.split_once('/').unwrap_or(("*", name));
        let valid = |part: &str| {
            !part.is_empty()
                && !part.starts_with(['-', '.', '+'])
                && part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "+_.-*".contains(c))
        };
        if !valid(category) || !valid(package) {
            bail!("invalid atom '{word}'");
        }
        let slot = slot
            .map(Slot::parse)
            .transpose()
            .with_context(|| format!("invalid slot in '{word}'"))?;
        Ok(Self::Glob {
            category: category.to_owned(),
            package: package.to_owned(),
            slot,
        })
    }

    /// Whether the package `cpv` in `slot` matches; an unknown slot does not refute one
    pub(crate) fn matches(&self, cpv: &Cpv, slot: Option<&Slot>) -> bool {
        match self {
            Self::Atom(dep) => dep.matches_cpv(cpv, slot),
            Self::Glob {
                category,
                package,
                slot: want,
            } => {
                let slot_ok = match (want, slot) {
                    (Some(want), Some(have)) => {
                        want.slot == have.slot
                            && (want.subslot.is_none()
                                || have.subslot.is_none()
                                || want.subslot == have.subslot)
                    }
                    _ => true,
                };
                slot_ok
                    && glob(category, cpv.cpn.category.as_str())
                    && glob(package, cpv.cpn.package.as_str())
            }
        }
    }
}

/// Parse every `--buildpkg-exclude` word, failing on the first invalid one
pub(crate) fn parse_patterns<'a>(
    words: impl IntoIterator<Item = &'a str>,
) -> Result<Vec<AtomPattern>> {
    words
        .into_iter()
        .map(AtomPattern::parse)
        .collect::<Result<_>>()
        .context("--buildpkg-exclude")
}

/// `text` matches `pattern`, where `*` stands for any run of characters
fn glob(pattern: &str, text: &str) -> bool {
    let mut parts = pattern.split('*');
    let first = parts.next().unwrap_or_default();
    let Some(mut rest) = text.strip_prefix(first) else {
        return false;
    };
    let mut parts: Vec<&str> = parts.collect();
    let Some(last) = parts.pop() else {
        return rest.is_empty();
    };
    for part in parts {
        match rest.find(part) {
            Some(at) => rest = &rest[at + part.len()..],
            None => return false,
        }
    }
    rest.len() >= last.len() && rest.ends_with(last)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matches(word: &str, cpv: &str, slot: Option<&str>) -> bool {
        let slot = slot.map(|s| Slot::parse(s).unwrap());
        AtomPattern::parse(word)
            .unwrap()
            .matches(&Cpv::parse(cpv).unwrap(), slot.as_ref())
    }

    #[test]
    fn a_bare_name_matches_any_category() {
        assert!(matches("jinja2", "dev-python/jinja2-3.1.6", None));
        assert!(!matches("jinja2", "dev-python/jinja2-time-0.2", None));
    }

    #[test]
    fn a_wildcard_matches_a_run_of_characters() {
        assert!(matches("dev-python/*", "dev-python/jinja2-3.1.6", None));
        assert!(!matches("dev-python/*", "dev-lang/python-3.12.4", None));
        assert!(matches("*/py*on", "dev-lang/python-3.12.4", None));
        assert!(!matches("*/py*on", "dev-lang/pythonx-1", None));
    }

    #[test]
    fn a_slot_must_match_the_packages_slot() {
        assert!(matches(
            "dev-lang/python:3.12",
            "dev-lang/python-3.12.4",
            Some("3.12")
        ));
        assert!(!matches(
            "dev-lang/python:3.12",
            "dev-lang/python-3.13.0",
            Some("3.13")
        ));
        assert!(!matches(
            "*/python:3.12",
            "dev-lang/python-3.13.0",
            Some("3.13")
        ));
    }

    #[test]
    fn operators_and_repos_are_accepted() {
        assert!(matches(
            ">=dev-python/jinja2-3",
            "dev-python/jinja2-3.1.6",
            None
        ));
        assert!(!matches(
            "<dev-python/jinja2-3",
            "dev-python/jinja2-3.1.6",
            None
        ));
        assert!(matches(
            "dev-python/jinja2::gentoo",
            "dev-python/jinja2-3.1.6",
            None
        ));
    }

    #[test]
    fn invalid_words_are_rejected() {
        for word in [
            "not an atom",
            ">=jinja2-3",
            ">=dev-python/*-3",
            "dev-python/",
            "-foo",
        ] {
            assert!(AtomPattern::parse(word).is_err(), "{word}");
        }
    }
}
