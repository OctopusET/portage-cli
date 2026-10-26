//! Post-solve USE analysis: from a solved graph, compute the USE flag changes
//! the solution implies (the "needed" set) — autounmask suggestions for new
//! packages and rebuild requirements for installed ones

use std::collections::{BTreeSet, HashMap};

use portage_atom::interner::{DefaultInterner, Interned};
use portage_atom::{UseDefault, UseDepKind, Version};
use pubgrub::SelectedDependencies;

use crate::convert;
use crate::package::PortagePackage;
use crate::use_config::UseFlagState;

use super::{PortageDependencyProvider, UseFlagRequirement};

/// Per-target accumulator in the post-solve fixpoint: the resolved version, the
/// flags that must be enabled / disabled, and the requiring packages' CPNs
type TargetAcc = (
    Version,
    BTreeSet<Interned<DefaultInterner>>,
    BTreeSet<Interned<DefaultInterner>>,
    BTreeSet<Interned<DefaultInterner>>,
);

pub(crate) fn eval_violated_use_dep(
    kind: UseDepKind,
    dep_effective_enabled: bool,
    parent_flag_enabled: bool,
) -> Option<bool> {
    match kind {
        UseDepKind::Enabled => (!dep_effective_enabled).then_some(true),
        UseDepKind::Disabled => dep_effective_enabled.then_some(false),
        // [flag?]: if parent has flag → dep must have flag
        UseDepKind::Conditional => (parent_flag_enabled && !dep_effective_enabled).then_some(true),
        // [!flag?] = `!flag? ( dep[-flag] )` (PMS 8.2.6.4): if parent lacks the
        // flag, the dep must have it *disabled*; violated when it's enabled.
        UseDepKind::ConditionalInverse => {
            (!parent_flag_enabled && dep_effective_enabled).then_some(false)
        }
        // [flag=]: dep must match parent
        UseDepKind::Equal => {
            (dep_effective_enabled != parent_flag_enabled).then_some(parent_flag_enabled)
        }
        // [!flag=]: dep must be opposite of parent
        UseDepKind::EqualInverse => {
            let required = !parent_flag_enabled;
            (dep_effective_enabled == parent_flag_enabled).then_some(required)
        }
    }
}

impl PortageDependencyProvider {
    /// The value the solver chose for the parent's ceded (`SolverDecided`) `flag`, if it chose one
    fn solved_parent_flag(
        &self,
        constraint: &convert::UseDepConstraint,
        flag: Interned<DefaultInterner>,
        solution: &SelectedDependencies<PortagePackage, Version>,
    ) -> Option<bool> {
        if self.use_decision_meta.is_empty() {
            return None;
        }
        let node = convert::use_decision_package(&constraint.parent_cpn_str, &flag);
        solution.get(&node).map(|v| *v == Version::new(&[1]))
    }

    /// Whether an `||` branch's real parent is kept at its installed version with `flag`
    /// already demanded of it in this fixpoint
    fn pending_installed_parent_enable(
        &self,
        constraint: &convert::UseDepConstraint,
        flag: Interned<DefaultInterner>,
        solution: &SelectedDependencies<PortagePackage, Version>,
        by_target: &HashMap<PortagePackage, TargetAcc>,
    ) -> bool {
        let Some((cat, pn)) = constraint.parent_cpn_str.as_str().split_once('/') else {
            return false;
        };
        by_target.iter().any(|(pkg, (_, enable, _, _))| {
            enable.contains(&flag)
                && pkg.cpn().package.as_str() == pn
                && pkg.cpn().category.as_str() == cat
                && self
                    .installed
                    .get(pkg)
                    .is_some_and(|(iv, _)| solution.get(pkg) == Some(iv))
        })
    }

