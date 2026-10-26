use std::collections::{BTreeMap, HashMap};

use camino::{Utf8Path, Utf8PathBuf};
use portage_atom::interner::Interned;
use portage_atom::{ConfigAtom, Dep, order_by_specificity};
use portage_atom_pubgrub::{ProfileUseNode, UseLayer, UseOverride};
use portage_repo::{
    AcceptSet, Environment, LicenseGroupRegistry, MakeConf, ProfileStack, RepoSet, Settings,
    SettingsSources,
};

use crate::force_mask::{ForceMask, ForceMaskLayer, fold_signed, index_by_cpn};
use crate::repo::AcceptToken;

type Result<T> = anyhow::Result<T>;

/// Resolved USE environment for the solver and display
pub struct UseEnv {
    /// Folded profile `make.defaults` (Portage's `defaults` layer, minus
    /// per-package `package.use`)
    ///
    /// Shared Arc base for every CPV.
    pub defaults: UseLayer,
    /// `make.conf` / extra_confs USE delta (Portage's `conf` layer)
    pub conf: UseLayer,
    /// Process-environment USE layer (`ResolvedUse::env_use`), **parsed once**
    ///
    /// Portage's `env` layer, folded in *after* `package_use`. A `-*` here
    /// wipes `package_use`; a conf-level `-*` must not.
    pub env_use: UseLayer,
    /// Keys from `USE_EXPAND` — used to group expanded flags in display
    pub expand: Vec<String>,
    /// Keys from `USE_EXPAND_HIDDEN` — groups to suppress in display
    pub expand_hidden: Vec<String>,
    /// Per-package USE overrides from `/etc/portage/package.use` and
    /// `package.env`: portage's `pkg` layer, above `conf`/make.conf
    ///
    /// Each file is ordered by atom specificity; its `*/*` lines are in `conf` instead.
    pub package_use: Vec<(ConfigAtom, Vec<UseOverride>)>,
    /// Per-profile-node `package.use` plus that node's make.defaults flags
    ///
    /// Parent first — Portage `setcpv` interleave inside `defaults`.
    pub profile_package_use: Vec<ProfileUseNode>,
    /// Masked packages: each repo's `profiles/package.mask`, the profile
    /// stack, and `/etc/portage/package.mask`
    pub package_mask: Vec<ConfigAtom>,
    /// Unmasks from each repo's `profiles/package.unmask`, the profiles and
    /// `/etc/portage/package.unmask`: a matching entry cancels any mask for that package
    pub package_unmask: Vec<ConfigAtom>,
    /// Profile USE force/mask policy (global + per-package + stable variants),
    /// applied per package to effective USE and consulted by the Level-C cede gate
    pub force_mask: ForceMask,
    /// Effective global `ACCEPT_KEYWORDS`, parsed to interned tokens
    pub accept_keywords: Vec<AcceptToken>,
    /// Per-package `package.accept_keywords` (and legacy `package.keywords`) entries
    ///
    /// `(atom, [tokens])`, tokens interned.
    ///
    /// A bare atom carries an empty token list (expanded to `~arch` when the host
    /// arch is known). Each file is ordered by atom specificity.
    pub package_accept_keywords: Vec<(ConfigAtom, Vec<AcceptToken>)>,
    /// Effective global `ACCEPT_LICENSE` after `@GROUP` expansion and `-` denials
    pub accept_license: AcceptSet,
    /// Per-package `package.license` entries: `(atom, overlay)`, each overlay
    /// already parsed/expanded against the license groups
    pub package_license: Vec<(ConfigAtom, AcceptSet)>,
    /// Effective global `ACCEPT_PROPERTIES`
    ///
    /// Same token grammar as `ACCEPT_LICENSE` minus `@GROUP`
    /// (`AcceptSet::from_tokens_plain`).
    pub accept_properties: AcceptSet,
    /// Per-package `package.properties` entries — see `package_license`
    pub package_properties: Vec<(ConfigAtom, AcceptSet)>,
    /// Effective global `ACCEPT_RESTRICT`. See `accept_properties`
    pub accept_restrict: AcceptSet,
    /// Per-package `package.accept_restrict` entries — see `package_license`
    pub package_restrict: Vec<(ConfigAtom, AcceptSet)>,
    /// Resolved `DISTDIR` (where fetched distfiles live), for download-size accounting
    pub distdir: String,
    /// `package.provided` CPVs from the profile stack
    ///
    /// Packages the system supplies externally (e.g. a host interpreter in
    /// a Gentoo Prefix).
    ///
    /// They satisfy matching deps and are never built or shown for merge.
    pub provided: Vec<portage_atom::Cpv>,
}

/// A `make.profile` `em` would write but has not yet
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PendingProfile {
    /// Real, already-existing profile directories it inherits, in order (one for a
    /// `make.profile` linking a profile directly)
    pub parents: Vec<Utf8PathBuf>,
    /// What it adds on top of `parents`, if it is a derived one
    pub layer: Option<ProfileLayer>,
}

/// The files of a profile `em` derives, beyond its `parent`: the in-memory form the files
/// are rendered from, and what a pending sysroot resolves with
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProfileLayer {
    /// `package.mask`
    pub package_mask: Vec<Dep>,
    /// `package.unmask`
    pub package_unmask: Vec<Dep>,
    /// `package.use`
    pub package_use: Vec<(Dep, Vec<String>)>,
}

/// Config `em` would write into a root's `/etc/portage` but has not yet (a `-p` run before
/// `--init-target`): the in-memory form the files are rendered from, which a resolve then
/// reads in their place
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PendingConfig {
    /// The profile `make.profile` would link, standing in for the root's own
    pub profile: Option<PendingProfile>,
    /// `make.conf` content, read after any `make.conf` on disk
    pub make_conf: Option<String>,
    /// `package.accept_keywords` lines
    pub package_accept_keywords: Vec<(Dep, Vec<AcceptToken>)>,
    /// `package.mask`
    pub package_mask: Vec<Dep>,
    /// `package.unmask`
    pub package_unmask: Vec<Dep>,
}

/// Read the config/profile/environment sources (profile stack, `make.conf`,
/// `package.use`/`.mask`/`.unmask`/`.license`/`.accept_keywords`, USE force/
/// mask) into a resolved [`UseEnv`], the shared input every per-package
/// policy fold in this crate runs on
pub async fn build_use_env(
    repos: &RepoSet,
    root: Option<&Utf8Path>,
    config_overlay: Option<&Utf8Path>,
    extra_use_override: Option<&str>,
    pending: Option<&PendingConfig>,
) -> Result<UseEnv> {
    compute_use_env(repos, root, config_overlay, extra_use_override, pending).await
}

