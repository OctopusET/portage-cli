//! The [`Settings`] of the config root a command selects, and the variables Portage derives
//! from the roots and repositories rather than from any config file

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use portage_repo::{
    ConfSource, EbuildShell, Environment, ProfileStack, Repository, Settings, SettingsSources,
};
use portage_resolve::Roots;

use crate::cli::Cli;

/// Portage's defaults for a host install; a prefix has none of its own yet
const MAKE_GLOBALS: &str = "/usr/share/portage/config/make.globals";

/// Stands in for an absent make.globals' FEATURES: the ones em always acted on, so make.conf can
/// still switch them off
const EM_FEATURES: &str = "binpkg-multi-instance preserve-libs";

/// The profile stack of `config_root`, its user profiles included; `None` without a `make.profile`
pub(crate) fn profile_stack(
    config_root: &Utf8Path,
    config_overlay: Option<&Utf8Path>,
) -> Result<Option<ProfileStack>> {
    let Ok(profile_path) =
        std::fs::canonicalize(config_root.join("etc/portage/make.profile").as_std_path())
    else {
        return Ok(None);
    };
    let mut stack = ProfileStack::build_in(profile_path, config_root)
        .context("building profile stack")?
        .with_user_profile(config_root.join("etc/portage/profile").into_std_path_buf())
        .context("loading the user profile")?;
    // `--prefix`'s config overlay (`<prefix>/etc/portage/profile`) layers on
    // the host profile, same as its `package.use`/`bashrc`. Site
    // `use.force`/`use.mask` there have to reach ebuild phases.
    if let Some(overlay) = config_overlay {
        stack = stack
            .with_user_profile(overlay.join("profile").into_std_path_buf())
            .context("loading the prefix overlay profile")?;
    }
    Ok(Some(stack))
}

/// make.conf(5) files of `config_root`: each a file or a Flat directory of fragments, the legacy
/// `etc/make.conf` first so `etc/portage/make.conf` overrides it, as in Portage
pub(crate) fn make_conf_files(config_root: &Utf8Path) -> Result<Vec<std::path::PathBuf>> {
    portage_repo::expand_make_conf_paths([
        config_root.join("etc/make.conf"),
        config_root.join("etc/portage/make.conf"),
    ])
    .context("listing make.conf")
}

/// What decides one [`Settings`]: the files it reads and whether make.globals applies
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct Key {
    config_root: Utf8PathBuf,
    config_overlay: Option<Utf8PathBuf>,
    /// Where `etc/profile.env` is read
    eroot: Utf8PathBuf,
    /// make.globals is the host install's; a prefix has none of its own yet
    host_globals: bool,
}

impl Key {
    /// The settings of a build or a query against `config_root`, merging into `eroot`
    pub(crate) fn new(
        config_root: Option<&Utf8Path>,
        config_overlay: Option<&Utf8Path>,
        eroot: &Utf8Path,
        eprefix: Option<&Utf8Path>,
    ) -> Self {
        Self {
            config_root: config_root.unwrap_or(Utf8Path::new("/")).to_owned(),
            config_overlay: config_overlay.map(Utf8Path::to_owned),
            eroot: eroot.to_owned(),
            host_globals: eprefix.is_none(),
        }
    }

    /// The key of the config root `roots` selects
    pub(crate) fn of(roots: &Roots) -> Self {
        Self::new(
            roots.config(),
            roots.config_overlay(),
            roots.merge_root(),
            roots.eprefix(),
        )
    }
}

type Cache = Mutex<HashMap<Key, Option<Arc<Settings>>>>;

/// Settings are evaluated once per [`Key`] per invocation
static CACHE: LazyLock<Cache> = LazyLock::new(Cache::default);

/// The settings `key` names; `None` when its config root has no profile
///
/// `repo` only provides the shell the files are evaluated in.
pub(crate) async fn cached(key: Key, repo: &Repository) -> Result<Option<Arc<Settings>>> {
    if let Some(hit) = CACHE.lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
        return Ok(hit.clone());
    }
    let settings = evaluate(&key, repo).await?.map(Arc::new);
    CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key, settings.clone());
    Ok(settings)
}

async fn evaluate(key: &Key, repo: &Repository) -> Result<Option<Settings>> {
    let Some(stack) = profile_stack(&key.config_root, key.config_overlay.as_deref())? else {
        return Ok(None);
    };
    let confs = make_conf_files(&key.config_root)?;
    let confs: Vec<ConfSource> = confs.iter().map(|p| ConfSource::File(p)).collect();
    let env_d = key.eroot.join("etc/profile.env");
    let globals = Utf8Path::new(MAKE_GLOBALS);
    let em_globals = format!("FEATURES=\"{EM_FEATURES}\"\n");
    let env = Environment::process();
    let mut shell = repo.shell().await.context("creating shell")?;
    let settings = Settings::evaluate(
        &mut shell,
        SettingsSources {
            stack: &stack,
            env_d: env_d
                .is_file()
                .then(|| ConfSource::File(env_d.as_std_path())),
            globals: Some(if key.host_globals && globals.is_file() {
                ConfSource::File(globals.as_std_path())
            } else {
                ConfSource::Str(&em_globals)
            }),
            confs: &confs,
            env: &env,
        },
    )
    .await
    .context("evaluating the configuration")?;
    Ok(Some(settings))
}