    /// Evaluate one parent's USE-dep constraints against the current
    /// solution and accumulate any violations into `by_target`
    ///
    /// Shared by the two passes of the [`compute_use_flag_requirements`]
    /// fixpoint: main-solution (parent = a solved package at its solution
    /// version) and upgrade-expansion (parent = an installed package at
    /// its pending upgrade version). `parent_ver` is the version being
    /// expanded.
    fn accumulate_use_dep_violations(
        &self,
        parent: &PortagePackage,
        parent_ver: &Version,
        parent_is_installed: bool,
        udeps: &[convert::UseDepConstraint],
        solution: &SelectedDependencies<PortagePackage, Version>,
        by_target: &mut HashMap<PortagePackage, TargetAcc>,
    ) {
        // The synthetic root's constraints are the requested atoms', credited to their labels.
        let credit_parent = !parent.is_virtual() || *parent == PortagePackage::synthetic_root();
        for constraint in udeps {
            let (target_pkg, vs) = &constraint.target;
            if target_pkg.is_virtual() {
                continue;
            }

            // Resolve target version and whether it is installed.
            let (target_ver, is_installed) =
                if let Some((inst_ver, _)) = self.installed.get(target_pkg) {
                    if vs.contains(inst_ver) {
                        (inst_ver, true)
                    } else {
                        continue;
                    }
                } else if let Some(sol_ver) = solution.get(target_pkg) {
                    if vs.contains(sol_ver) {
                        (sol_ver, false)
                    } else {
                        continue;
                    }
                } else {
                    continue;
                };

            for ud in &constraint.use_deps {
                // Parent's flag state: currently active OR will be enabled after
                // this build run (already recorded in by_target.required_enabled).
                // An || branch's constraints sit on its choice node, which has no USE of its own.
                let parent_flag_enabled = if parent.is_virtual() {
                    self.solved_parent_flag(constraint, ud.flag, solution)
                        .unwrap_or_else(|| constraint.parent_enabled.contains(&ud.flag))
                        || self.pending_installed_parent_enable(
                            constraint, ud.flag, solution, by_target,
                        )
                } else if parent_is_installed {
                    self.installed_use
                        .get(parent)
                        .is_some_and(|u| u.contains(&ud.flag))
                        || by_target
                            .get(parent)
                            .is_some_and(|(_, e, _, _)| e.contains(&ud.flag))
                } else {
                    self.solved_parent_flag(constraint, ud.flag, solution)
                        .unwrap_or_else(|| {
                            self.effective_flag_new(parent, parent_ver, ud.flag, None)
                        })
                };

                let dep_effective_enabled = if is_installed {
                    let active = self
                        .installed_use
                        .get(target_pkg)
                        .map(Vec::as_slice)
                        .unwrap_or(&[]);
                    let in_iuse = self
                        .packages
                        .get(target_pkg)
                        .and_then(|d| d.versions.get(target_ver))
                        .is_some_and(|vd| vd.iuse.contains(&ud.flag))
                        || self
                            .installed_iuse
                            .get(target_pkg)
                            .is_some_and(|i| i.contains(&ud.flag));
                    if in_iuse {
                        active.contains(&ud.flag)
                    } else {
                        matches!(ud.default, Some(UseDefault::Enabled))
                    }
                } else {
                    self.effective_flag_new(target_pkg, target_ver, ud.flag, ud.default)
                };

                // Account for a decision already made for this target in this pass:
                // a pending enable/disable overrides the static state so a second
                // requirer demanding the opposite value sees the conflict and
                // records its (opposite) requirement too, instead of being silently
                // satisfied. This surfaces `dev/foo[bar]` vs `dev/foo[-bar]` as both
                // sides recorded rather than one being lost.
                let dep_effective_enabled = match by_target.get(target_pkg) {
                    Some((_, en, _, _)) if en.contains(&ud.flag) => true,
                    Some((_, _, di, _)) if di.contains(&ud.flag) => false,
                    _ => dep_effective_enabled,
                };

                // A flag outside the target's IUSE fails the atom whatever its state, so it is
                // always recorded, as the change the atom asks for.
                let violation = if self.use_dep_flag_missing(target_pkg, target_ver, ud) {
                    Some(match ud.kind {
                        UseDepKind::Enabled | UseDepKind::Conditional => true,
                        UseDepKind::Disabled | UseDepKind::ConditionalInverse => false,
                        UseDepKind::Equal => parent_flag_enabled,
                        UseDepKind::EqualInverse => !parent_flag_enabled,
                    })
                } else {
                    eval_violated_use_dep(ud.kind, dep_effective_enabled, parent_flag_enabled)
                };
                if let Some(requires_enabled) = violation {
                    let entry = by_target.entry(target_pkg.clone()).or_insert_with(|| {
                        (
                            target_ver.clone(),
                            BTreeSet::new(),
                            BTreeSet::new(),
                            BTreeSet::new(),
                        )
                    });
                    if requires_enabled {
                        entry.1.insert(ud.flag);
                    } else {
                        entry.2.insert(ud.flag);
                    }
                    if credit_parent {
                        entry.3.insert(constraint.parent_cpn_str);
                    }
                }
            }
        }
    }