async fn compute_use_env(
    repos: &RepoSet,
    root: Option<&Utf8Path>,
    config_overlay: Option<&Utf8Path>,
    extra_use_override: Option<&str>,
    pending: Option<&PendingConfig>,
) -> Result<UseEnv> {
    let portage_dir = root.unwrap_or(Utf8Path::new("/")).join("etc/portage");
    let root_dir = root.unwrap_or(Utf8Path::new("/"));

    // A pending profile's parents stand in for `make.profile`, and its own layer is applied below.
    let pending_profile = pending.and_then(|p| p.profile.as_ref());
    let stack = match pending_profile {
        Some(profile) => {
            let parents: Vec<_> = profile
                .parents
                .iter()
                .map(|p| p.as_std_path().to_path_buf())
                .collect();
            ProfileStack::build_parents_in(&parents, root_dir)
        }
        None => {
            let profile_link = portage_dir.join("make.profile");
            let profile_path = std::fs::canonicalize(profile_link.as_std_path())
                .map_err(|e| anyhow::anyhow!("cannot resolve {profile_link}: {e}"))?;
            ProfileStack::build_in(profile_path, root_dir)
        }
    };
    let layer = pending_profile.and_then(|p| p.layer.as_ref());
    // Portage appends `/etc/portage/profile` as the top (highest-priority)
    // profile layer, so its use.force/use.mask/package.use*/package.mask override
    // the resolved make.profile chain. Fold it in so Level-C never cedes a flag a
    // site override pins and the plan honours site masks (portage(5)).
    let mut stack = stack
        .map_err(|e| anyhow::anyhow!("failed to build profile stack: {e}"))?
        .with_user_profile(portage_dir.join("profile").into_std_path_buf())
        .map_err(|e| anyhow::anyhow!("failed to load /etc/portage/profile: {e}"))?;
    // `--prefix`'s config overlay is a second user-profile on top, same
    // precedence as the per-package files below. Site `use.force`/`use.mask`
    // there have to reach the depgraph the same way the build shell sees them.
    if let Some(overlay) = config_overlay {
        stack = stack
            .with_user_profile(overlay.join("profile").into_std_path_buf())
            .map_err(|e| anyhow::anyhow!("failed to load the prefix overlay profile: {e}"))?;
    }
    let repo = repos.main();
    let mut shell = repo
        .shell()
        .await
        .map_err(|e| anyhow::anyhow!("failed to start ebuild shell: {e}"))?;

    // make.conf(5): path may be a file or a directory of Flat fragments.
    // Portage sources legacy `/etc/make.conf` first, then `/etc/portage/make.conf`
    // so the latter overrides (config.py make_conf_paths order). A missing path
    // (the pending-sysroot case) lists as empty, not an error.
    let conf_owned = portage_repo::expand_make_conf_paths([
        root_dir.join("etc/make.conf"),
        root_dir.join("etc/portage/make.conf"),
    ])
    .map_err(|e| anyhow::anyhow!("listing make.conf: {e}"))?;
    let mut confs: Vec<portage_repo::ConfSource> = conf_owned
        .iter()
        .map(|p| portage_repo::ConfSource::File(p.as_path()))
        .collect();
    if let Some(make_conf) = pending.and_then(|p| p.make_conf.as_deref()) {
        confs.push(portage_repo::ConfSource::Str(make_conf));
    }

    // env.d and make.globals set nothing a resolve reads (ACCEPT_* have Portage's defaults below).
    let env = Environment::process();
    let (settings, resolved) = Settings::evaluate_with_use(
        &mut shell,
        SettingsSources {
            stack: &stack,
            env_d: None,
            globals: None,
            confs: &confs,
            env: &env,
        },
        extra_use_override,
    )
    .await
    .map_err(|e| anyhow::anyhow!("failed to evaluate USE flags: {e}"))?;

    let split_var = |name: &str| -> Vec<String> {
        settings
            .get(name)
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_string)
            .collect()
    };

    let expand = split_var("USE_EXPAND");
    let expand_hidden = split_var("USE_EXPAND_HIDDEN");
    let accept_keywords =
        effective_accept_keywords(&split_var, settings.get("ARCH").unwrap_or_default());

    // Per-package keyword acceptance, in portage's precedence order: each profile,
    // then site `package.keywords` + `package.accept_keywords` (one layer, as in
    // Portage), then the config overlay. Bare atoms mean "accept this package's `~arch`".
    let mut package_accept_keywords: Vec<(ConfigAtom, Vec<AcceptToken>)> = Vec::new();
    for profile in stack.profiles() {
        let entries = profile.package_accept_keywords().unwrap_or_default();
        package_accept_keywords.extend(parse_keyword_tokens(
            entries
                .into_iter()
                .map(|(dep, toks)| (ConfigAtom::Dep(dep), toks))
                .collect(),
        ));
    }
    let mut site_keywords = load_atom_tokens(portage_dir.join("package.keywords").as_str());
    site_keywords.extend(load_atom_tokens(
        portage_dir.join("package.accept_keywords").as_str(),
    ));
    package_accept_keywords.extend(parse_keyword_tokens(site_keywords));
    if let Some(overlay) = config_overlay {
        package_accept_keywords.extend(parse_keyword_tokens(load_atom_tokens(
            overlay.join("package.accept_keywords").as_str(),
        )));
    }
    if let Some(pending) = pending {
        package_accept_keywords.extend(
            pending
                .package_accept_keywords
                .iter()
                .map(|(dep, toks)| (ConfigAtom::Dep(dep.clone()), toks.clone())),
        );
    }
    let license_groups = LicenseGroupRegistry::from_files(
        repos
            .iter()
            .map(|r| r.path().join("profiles/license_groups"))
            .chain(std::iter::once(portage_dir.join("license_groups")))
            .chain(config_overlay.map(|o| o.join("license_groups"))),
    );
    // The `ACCEPT_*` lists arrive stacked, the process environment included; an empty
    // `ACCEPT_LICENSE` takes Portage's default.
    let mut accept_license_tokens = split_var("ACCEPT_LICENSE");
    if accept_license_tokens.is_empty() {
        accept_license_tokens = vec!["*".to_string(), "-@EULA".to_string()];
    }

    // Per-package `package.license`: the profile stack and the site file form one
    // layer (one dict in Portage), the config overlay another.
    let mut site_license: Vec<(ConfigAtom, Vec<String>)> = stack
        .package_license()
        .unwrap_or_default()
        .into_iter()
        .map(|(dep, toks)| (ConfigAtom::Dep(dep), toks))
        .collect();
    site_license.extend(load_atom_tokens(
        portage_dir.join("package.license").as_str(),
    ));
    let overlay_license = config_overlay
        .map(|o| load_atom_tokens(o.join("package.license").as_str()))
        .unwrap_or_default();
    let package_license = per_package_accept(
        [site_license, overlay_license],
        &mut accept_license_tokens,
        |toks| AcceptSet::from_tokens(toks, &license_groups),
    );
    let accept_license = AcceptSet::from_tokens(&accept_license_tokens, &license_groups);

    // ACCEPT_PROPERTIES / ACCEPT_RESTRICT: same shape as ACCEPT_LICENSE minus
    // @GROUP support. make.globals' `*` is the bottom layer, so a lone `-token` denies only that.
    let accept_list = |name: &str| {
        let mut v = vec!["*".to_string()];
        v.extend(split_var(name));
        v
    };
    let site_and_overlay = |name: &str| {
        [
            load_atom_tokens(portage_dir.join(name).as_str()),
            config_overlay
                .map(|o| load_atom_tokens(o.join(name).as_str()))
                .unwrap_or_default(),
        ]
    };
    let mut accept_properties_tokens = accept_list("ACCEPT_PROPERTIES");
    let package_properties = per_package_accept(
        site_and_overlay("package.properties"),
        &mut accept_properties_tokens,
        AcceptSet::from_tokens_plain,
    );
    let accept_properties = AcceptSet::from_tokens_plain(&accept_properties_tokens);

    let mut accept_restrict_tokens = accept_list("ACCEPT_RESTRICT");
    let package_restrict = per_package_accept(
        site_and_overlay("package.accept_restrict"),
        &mut accept_restrict_tokens,
        AcceptSet::from_tokens_plain,
    );
    let accept_restrict = AcceptSet::from_tokens_plain(&accept_restrict_tokens);

    let distdir = shell
        .get_var("DISTDIR")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/var/cache/distfiles".to_string());

    // Profile/make.conf fold and process-env USE, tokenized once here so
    // every per-package `resolve_effective_use` reuses interned layer tokens
    // (USE_EXPAND can put 100+ flags into pre_env — re-splitting that string
    // per CPV is pure waste). Kept as two layers: a `-*` in env must still
    // wipe package.use while a conf-level `-*` must not. Each layer also
    // carries the USE_EXPAND groups it explicitly assigned, so the per-package
    // fold can apply portage's non-incremental group replace to the ebuild's
    // own `+`-defaulted IUSE — `L10N="en-US"` in make.conf must beat
    // chromium-2.eclass's `IUSE="+l10n_${lang}"` for every bundled locale.
    let defaults = UseLayer::parse(&resolved.defaults_use);
    let env_use =
        UseLayer::parse(&resolved.env_use).with_group_clears(resolved.env_expand_assigned);

    // Profile `package.use` is inside Portage's `defaults` layer, after that
    // node's make.defaults and below make.conf. Per-node so a child
    // make.defaults can restate a flag a parent package.use turned off.
    let expand_keys = &expand;
    let expand_values = |key: &str| split_var(key);
    let expand_flags = |raw: Vec<(ConfigAtom, Vec<String>)>| {
        order_by_specificity(raw)
            .into_iter()
            .map(|(atom, flags)| {
                (
                    atom,
                    expand_use_expand_colon(&flags, expand_keys, &expand_values),
                )
            })
            .collect::<Vec<_>>()
    };
    let profile_node = |raw: Vec<(Dep, Vec<String>)>, defaults: UseLayer| {
        let puse_clear_all = raw
            .iter()
            .any(|(_, flags)| flags_have_use_clear_all(flags, expand_keys));
        let raw = raw
            .into_iter()
            .map(|(dep, flags)| (ConfigAtom::Dep(dep), flags))
            .collect();
        ProfileUseNode {
            defaults,
            package_use: expand_flags(raw),
            puse_clear_all,
        }
    };
    let mut defaults_by_path: HashMap<std::path::PathBuf, String> =
        resolved.profile_defaults.into_iter().collect();
    let mut profile_package_use: Vec<ProfileUseNode> = stack
        .profiles()
        .iter()
        .map(|p| {
            let md = p.path().join("make.defaults");
            let layer_defaults = defaults_by_path
                .remove(&md)
                .map(|s| UseLayer::parse(&s))
                .unwrap_or_else(UseLayer::empty);
            profile_node(p.package_use().unwrap_or_default(), layer_defaults)
        })
        .collect();
    // The derived profile is the leaf, after its parents; it sets no USE defaults of its own.
    if let Some(layer) = layer {
        profile_package_use.push(profile_node(layer.package_use.clone(), UseLayer::empty()));
    }

    // User `package.use`, then `package.env`'s own USE, per config directory (host,
    // then overlay; Portage does not interleave config layers). A `*/*` line of either
    // extends make.conf's USE, as Portage folds it into the `conf` layer.
    let mut conf_use = resolved.conf_use.clone();
    let mut package_use: Vec<(ConfigAtom, Vec<UseOverride>)> = Vec::new();
    for dir in std::iter::once(portage_dir.as_path()).chain(config_overlay) {
        let raw = load_atom_tokens(dir.join("package.use").as_str())
            .into_iter()
            .filter(|(_, flags)| !flags.is_empty())
            .collect();
        let site_use = expand_flags(raw);
        let env_overrides = load_package_env_use(dir).await;
        for (atom, overrides) in site_use.into_iter().chain(env_overrides) {
            if !atom.is_match_all() {
                package_use.push((atom, overrides));
                continue;
            }
            for ov in overrides {
                let sign = if ov.enable { "" } else { "-" };
                conf_use.push_str(&format!(" {sign}{}", ov.flag));
            }
        }
    }
    let conf = UseLayer::parse(&conf_use).with_group_clears(resolved.conf_expand_assigned);

    // Mask sources, in portage's order: each repo's `profiles/package.mask` (scoped to that
    // repo), the profile chain (with `-atom` removals already resolved), and the site files,
    // whose `-atom` lines may cancel an atom from any layer. A mask is overridden per package
    // by an unmask from the same layers.
    let mut package_mask: Vec<ConfigAtom> = Vec::new();
    let mut package_unmask: Vec<ConfigAtom> = Vec::new();
    for r in repos.iter() {
        package_mask.extend(r.repo_package_mask()?.into_iter().map(ConfigAtom::Dep));
        package_unmask.extend(r.repo_package_unmask()?.into_iter().map(ConfigAtom::Dep));
    }
    let profile_masks = stack.package_mask()?.into_iter();
    package_mask.extend(
        profile_masks
            .chain(layer.iter().flat_map(|l| l.package_mask.clone()))
            .map(ConfigAtom::Dep),
    );
    let profile_unmasks = stack.package_unmask()?.into_iter();
    package_unmask.extend(
        profile_unmasks
            .chain(layer.iter().flat_map(|l| l.package_unmask.clone()))
            .map(ConfigAtom::Dep),
    );
    stack_user_atoms(
        &mut package_mask,
        load_atom_lines(portage_dir.join("package.mask").as_str()),
    );
    stack_user_atoms(
        &mut package_unmask,
        load_atom_lines(portage_dir.join("package.unmask").as_str()),
    );
    if let Some(pending) = pending {
        package_mask.extend(pending.package_mask.iter().cloned().map(ConfigAtom::Dep));
        package_unmask.extend(pending.package_unmask.iter().cloned().map(ConfigAtom::Dep));
    }
    if let Some(overlay) = config_overlay {
        stack_user_atoms(
            &mut package_mask,
            load_atom_lines(overlay.join("package.mask").as_str()),
        );
        stack_user_atoms(
            &mut package_unmask,
            load_atom_lines(overlay.join("package.unmask").as_str()),
        );
    }

    // `package.provided` — CPVs the system supplies externally (profile stack,
    // incl. the folded-in `/etc/portage/profile`). Fed to the solver as
    // dependency-satisfying, never-built packages.
    let provided = stack.package_provided().unwrap_or_default();

    // Profile USE force/mask. Both global use.force/use.mask are applied per
    // package by `ForceMask::apply` (force_mask.rs) — global use.force isn't
    // folded into `pre_env` (unlike the pre-2026-07-12 collapsed `config`),
    // so it must be applied here alongside use.mask, not assumed already
    // baked into the base state. The package-level and *.stable.* sets are
    // also per package; they carry raw `-flag` tokens so unforce/unmask is
    // resolved per package.
    let use_expand_implicit = split_var("USE_EXPAND_IMPLICIT");
    let use_expand_unprefixed = split_var("USE_EXPAND_UNPREFIXED");
    let mut expand_values = HashMap::new();
    for v in expand.iter().chain(use_expand_unprefixed.iter()) {
        let vals = split_var(&format!("USE_EXPAND_VALUES_{v}"));
        if !vals.is_empty() {
            expand_values.insert(v.clone(), vals);
        }
    }
    let force_mask = ForceMask {
        iuse_injection: crate::force_mask::IuseInjection::new(
            split_var("IUSE_IMPLICIT"),
            expand.clone(),
            use_expand_implicit,
            use_expand_unprefixed,
            expand_values,
        ),
        layers: stack
            .profiles()
            .iter()
            .map(|p| ForceMaskLayer {
                use_force: fold_signed(p.use_force().unwrap_or_default()),
                use_mask: fold_signed(p.use_mask().unwrap_or_default()),
                use_stable_force: fold_signed(p.use_stable_force().unwrap_or_default()),
                use_stable_mask: fold_signed(p.use_stable_mask().unwrap_or_default()),
                pkg_force: index_by_cpn(p.package_use_force().unwrap_or_default()),
                pkg_mask: index_by_cpn(p.package_use_mask().unwrap_or_default()),
                pkg_stable_force: index_by_cpn(p.package_use_stable_force().unwrap_or_default()),
                pkg_stable_mask: index_by_cpn(p.package_use_stable_mask().unwrap_or_default()),
            })
            .collect(),
    };

    Ok(UseEnv {
        defaults,
        conf,
        env_use,
        expand,
        expand_hidden,
        package_use,
        profile_package_use,
        package_mask,
        package_unmask,
        force_mask,
        accept_keywords,
        package_accept_keywords,
        accept_license,
        package_license,
        accept_properties,
        package_properties,
        accept_restrict,
        package_restrict,
        distdir,
        provided,
    })
}