/// The settings of the config root `cli` selects; `None` when it has no profile
pub(crate) async fn load(cli: &Cli) -> Result<Option<Arc<Settings>>> {
    cached(Key::of(&cli.roots()), &crate::crossdev::main_repo(cli)?).await
}

/// Variables a build shell takes from the settings, because the profile and make.conf it
/// sources do not hold Portage's whole value: make.globals', env.d's and the environment's parts
///
/// em's own merge, fetch and elog decisions read them, as Portage's do, so the environment
/// counts here although it does not otherwise reach builds.
const SEEDED: &[&str] = &[
    "CONFIG_PROTECT",
    "CONFIG_PROTECT_MASK",
    "FEATURES",
    "GENTOO_MIRRORS",
    "PORTAGE_ELOG_CLASSES",
    "PORTAGE_ELOG_SYSTEM",
    "UNINSTALL_IGNORE",
];

/// `FEATURES` em acts on when the configuration sets them; the rest never reach a build, so an
/// ebuild testing for a sandbox em lacks does not assume it
const HONOURED_FEATURES: &[&str] = &[
    "binpkg-docompress",
    "binpkg-dostrip",
    "binpkg-ignore-signature",
    "binpkg-multi-instance",
    "binpkg-request-signature",
    "binpkg-signing",
    "buildpkg",
    "buildpkg-live",
    "config-protect-if-modified",
    "digest",
    "distlocks",
    "keeptemp",
    "keepwork",
    "noclean",
    "nodoc",
    "noinfo",
    "noman",
    "nostrip",
    "pkgdir-index-trusted",
    "preserve-libs",
    "protect-owned",
    "test",
    "unmerge-orphans",
];

/// The `FEATURES` a build sees: the honoured switches `configured` sets, and `userpriv` when the
/// build does not run as root
pub(crate) fn build_features(configured: &str, as_root: bool) -> String {
    let mut out: Vec<&str> = configured
        .split_whitespace()
        .filter(|f| HONOURED_FEATURES.contains(f))
        .collect();
    out.extend((!as_root).then_some("userpriv"));
    out.sort_unstable();
    out.dedup();
    out.join(" ")
}

/// Whether the FEATURES value `features` lists `name`
pub(crate) fn has_feature(features: &str, name: &str) -> bool {
    features.split_whitespace().any(|f| f == name)
}

/// The FEATURES em acts on: the settings' own, or [`EM_FEATURES`] without settings (no profile)
pub(crate) fn features(settings: Option<&Settings>) -> &str {
    settings.map_or(EM_FEATURES, |s| s.get("FEATURES").unwrap_or_default())
}

/// Give a build shell the [`SEEDED`] values and the stacked `FEATURES` (make.globals included)
/// for `package.env` to stack onto; without settings (no profile), em's own default features
pub(crate) async fn seed_build_shell(
    shell: &mut EbuildShell,
    settings: Option<&Settings>,
) -> Result<()> {
    let mut script: String = SEEDED
        .iter()
        .filter_map(|&name| {
            let value = settings?.get(name)?;
            Some(format!("export {name}={}\n", shell_quote(value)))
        })
        .collect();
    if settings.is_none() {
        let current = shell.get_var("FEATURES").unwrap_or_default();
        let mut words: Vec<&str> = current.split_whitespace().collect();
        words.extend(
            EM_FEATURES
                .split_whitespace()
                .filter(|f| !has_feature(&current, f)),
        );
        script += &format!("export FEATURES={}\n", shell_quote(&words.join(" ")));
    }
    shell
        .run_string(&script)
        .await
        .context("seeding the build configuration")
}

/// Replace the shell's `FEATURES` with [`build_features`], and return it
pub(crate) async fn export_provided_features(shell: &mut EbuildShell) -> Result<String> {
    let configured = shell.get_var("FEATURES").unwrap_or_default();
    let features = build_features(&configured, rustix::process::getuid().is_root());
    shell
        .run_string(&format!("export FEATURES={}\n", shell_quote(&features)))
        .await
        .context("setting FEATURES")?;
    Ok(features)
}