    /// Walk the full PubGrub solution (including virtual choice packages)
    /// and collect USE flag requirements for every package with at least
    /// one violated or unsatisfied USE dep constraint
    ///
    /// Installed packages are checked against their VDB-recorded active
    /// USE (only violations collected); non-installed packages are checked
    /// against the global `use_config` (unset-by-config requirements
    /// collected as annotations).
    pub(crate) fn compute_use_flag_requirements(
        &self,
        solution: &SelectedDependencies<PortagePackage, Version>,
    ) -> Vec<UseFlagRequirement> {
        // Accumulate per target: (version, enable_set, disable_set, requirers).
        let mut by_target: HashMap<PortagePackage, TargetAcc> = HashMap::new();
        // Installed packages that should be upgraded to a newer repo version
        // rather than rebuilt at the installed version.  Keyed by the installed
        // package; value is the newer version to build instead.
        let mut upgrade_to: HashMap<PortagePackage, Version> = HashMap::new();
        let root = PortagePackage::synthetic_root();
        let root_ver = Version::parse("0").expect("\"0\" is a version");

        // Iterate to fixpoint:
        // 1. Conditional deps cascade — when package A needs flag X, A's own
        //    `B[X(-)?]` deps fire, requiring B to have X as well.
        // 2. When an installed package gains a violation, check if a newer repo
        //    version exists whose constraints should also be expanded (upgrade
        //    the package rather than rebuilding the old version).
        loop {
            let prev_total: usize = by_target
                .values()
                .map(|(_, e, d, _)| e.len() + d.len())
                .sum();
            let prev_upgrades = upgrade_to.len();

            // -- main solution packages --
            for (pkg, ver) in solution.iter() {
                let Some(vd) = self.package_data(pkg).and_then(|d| d.versions.get(ver)) else {
                    continue;
                };
                // "Installed" for parent-flag evaluation means *selected at its
                // installed version*: a package being upgraded gets its NEW
                // version's flags from the desired config, not the old build's
                // active USE (which would silently drop `[flag?]` conditionals
                // the upgrade newly enables).
                let at_installed_ver = self.installed.get(pkg).is_some_and(|(iv, _)| iv == ver);
                self.accumulate_use_dep_violations(
                    pkg,
                    ver,
                    at_installed_ver,
                    &vd.use_deps,
                    solution,
                    &mut by_target,
                );
            }

            // -- the requested atoms' own USE requirements --
            self.accumulate_use_dep_violations(
                &root,
                &root_ver,
                false,
                &self.root_use_deps,
                solution,
                &mut by_target,
            );

            // -- upgrade expansion --
            // For each installed package with violations, check whether a newer
            // repo version exists.  If so, record the upgrade and process the
            // newer version's USE dep constraints in the next fixpoint iteration.
            let installed_with_violations: Vec<(PortagePackage, Version)> = by_target
                .iter()
                .filter(|(pkg, _)| self.installed.contains_key(pkg))
                .filter(|(pkg, _)| !upgrade_to.contains_key(*pkg))
                .filter_map(|(pkg, (inst_ver, _, _, _))| {
                    self.package_data(pkg)
                        .and_then(|d| {
                            // Widened candidate supply: never auto-upgrade an
                            // installed package onto a tagged version — that
                            // would silently unmask it outside the autounmask
                            // report path.
                            d.versions
                                .keys()
                                .filter(|v| v > &inst_ver && !d.versions[*v].needs_unmask)
                                .max()
                        })
                        .map(|new_ver| (pkg.clone(), new_ver.clone()))
                })
                .collect();

            for (pkg, new_ver) in installed_with_violations {
                upgrade_to.insert(pkg.clone(), new_ver.clone());

                // Expand the newer version's USE dep constraints. The "parent" is
                // the upgraded package itself at its new version.
                let Some(vd) = self
                    .package_data(&pkg)
                    .and_then(|d| d.versions.get(&new_ver))
                else {
                    continue;
                };
                // The upgraded version is by definition not the installed build.
                self.accumulate_use_dep_violations(
                    &pkg,
                    &new_ver,
                    false,
                    &vd.use_deps,
                    solution,
                    &mut by_target,
                );
            }

            let new_total: usize = by_target
                .values()
                .map(|(_, e, d, _)| e.len() + d.len())
                .sum();
            if new_total == prev_total && upgrade_to.len() == prev_upgrades {
                break;
            }
        }

        let mut reqs: Vec<UseFlagRequirement> = by_target
            .into_iter()
            .map(
                |(pkg, (ver, enable, disable, requirers))| UseFlagRequirement {
                    package: pkg.clone(),
                    version: ver,
                    upgrade_to: upgrade_to.remove(&pkg),
                    required_enabled: enable.into_iter().collect(),
                    required_disabled: disable.into_iter().collect(),
                    required_by: requirers.into_iter().map(|s| s.to_string()).collect(),
                },
            )
            .collect();
        // `by_target` is a HashMap, so collect order is nondeterministic; sort by
        // (package, version) so use_flag_requirements — and everything derived
        // from it (reinstall_deps → the appended merge-order tail, and the
        // autounmask report order) — is reproducible across runs.
        reqs.sort_by(|a, b| {
            a.package
                .cmp(&b.package)
                .then_with(|| a.version.cmp(&b.version))
        });
        reqs
    }