/// `make.conf` often sets `ACCEPT_KEYWORDS="${ARCH} ~${ARCH}"`
///
/// When `ARCH` is not yet visible at source time, brush leaves `~` only —
/// rebuild from `ARCH` once the profile stack has settled.
fn effective_accept_keywords(
    split_var: &dyn Fn(&str) -> Vec<String>,
    arch: &str,
) -> Vec<AcceptToken> {
    let arch = arch.to_owned();
    let ak = split_var("ACCEPT_KEYWORDS");
    let parse = |toks: &[String]| toks.iter().filter_map(|t| AcceptToken::parse(t)).collect();
    if arch.is_empty() {
        return parse(&ak);
    }
    let testing = format!("~{arch}");
    let has_arch = ak.iter().any(|k| k == &arch || k == &testing);
    if has_arch {
        return parse(&ak);
    }
    // Broken expansion (e.g. `["~"]` or empty) — mirror portage's make.conf default.
    parse(&[arch, testing])
}

/// Load a `package.*` file as raw `(atom, [token])` lines, in file order
///
/// `#` comments, optionally a directory (children summed in lexical order). A line
/// with no token is kept: `package.accept_keywords` reads a bare atom as `~arch`.
///
/// `package.use` tokens stay verbatim; USE_EXPAND `KEY:` groups are expanded later by
/// [`expand_use_expand_colon`].
fn load_atom_tokens(path: &str) -> Vec<(ConfigAtom, Vec<String>)> {
    let mut result = Vec::new();
    for line in portage_repo::read_config_lines(path).unwrap_or_default() {
        let mut parts = line.split_whitespace();
        let Some(atom_str) = parts.next() else {
            continue;
        };
        let Some(atom) = parse_config_atom(path, atom_str) else {
            continue;
        };
        result.push((atom, parts.map(str::to_string).collect()));
    }
    result
}