pub(crate) fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// The variables Portage derives instead of reading: the roots, and the repositories as
/// PORTDIR/PORTDIR_OVERLAY
pub(crate) fn derived(roots: &Roots) -> Result<Vec<(&'static str, String)>> {
    let slash = |path: &Utf8Path| {
        let path = path.as_str().trim_end_matches('/');
        format!("{path}/")
    };
    let eprefix = roots.eprefix().map_or(String::new(), |p| {
        p.as_str().trim_end_matches('/').to_owned()
    });
    let merge_root = roots.merge_root().as_str().trim_end_matches('/');
    let root = merge_root
        .strip_suffix(eprefix.as_str())
        .unwrap_or(merge_root);
    let root = slash(Utf8Path::new(root));
    let sysroot = roots.build_sysroot().map_or_else(|| "/".to_owned(), slash);
    let broot = roots
        .broot()
        .map(|p| p.as_str().trim_end_matches('/').to_owned())
        .unwrap_or_default();
    let repos = roots.repos_conf().context("reading repos.conf")?;
    let (portdir, overlays) = repo_locations(&repos);
    let mut vars = vec![
        ("ROOT", root.clone()),
        ("EPREFIX", eprefix.clone()),
        (
            "EROOT",
            slash(Utf8Path::new(&format!(
                "{}{eprefix}",
                root.trim_end_matches('/')
            ))),
        ),
        ("SYSROOT", sysroot.clone()),
        (
            "ESYSROOT",
            slash(Utf8Path::new(&format!(
                "{}{eprefix}",
                sysroot.trim_end_matches('/')
            ))),
        ),
        ("BROOT", broot),
        (
            "PORTAGE_CONFIGROOT",
            slash(roots.config().unwrap_or(Utf8Path::new("/"))),
        ),
        ("PORTDIR_OVERLAY", overlays),
    ];
    vars.extend(portdir.map(|p| ("PORTDIR", p)));
    Ok(vars)
}

/// `PORTDIR` and `PORTDIR_OVERLAY` as Portage derives them: the main repo's location, and every
/// other repo's, space-separated
fn repo_locations(repos: &portage_repo::ReposConf) -> (Option<String>, String) {
    // Portage's own repos.conf names `gentoo` the main repo when a config root does not.
    let main_repo = repos.main_repo().or_else(|| repos.find("gentoo"));
    let main = main_repo.map(|r| r.name);
    let location =
        |r: &portage_repo::RepoEntry| r.location.as_path().map(|p| p.display().to_string());
    let overlays: Vec<String> = repos
        .repos()
        .iter()
        .filter(|r| Some(r.name) != main)
        .filter_map(location)
        .collect();
    (main_repo.and_then(location), overlays.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn without_make_globals_make_conf_switches_off_the_always_on_features() {
        let dir = tempfile::tempdir().unwrap();
        let base = Utf8Path::from_path(dir.path()).unwrap();
        let repo_dir = base.join("repo");
        std::fs::create_dir_all(repo_dir.join("metadata")).unwrap();
        std::fs::write(repo_dir.join("metadata/layout.conf"), "").unwrap();
        std::fs::create_dir_all(repo_dir.join("profiles/test")).unwrap();
        let repo = Repository::builder()
            .in_memory_cache()
            .open(repo_dir.as_std_path())
            .unwrap();
        let features = async |make_conf: &str| {
            let root = base.join(format!("root{}", make_conf.len()));
            let etc = root.join("etc/portage");
            std::fs::create_dir_all(&etc).unwrap();
            std::os::unix::fs::symlink(repo_dir.join("profiles/test"), etc.join("make.profile"))
                .unwrap();
            std::fs::write(etc.join("make.conf"), make_conf).unwrap();
            let key = Key::new(Some(&root), None, &root, Some(&root));
            let settings = evaluate(&key, &repo).await.unwrap().unwrap();
            settings.get("FEATURES").unwrap_or_default().to_owned()
        };
        assert_eq!(features("").await, "binpkg-multi-instance preserve-libs");
        assert_eq!(
            features("FEATURES=\"-preserve-libs\"\n").await,
            "binpkg-multi-instance"
        );
    }

    #[test]
    fn without_a_profile_em_still_acts_on_its_default_features() {
        assert!(has_feature(features(None), "preserve-libs"));
        assert!(has_feature(features(None), "binpkg-multi-instance"));
    }

    #[test]
    fn a_build_sees_only_the_features_em_provides() {
        let stacked = "ccache sandbox network-sandbox test keepwork distcc preserve-libs \
                       binpkg-multi-instance unmerge-orphans";
        assert_eq!(
            build_features(stacked, false),
            "binpkg-multi-instance keepwork preserve-libs test unmerge-orphans userpriv"
        );
        assert_eq!(build_features("test", true), "test");
    }

    #[test]
    fn portdir_is_the_main_repo_and_portdir_overlay_the_rest() {
        let root = tempfile::tempdir().unwrap();
        let conf = root.path().join("etc/portage/repos.conf");
        std::fs::create_dir_all(&conf).unwrap();
        std::fs::write(
            conf.join("repos.conf"),
            "[DEFAULT]\nmain-repo = gentoo\n\n[gentoo]\nlocation = /r/gentoo\n\n\
             [guru]\nlocation = /r/guru\n\n[local]\nlocation = /r/local\n",
        )
        .unwrap();
        let repos = portage_repo::ReposConf::load_rooted(
            camino::Utf8Path::from_path(root.path()).unwrap(),
            &[],
        )
        .unwrap();
        let (portdir, overlays) = repo_locations(&repos);
        assert_eq!(portdir.as_deref(), Some("/r/gentoo"));
        let mut overlays: Vec<&str> = overlays.split(' ').collect();
        overlays.sort();
        assert_eq!(overlays, ["/r/guru", "/r/local"]);
    }
}