    /// Return all USE flag requirements collected by the post-solve validation pass
    ///
    /// Includes both reinstall candidates (`R`) and informational annotations
    /// for newly-installed packages.  Populated by `resolve_targets`.
    pub fn use_flag_requirements(&self) -> &[UseFlagRequirement] {
        &self.use_flag_requirements
    }

    /// Return only the requirements that correspond to reinstalls: installed
    /// packages whose current USE flags violate at least one constraint from the
    /// resolved set
    pub fn reinstall_deps(&self) -> Vec<&UseFlagRequirement> {
        self.use_flag_requirements
            .iter()
            .filter(|r| self.installed.contains_key(&r.package))
            .collect()
    }

    /// Effective state of `flag` on a non-installed package version that
    /// will be freshly built
    ///
    /// Mirrors what the build will see: `package.use` and global USE
    /// applied on the ebuild's IUSE defaults; outside IUSE, the dep's own
    /// `(+)`/`(-)` default.
    ///
    /// Except at the installed version, where the VDB's own record is
    /// ground truth instead — whatever the already-built package has,
    /// independent of what the current tree metadata says (covers both a
    /// revbumped/pruned version and an in-place IUSE edit with no revbump).
    pub(crate) fn effective_flag_new(
        &self,
        pkg: &PortagePackage,
        ver: &Version,
        flag: Interned<DefaultInterner>,
        dep_default: Option<UseDefault>,
    ) -> bool {
        let vd = self.package_data(pkg).and_then(|d| d.versions.get(ver));
        let in_iuse = vd.is_some_and(|v| v.iuse.contains(&flag));
        if in_iuse {
            // `desired` already folds package.use, global USE, and IUSE
            // defaults, so a single lookup gives the flag's effective state.
            return vd.is_some_and(|v| v.desired.get(flag) == UseFlagState::Enabled);
        }
        let at_installed_ver = self.installed.get(pkg).is_some_and(|(iv, _)| iv == ver);
        if at_installed_ver
            && let Some(iuse) = self.installed_iuse.get(pkg)
            && iuse.contains(&flag)
        {
            return self
                .installed_use
                .get(pkg)
                .is_some_and(|u| u.contains(&flag));
        }
        matches!(dep_default, Some(UseDefault::Enabled))
    }

    /// Whether `ud` names a flag outside `pkg-ver`'s IUSE (or its VDB IUSE at the installed
    /// version) without a `(+)`/`(-)` default, so the atom cannot match it (PMS 8.3.4)
    pub(crate) fn use_dep_flag_missing(
        &self,
        pkg: &PortagePackage,
        ver: &Version,
        ud: &portage_atom::UseDep,
    ) -> bool {
        if ud.default.is_some() {
            return false;
        }
        let in_repo = self
            .package_data(pkg)
            .and_then(|d| d.versions.get(ver))
            .is_some_and(|v| v.iuse.contains(&ud.flag));
        let in_vdb = self.installed.get(pkg).is_some_and(|(iv, _)| iv == ver)
            && self
                .installed_iuse
                .get(pkg)
                .is_some_and(|i| i.contains(&ud.flag));
        !in_repo && !in_vdb
    }