/// One `package.accept_keywords` layer, ordered by atom specificity, tokens interned
fn parse_keyword_tokens(
    layer: Vec<(ConfigAtom, Vec<String>)>,
) -> impl Iterator<Item = (ConfigAtom, Vec<AcceptToken>)> {
    order_by_specificity(layer).into_iter().map(|(atom, toks)| {
        let parsed = toks.iter().filter_map(|t| AcceptToken::parse(t)).collect();
        (atom, parsed)
    })
}

/// `package.license`-style layers, each ordered by atom specificity, as per-package
/// overlays; a `*/*` line's tokens extend the global `accept_tokens` instead, as in Portage
fn per_package_accept(
    layers: impl IntoIterator<Item = Vec<(ConfigAtom, Vec<String>)>>,
    accept_tokens: &mut Vec<String>,
    to_set: impl Fn(&[String]) -> AcceptSet,
) -> Vec<(ConfigAtom, AcceptSet)> {
    let mut out = Vec::new();
    for layer in layers {
        for (atom, toks) in order_by_specificity(layer) {
            if atom.is_match_all() {
                accept_tokens.extend(toks);
            } else {
                let set = to_set(&toks);
                out.push((atom, set));
            }
        }
    }
    out
}

/// `/etc/portage/package.env`'s own USE contribution, as `package_use`-shaped
/// overrides: for each matched atom, source its env files (in line order,
/// each on top of the last, seeded empty) via a real shell via
/// [`MakeConf::apply_to`] — so `USE="${USE} -flag"` self-reference
/// evaluates correctly — and take the resulting tokens as this atom's
/// override list, [`UseOverride::parse`]d like a `package.use` line
///
/// Seeded empty (not the profile's USE) because this collects *this atom's
/// own* contribution, independent of any candidate's baseline — that folds
/// in later, per candidate, by `resolve_effective_use`. An unrelated
/// variable unset here expands empty rather than the real build env's
/// value — same approximation `binpkg::DesiredBuildEnv::key_for` makes,
/// never worse than "resolver ignores package.env USE" today.
async fn load_package_env_use(portage_dir: &Utf8Path) -> Vec<(ConfigAtom, Vec<UseOverride>)> {
    let entries =
        portage_repo::package_env::load_package_env(portage_dir.join("package.env").as_std_path());
    let env_dir = portage_dir.join("env");
    let mut out = Vec::new();
    for (atom, names) in entries {
        let mut env: BTreeMap<String, String> = BTreeMap::new();
        env.insert("USE".to_string(), String::new());
        for name in &names {
            if let Ok(mc) = MakeConf::load(&env_dir.join(name)) {
                let _ = mc.apply_to(&mut env).await;
            }
        }
        let tokens: Vec<UseOverride> = env
            .get("USE")
            .map(|use_str| use_str.split_whitespace().map(UseOverride::parse).collect())
            .unwrap_or_default();
        if !tokens.is_empty() {
            out.push((atom, tokens));
        }
    }
    out
}

/// Expand the `USE_EXPAND:` colon form in `package.use` tokens to interned overrides
///
/// A USE_EXPAND name followed by `:` makes every subsequent value a member
/// of that group, e.g. `cat/pkg L10N: de en` ⇒ `l10n_de l10n_en`. A bare
/// `-*` inside a group clears its live values before the trailing values
/// rebuild it (`PYTHON_TARGETS: -* python2_7` ⇒ only
/// `python_targets_python2_7`).
///
/// Only keys present in `use_expand` start a group; any other token — including
/// one that merely ends in `:` — is parsed as an ordinary flag, so plain flags
/// and a bare `-*` keep working.
/// A bare `-*` (not inside a `KEY:` group) is a whole-USE clear-all.
fn flags_have_use_clear_all(flags: &[String], use_expand: &[String]) -> bool {
    let mut in_group = false;
    for tok in flags {
        if let Some(key) = tok.strip_suffix(':')
            && use_expand.iter().any(|k| k == key)
        {
            in_group = true;
            continue;
        }
        if !in_group && tok == "-*" {
            return true;
        }
    }
    false
}

fn expand_use_expand_colon(
    tokens: &[String],
    use_expand: &[String],
    expand_values: &dyn Fn(&str) -> Vec<String>,
) -> Vec<UseOverride> {
    let mut out: Vec<UseOverride> = Vec::with_capacity(tokens.len());
    let mut group: Option<&str> = None;
    for tok in tokens {
        if let Some(key) = tok.strip_suffix(':')
            && use_expand.iter().any(|k| k == key)
        {
            group = Some(key);
            continue;
        }
        let Some(key) = group else {
            out.push(UseOverride::parse(tok));
            continue;
        };
        let prefix = key.to_lowercase();
        if tok == "-*" {
            // Clear the group's live values; the tokens after rebuild it.
            for v in expand_values(key) {
                out.push(UseOverride {
                    flag: Interned::intern(&format!("{prefix}_{v}")),
                    enable: false,
                });
            }
        } else {
            let (enable, val) = match tok.strip_prefix('-') {
                Some(v) => (false, v),
                None => (true, tok.as_str()),
            };
            out.push(UseOverride {
                flag: Interned::intern(&format!("{prefix}_{val}")),
                enable,
            });
        }
    }
    out
}

/// Load a `package.mask`-format atom list as `(is_removal, atom)` lines
fn load_atom_lines(path: &str) -> Vec<(bool, ConfigAtom)> {
    let mut result = Vec::new();
    for line in portage_repo::read_config_lines(path).unwrap_or_default() {
        let (removal, atom) = match line.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, line.as_str()),
        };
        if let Some(atom) = parse_config_atom(path, atom) {
            result.push((removal, atom));
        }
    }
    result
}

/// Parse the atom of a `path` line, warning instead of failing on one em cannot read
///
/// A build-id atom selects one binary package and never an ebuild, and em applies this
/// configuration to ebuilds only, so it is dropped.
fn parse_config_atom(path: &str, atom: &str) -> Option<ConfigAtom> {
    let dep = ConfigAtom::parse(atom)
        .inspect_err(|e| tracing::warn!("{path}: ignoring unsupported atom {atom}: {e}"))
        .ok()?;
    if let ConfigAtom::Dep(d) = &dep
        && d.pinned_build().is_some()
    {
        tracing::warn!("{path}: ignoring {atom}: a build id matches only a binary package");
        return None;
    }
    Some(dep)
}