    /// Check whether all USE dep constraints for an OR-group branch are
    /// consistent with the desired final state of their targets
    ///
    /// An installed target in range is judged on its VDB USE, or its desired USE when a
    /// rebuild would enable the flag; any other target on its newest accepted version in
    /// range. A constraint fanned out over a multi-slot target holds when one slot does.
    /// Returns `true` when every constraint holds or has no version to judge.
    pub(crate) fn use_dep_branch_satisfied(&self, udeps: &[convert::UseDepConstraint]) -> bool {
        let verdicts: Vec<Option<bool>> = udeps
            .iter()
            .map(|c| self.use_dep_constraint_satisfied(c))
            .collect();
        udeps.iter().zip(&verdicts).all(|(c, v)| {
            *v != Some(false)
                || udeps
                    .iter()
                    .zip(&verdicts)
                    .any(|(o, ov)| *ov == Some(true) && c.fanout.is_some() && o.fanout == c.fanout)
        })
    }

    /// One [`Self::use_dep_branch_satisfied`] constraint; `None` when no version is in range
    fn use_dep_constraint_satisfied(&self, constraint: &convert::UseDepConstraint) -> Option<bool> {
        let (target_pkg, vs) = &constraint.target;
        let installed = self
            .installed
            .get(target_pkg)
            .map(|(v, _)| v)
            .filter(|v| vs.contains(v));
        let ver = match installed {
            Some(v) => v.clone(),
            None => self
                .package_data(target_pkg)?
                .versions
                .iter()
                .filter(|(v, vd)| vs.contains(v) && !vd.needs_unmask && !vd.installed_only)
                .map(|(v, _)| v)
                .max()?
                .clone(),
        };
        let active = match installed {
            Some(_) => self
                .installed_use
                .get(target_pkg)
                .map(Vec::as_slice)
                .unwrap_or(&[]),
            None => &[],
        };
        Some(constraint.use_deps.iter().all(|ud| {
            if self.use_dep_flag_missing(target_pkg, &ver, ud) {
                return false;
            }
            let parent_flag_enabled = constraint.parent_enabled.contains(&ud.flag);
            let dep_effective_enabled = active.contains(&ud.flag)
                || self.effective_flag_new(target_pkg, &ver, ud.flag, ud.default);
            eval_violated_use_dep(ud.kind, dep_effective_enabled, parent_flag_enabled).is_none()
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::eval_violated_use_dep;
    use portage_atom::UseDepKind;

    // PMS 8.2.6.4 six-form table, evaluated directly. `Some(true)` = violated,
    // flag must be enabled; `Some(false)` = violated, flag must be disabled;
    // `None` = satisfied. Both conditional-inverse rows are the pre-fix trap:
    // `!flag?` with parent OFF requires the dep flag *disabled*
    // (`!flag? ( dep[-flag] )`), so parent-off/dep-off is satisfied and
    /// parent-off/dep-on is the violation (must disable), not the reverse.
    #[test]
    fn eval_violated_use_dep_matches_pms_table() {
        use UseDepKind::*;
        // (kind, dep_enabled, parent_enabled) -> expected
        let cases = [
            (Enabled, true, false, None),
            (Enabled, false, false, Some(true)),
            (Disabled, false, false, None),
            (Disabled, true, false, Some(false)),
            // [flag?]: parent on -> dep must be on
            (Conditional, true, true, None),
            (Conditional, false, true, Some(true)),
            (Conditional, false, false, None),
            // [!flag?]: parent off -> dep must be OFF
            (ConditionalInverse, false, false, None),
            (ConditionalInverse, true, false, Some(false)),
            (ConditionalInverse, true, true, None),
            // [flag=]: dep must match parent
            (Equal, true, true, None),
            (Equal, false, true, Some(true)),
            (Equal, true, false, Some(false)),
            // [!flag=]: dep must be opposite of parent
            (EqualInverse, false, true, None),
            (EqualInverse, true, true, Some(false)),
            (EqualInverse, false, false, Some(true)),
        ];
        for (kind, dep, parent, expected) in cases {
            assert_eq!(
                eval_violated_use_dep(kind, dep, parent),
                expected,
                "kind={kind:?} dep_enabled={dep} parent_enabled={parent}"
            );
        }
    }
}