/// Stack site atom lines onto `acc`: `-atom` drops matching atoms from every earlier layer,
/// and one without `::repo` also drops that atom's repo-scoped forms
fn stack_user_atoms(acc: &mut Vec<ConfigAtom>, lines: Vec<(bool, ConfigAtom)>) {
    for (removal, atom) in lines {
        if !removal {
            acc.push(atom);
            continue;
        }
        acc.retain(|a| match atom.repo() {
            Some(_) => *a != atom,
            None => a.without_repo() != atom,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AcceptToken, PendingConfig, PendingProfile, ProfileLayer, UseEnv, build_use_env,
        expand_use_expand_colon, flags_have_use_clear_all, load_atom_tokens, load_package_env_use,
    };
    use portage_atom::{ConfigAtom, Cpv, Dep};
    use portage_atom_pubgrub::UseOverride;

    fn make_test_repo(dir: &std::path::Path) -> portage_repo::RepoSet {
        std::fs::create_dir_all(dir.join("metadata")).unwrap();
        std::fs::write(dir.join("metadata/layout.conf"), "").unwrap();
        std::fs::create_dir_all(dir.join("profiles")).unwrap();
        portage_repo::RepoSet::single(
            portage_repo::Repository::builder()
                .in_memory_cache()
                .open(dir)
                .unwrap(),
        )
    }

    // Regression: `em crossdev --setup -p` on a target that has never been
    // `--init-target`'d for real has no `etc/portage/make.profile`/
    // `make.conf` on disk yet. Without a pending profile, resolving
    // against that root must fail exactly the way it used to (proving this
    // is a real repro, not a vacuous pass); with one, it must succeed using
    // the real profile dir plus the synthesized make.conf content.
    #[tokio::test]
    async fn sysroot_override_lets_a_cold_target_resolve() {
        let repo_dir = tempfile::tempdir().unwrap();
        let repo = make_test_repo(repo_dir.path());
        let profile_dir = repo_dir.path().join("test-profile");
        std::fs::create_dir_all(&profile_dir).unwrap();
        std::fs::write(profile_dir.join("make.defaults"), "").unwrap();
        let profile_dir = camino::Utf8Path::from_path(&profile_dir).unwrap();

        // A sysroot that has never been `--init-target`'d: no `etc/portage/*`
        // at all under `root`.
        let sysroot = tempfile::tempdir().unwrap();
        let root = camino::Utf8Path::from_path(sysroot.path()).unwrap();

        match build_use_env(&repo, Some(root), None, None, None).await {
            Ok(_) => {
                panic!("a cold, never-initialized target must not resolve without an override")
            }
            Err(e) => assert!(
                e.to_string().contains("cannot resolve"),
                "expected the make.profile canonicalize failure, got: {e}"
            ),
        }

        let pending = PendingConfig {
            profile: Some(PendingProfile {
                parents: vec![profile_dir.to_owned()],
                layer: None,
            }),
            make_conf: Some("CHOST=\"riscv64-unknown-linux-gnu\"\n".into()),
            ..Default::default()
        };
        build_use_env(&repo, Some(root), None, None, Some(&pending))
            .await
            .expect("a pending profile must let a never-initialized target resolve");
    }

    #[tokio::test]
    async fn a_pending_derived_profile_layer_applies_like_its_files() {
        let repo_dir = tempfile::tempdir().unwrap();
        let repo = make_test_repo(repo_dir.path());
        let profile_dir = repo_dir.path().join("test-profile");
        std::fs::create_dir_all(&profile_dir).unwrap();
        std::fs::write(profile_dir.join("make.defaults"), "").unwrap();
        let parents = vec![camino::Utf8PathBuf::try_from(profile_dir).unwrap()];
        let sysroot = tempfile::tempdir().unwrap();
        let root = camino::Utf8Path::from_path(sysroot.path()).unwrap();
        let layer = ProfileLayer {
            package_mask: vec![Dep::parse("sys-devel/gcc").unwrap()],
            package_unmask: vec![Dep::parse("sys-devel/gcc:15").unwrap()],
            package_use: vec![(
                Dep::parse("llvm-runtimes/libunwind").unwrap(),
                vec!["static-libs".into()],
            )],
        };
        let pending = PendingConfig {
            profile: Some(PendingProfile {
                parents,
                layer: Some(layer.clone()),
            }),
            ..Default::default()
        };

        let env = build_use_env(&repo, Some(root), None, None, Some(&pending))
            .await
            .unwrap();
        let atom = |dep: &Dep| ConfigAtom::Dep(dep.clone());
        assert!(env.package_mask.contains(&atom(&layer.package_mask[0])));
        assert!(env.package_unmask.contains(&atom(&layer.package_unmask[0])));
        let leaf = env.profile_package_use.last().unwrap();
        assert_eq!(leaf.package_use[0].0, atom(&layer.package_use[0].0));
    }

    #[tokio::test]
    async fn pending_config_applies_like_its_files() {
        let repo_dir = tempfile::tempdir().unwrap();
        let repo = make_test_repo(repo_dir.path());
        let profile_dir = repo_dir.path().join("test-profile");
        std::fs::create_dir_all(&profile_dir).unwrap();
        std::fs::write(profile_dir.join("make.defaults"), "").unwrap();
        let parents = vec![camino::Utf8PathBuf::try_from(profile_dir).unwrap()];
        let root_dir = tempfile::tempdir().unwrap();
        let root = camino::Utf8Path::from_path(root_dir.path()).unwrap();
        let wrappers =
            Dep::parse("=cross-riscv64-unknown-linux-gnu/clang-crossdev-wrappers-23*").unwrap();
        let pending = PendingConfig {
            profile: Some(PendingProfile {
                parents,
                layer: None,
            }),
            make_conf: None,
            package_accept_keywords: vec![(wrappers.clone(), vec![AcceptToken::Any])],
            package_mask: vec![Dep::parse("cross-riscv64-unknown-linux-gnu/gcc").unwrap()],
            package_unmask: vec![Dep::parse("cross-riscv64-unknown-linux-gnu/gcc:15").unwrap()],
        };

        let env = build_use_env(&repo, Some(root), None, None, Some(&pending))
            .await
            .unwrap();
        assert!(
            env.package_accept_keywords
                .iter()
                .any(|(atom, toks)| *atom == ConfigAtom::Dep(wrappers.clone()) && toks.len() == 1)
        );
        let atom = |dep: &Dep| ConfigAtom::Dep(dep.clone());
        assert!(env.package_mask.contains(&atom(&pending.package_mask[0])));
        assert!(
            env.package_unmask
                .contains(&atom(&pending.package_unmask[0]))
        );
    }

    #[tokio::test]
    async fn prefix_overlay_masks_and_unmasks_are_read() {
        let repo_dir = tempfile::tempdir().unwrap();
        let repo = make_test_repo(repo_dir.path());
        let profile_dir = repo_dir.path().join("test-profile");
        std::fs::create_dir_all(&profile_dir).unwrap();
        std::fs::write(profile_dir.join("make.defaults"), "").unwrap();
        let profile_dir = camino::Utf8Path::from_path(&profile_dir).unwrap();
        let sysroot = tempfile::tempdir().unwrap();
        let root = camino::Utf8Path::from_path(sysroot.path()).unwrap();
        let pending = PendingConfig {
            profile: Some(PendingProfile {
                parents: vec![profile_dir.to_owned()],
                layer: None,
            }),
            ..Default::default()
        };

        let overlay = tempfile::tempdir().unwrap();
        std::fs::write(overlay.path().join("package.mask"), ">dev-libs/foo-1\n").unwrap();
        std::fs::write(overlay.path().join("package.unmask"), "=dev-libs/foo-3\n").unwrap();
        let overlay = camino::Utf8Path::from_path(overlay.path()).unwrap();

        let env = build_use_env(&repo, Some(root), Some(overlay), None, Some(&pending))
            .await
            .unwrap();
        let atom = |s: &str| ConfigAtom::parse(s).unwrap();
        assert!(env.package_mask.contains(&atom(">dev-libs/foo-1")));
        assert_eq!(env.package_unmask, [atom("=dev-libs/foo-3")]);
    }

    #[test]
    fn a_build_id_mask_leaves_the_ebuild_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("package.mask");
        std::fs::write(&path, "=cat/pkg-1-3\n>=cat/pkg-2-3\n").unwrap();
        let lines = super::load_atom_lines(path.to_str().unwrap());
        let masks: Vec<_> = lines.iter().map(|(_, d)| d.to_string()).collect();
        assert_eq!(masks, [">=cat/pkg-2-3"]);
    }

    #[tokio::test]
    async fn a_site_removal_cancels_a_repo_level_mask() {
        let repo_dir = tempfile::tempdir().unwrap();
        let profiles = repo_dir.path().join("profiles");
        std::fs::create_dir_all(&profiles).unwrap();
        std::fs::write(profiles.join("repo_name"), "gentoo\n").unwrap();
        std::fs::write(
            profiles.join("package.mask"),
            "dev-libs/foo\ndev-libs/bar\n",
        )
        .unwrap();
        let repos = make_test_repo(repo_dir.path());
        let profile_dir = repo_dir.path().join("test-profile");
        std::fs::create_dir_all(&profile_dir).unwrap();
        std::fs::write(profile_dir.join("make.defaults"), "").unwrap();
        let pending = PendingConfig {
            profile: Some(PendingProfile {
                parents: vec![camino::Utf8PathBuf::try_from(profile_dir).unwrap()],
                layer: None,
            }),
            ..Default::default()
        };
        let root_dir = tempfile::tempdir().unwrap();
        let site = root_dir.path().join("etc/portage");
        std::fs::create_dir_all(&site).unwrap();
        std::fs::write(site.join("package.mask"), "-dev-libs/foo\n").unwrap();
        let root = camino::Utf8Path::from_path(root_dir.path()).unwrap();

        let env = build_use_env(&repos, Some(root), None, None, Some(&pending))
            .await
            .unwrap();
        let masks: Vec<_> = env.package_mask.iter().map(|d| d.to_string()).collect();
        assert_eq!(masks, ["dev-libs/bar::gentoo"]);
    }

    #[tokio::test]
    async fn a_lone_accept_properties_negation_keeps_the_globals_star() {
        let repo_dir = tempfile::tempdir().unwrap();
        let repos = make_test_repo(repo_dir.path());
        let profile_dir = repo_dir.path().join("test-profile");
        std::fs::create_dir_all(&profile_dir).unwrap();
        std::fs::write(profile_dir.join("make.defaults"), "").unwrap();
        let pending = PendingConfig {
            profile: Some(PendingProfile {
                parents: vec![camino::Utf8PathBuf::try_from(profile_dir).unwrap()],
                layer: None,
            }),
            make_conf: Some("ACCEPT_PROPERTIES=\"-interactive\"\n".into()),
            ..Default::default()
        };
        let root_dir = tempfile::tempdir().unwrap();
        let root = camino::Utf8Path::from_path(root_dir.path()).unwrap();

        let env = build_use_env(&repos, Some(root), None, None, Some(&pending))
            .await
            .unwrap();
        assert!(env.accept_properties.accepts("live"));
        assert!(!env.accept_properties.accepts("interactive"));
        assert!(env.accept_restrict.accepts("fetch"));
    }

    /// Resolve with `site` files written into the root's `etc/portage`
    async fn env_with_site(site: &[(&str, &str)]) -> UseEnv {
        let repo_dir = tempfile::tempdir().unwrap();
        let repos = make_test_repo(repo_dir.path());
        let profile_dir = repo_dir.path().join("test-profile");
        std::fs::create_dir_all(&profile_dir).unwrap();
        std::fs::write(profile_dir.join("make.defaults"), "").unwrap();
        let pending = PendingConfig {
            profile: Some(PendingProfile {
                parents: vec![camino::Utf8PathBuf::try_from(profile_dir).unwrap()],
                layer: None,
            }),
            ..Default::default()
        };
        let root_dir = tempfile::tempdir().unwrap();
        let etc = root_dir.path().join("etc/portage");
        std::fs::create_dir_all(&etc).unwrap();
        for (name, body) in site {
            std::fs::write(etc.join(name), body).unwrap();
        }
        let root = camino::Utf8Path::from_path(root_dir.path()).unwrap();
        build_use_env(&repos, Some(root), None, None, Some(&pending))
            .await
            .unwrap()
    }

    fn atoms<T>(entries: &[(ConfigAtom, T)]) -> Vec<String> {
        entries.iter().map(|(a, _)| a.to_string()).collect()
    }

    #[tokio::test]
    async fn wildcard_lines_apply_and_match_all_folds_into_the_global_setting() {
        let env = env_with_site(&[
            ("package.use", "dev-libs/* foo\n*/* bar\n"),
            ("package.accept_keywords", "*/* ~amd64\n"),
            ("package.license", "*/* GPL-2\n"),
            ("package.mask", "dev-libs/*::gentoo\n"),
        ])
        .await;
        assert_eq!(atoms(&env.package_use), ["dev-libs/*"]);
        assert!(
            env.conf
                .mentions(portage_atom::interner::Interned::intern("bar"))
        );
        assert_eq!(atoms(&env.package_accept_keywords), ["*/*"]);
        assert!(env.package_license.is_empty());
        assert!(env.accept_license.accepts("GPL-2"));
        let mask: Vec<_> = env.package_mask.iter().map(|a| a.to_string()).collect();
        assert_eq!(mask, ["dev-libs/*::gentoo"]);
    }

    #[tokio::test]
    async fn the_most_specific_line_applies_last_whatever_the_file_order() {
        let env = env_with_site(&[
            ("package.use", "=dev-libs/foo-1 x\ndev-libs/foo -x\n"),
            (
                "package.license",
                "=dev-libs/foo-1 LIC\ndev-libs/foo -LIC\n",
            ),
        ])
        .await;
        assert_eq!(atoms(&env.package_use), ["dev-libs/foo", "=dev-libs/foo-1"]);
        assert_eq!(
            atoms(&env.package_license),
            ["dev-libs/foo", "=dev-libs/foo-1"]
        );
        let empty = portage_atom_pubgrub::UseLayer::default();
        let cfg = portage_atom_pubgrub::resolve_effective_use(
            &Default::default(),
            &empty,
            &Cpv::parse("dev-libs/foo-1").unwrap(),
            None,
            &env.package_use,
            &empty,
            &[],
            &empty,
        );
        assert_eq!(
            cfg.get(portage_atom::interner::Interned::intern("x")),
            portage_atom_pubgrub::UseFlagState::Enabled
        );
    }

    // Expected override shorthand
    fn ov(s: &str) -> UseOverride {
        UseOverride::parse(s)
    }

    // Build an `etc/portage` dir with a `package.env` and named env files,
    // matching `portage_repo::package_env`'s own test fixture shape
    fn portage_dir(package_env: &str, env_files: &[(&str, &str)]) -> tempfile::TempDir {
        let td = tempfile::TempDir::new().unwrap();
        let pd = td.path();
        std::fs::write(pd.join("package.env"), package_env).unwrap();
        std::fs::create_dir_all(pd.join("env")).unwrap();
        for (name, body) in env_files {
            std::fs::write(pd.join("env").join(name), body).unwrap();
        }
        td
    }

    #[tokio::test]
    async fn package_env_use_becomes_package_use_shaped_overrides() {
        let td = portage_dir(
            "dev-libs/foo  ccache-workaround\n",
            &[("ccache-workaround", "USE=\"${USE} -jit ccache\"\n")],
        );
        let dir = camino::Utf8Path::from_path(td.path()).unwrap();
        let out = load_package_env_use(dir).await;
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, ConfigAtom::parse("dev-libs/foo").unwrap());
        assert_eq!(out[0].1, vec![ov("-jit"), ov("ccache")]);
    }

    #[tokio::test]
    async fn package_env_use_chains_multiple_files_in_line_order() {
        let td = portage_dir(
            "dev-libs/foo  first second\n",
            &[
                ("first", "USE=\"${USE} a\"\n"),
                ("second", "USE=\"${USE} b\"\n"),
            ],
        );
        let dir = camino::Utf8Path::from_path(td.path()).unwrap();
        let out = load_package_env_use(dir).await;
        assert_eq!(out[0].1, vec![ov("a"), ov("b")]);
    }

    #[tokio::test]
    async fn package_env_without_use_assignment_yields_no_override() {
        // An env file that only touches build vars (CFLAGS/FEATURES/etc, the
        // already-landed build-env slice) shouldn't manufacture a USE entry.
        let td = portage_dir(
            "dev-libs/foo  flags-only\n",
            &[("flags-only", "CFLAGS=\"-O3\"\n")],
        );
        let dir = camino::Utf8Path::from_path(td.path()).unwrap();
        let out = load_package_env_use(dir).await;
        assert!(out.is_empty());
    }

    #[test]
    fn colon_form_expands_values() {
        // `cat/pkg L10N: de en` ⇒ l10n_de l10n_en; a plain flag is untouched.
        let keys = ["L10N".to_string()];
        let none = |_: &str| Vec::new();
        let out = expand_use_expand_colon(
            &["foo".into(), "L10N:".into(), "de".into(), "en".into()],
            &keys,
            &none,
        );
        assert_eq!(out, vec![ov("foo"), ov("l10n_de"), ov("l10n_en")]);
    }

    #[test]
    fn colon_form_negates_value() {
        let keys = ["L10N".to_string()];
        let none = |_: &str| Vec::new();
        let out = expand_use_expand_colon(&["L10N:".into(), "-de".into()], &keys, &none);
        assert_eq!(out, vec![ov("-l10n_de")]);
    }

    #[test]
    fn colon_form_dash_star_clears_group_defaults() {
        // `PYTHON_TARGETS: -* python2_7` clears the live defaults (python3_13)
        // then adds python2_7 — the documented "pin a single impl" pattern.
        let keys = ["PYTHON_TARGETS".to_string()];
        let defaults = |k: &str| {
            if k == "PYTHON_TARGETS" {
                vec!["python3_13".to_string()]
            } else {
                Default::default()
            }
        };
        let out = expand_use_expand_colon(
            &["PYTHON_TARGETS:".into(), "-*".into(), "python2_7".into()],
            &keys,
            &defaults,
        );
        assert_eq!(
            out,
            vec![
                ov("-python_targets_python3_13"),
                ov("python_targets_python2_7")
            ]
        );
    }

    #[test]
    fn colon_form_switches_group_mid_line() {
        // A new `KEY:` switches the active group; both groups expand.
        let keys = ["L10N".to_string(), "PYTHON_TARGETS".to_string()];
        let none = |_: &str| Vec::new();
        let out = expand_use_expand_colon(
            &[
                "L10N:".into(),
                "de".into(),
                "PYTHON_TARGETS:".into(),
                "python3_13".into(),
            ],
            &keys,
            &none,
        );
        assert_eq!(out, vec![ov("l10n_de"), ov("python_targets_python3_13")]);
    }

    #[test]
    fn colon_form_ignores_unknown_key() {
        // A `:` suffix on something that isn't a USE_EXPAND key is parsed as a
        // plain flag (left intact, interned verbatim).
        let keys = ["L10N".to_string()];
        let none = |_: &str| Vec::new();
        let out = expand_use_expand_colon(&["WEIRD:".into(), "x".into()], &keys, &none);
        assert_eq!(out, vec![ov("WEIRD:"), ov("x")]);
    }

    #[test]
    fn colon_form_passes_through_plain_flags() {
        let keys = ["L10N".to_string()];
        let none = |_: &str| Vec::new();
        let out = expand_use_expand_colon(&["nls".into(), "-debug".into()], &keys, &none);
        assert_eq!(out, vec![ov("nls"), ov("-debug")]);
    }

    #[test]
    fn bare_star_is_a_use_clear_all_colon_star_is_not() {
        let keys = ["PYTHON_TARGETS".to_string()];
        assert!(flags_have_use_clear_all(
            &["-*".into(), "build".into()],
            &keys
        ));
        assert!(!flags_have_use_clear_all(
            &["PYTHON_TARGETS:".into(), "-*".into(), "python3_13".into()],
            &keys
        ));
    }

    // PMS 5.2.4 dir-form: a `/etc/portage/package.use` *directory*'s regular files are concatenated in filename order — and, matching real portage's `_recursive_basename_filter`, dotfiles and `~` editor backups are skipped
    //
    // Before the shared-`read_config_lines` fix these loaders read every regular
    // file, so a stray `.swp`/`foo~` that happened to parse as `atom flag` leaked
    // in as real data that real portage ignores.
    #[test]
    fn package_use_dir_skips_dotfiles_and_backups() {
        let td = tempfile::TempDir::new().unwrap();
        let dir = td.path().join("package.use");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("10-a"), "dev-libs/a first\n").unwrap();
        std::fs::write(dir.join("20-b"), "# c\ndev-libs/b second\n").unwrap();
        // Both must be ignored the way real portage ignores them.
        std::fs::write(dir.join(".hidden"), "dev-libs/hidden ghost\n").unwrap();
        std::fs::write(dir.join("30-b~"), "dev-libs/backup ghost\n").unwrap();

        let out = load_atom_tokens(dir.to_str().unwrap());
        let atoms: Vec<_> = out.iter().map(|(d, _)| d.to_string()).collect();
        assert_eq!(atoms, vec!["dev-libs/a", "dev-libs/b"]);
        // Filename order preserved across the concatenation.
        assert_eq!(out[0].1, vec!["first".to_string()]);
        assert_eq!(out[1].1, vec!["second".to_string()]);
    }

    #[test]
    fn package_use_dir_reads_nested_subdirectories() {
        let td = tempfile::TempDir::new().unwrap();
        let dir = td.path().join("package.use");
        std::fs::create_dir_all(dir.join("20-sub")).unwrap();
        std::fs::write(dir.join("10-a"), "dev-libs/a x\n").unwrap();
        std::fs::write(dir.join("20-sub/b"), "dev-libs/b y\n").unwrap();
        std::fs::write(dir.join("30-c"), "dev-libs/c z\n").unwrap();

        let out = load_atom_tokens(dir.to_str().unwrap());
        let atoms: Vec<_> = out.iter().map(|(d, _)| d.to_string()).collect();
        assert_eq!(atoms, vec!["dev-libs/a", "dev-libs/b", "dev-libs/c"]);
    }
}
