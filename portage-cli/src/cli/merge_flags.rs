//! Merge-behavior flags: everything `emerge_atoms`/`run_merge_plan` read to
//! decide *how* to resolve and build, as opposed to root-model flags
//! ([`super::Topology`]/[`super::RootArg`]) or depgraph-shape flags
//! ([`super::DepgraphFlags`]).
//!
//! Flattened onto each merge-shaped applet (`EmergeArgs`, `CrossdevArgs`,
//! `ToolchainArgs`, `StagesArgs`, `SetupArgs`, `Revdep`, `Depclean`). Not
//! global: `-a` must not reach `use`. Prefix-position `em -uD @world` reaches
//! these via the emerge retry, not a second flatten on [`super::Cli`].
//!
//! `--search`/`--searchdesc`/`--nodeps` are not here: they select a different
//! emerge *action* ([`super::EmergeModeArgs`]), or a per-step `EmergeOpts`
//! override in the staged bootstrap.

use std::ffi::OsString;

#[derive(
    winnow_args::Args, Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize,
)]
#[serde(default)]
#[arg(
    next_help_heading = "Merge",
    heading("Merge", help = "How the solver and build scheduler behave.")
)]
pub struct MergeFlags {
    /// Ask for confirmation before performing actions
    #[arg(short = 'a', long)]
    pub ask: bool,

    /// Update installed packages to newest available versions
    #[arg(short = 'u', long)]
    pub update: bool,

    /// Write required USE changes to /etc/portage/package.use/
    #[arg(long)]
    pub autounmask_write: bool,

    /// Build and install packages but do not add them to the world file
    #[arg(short = '1', long = "oneshot")]
    pub oneshot: bool,

    /// Only fetch distfiles, do not build or install
    #[arg(short = 'f', long)]
    pub fetchonly: bool,

    /// Instead of building, just fetch every SRC_URI file (regardless of
    /// USE setting) for the resolved packages.
    #[arg(short = 'F', long)]
    pub fetch_all_uri: bool,

    /// Build binary packages for all merged packages
    ///
    /// `--buildpkg=n` overrides `FEATURES=buildpkg`.
    #[arg(long, require_equals, default_missing = "y", value_name = "y|n")]
    #[serde(deserialize_with = "saved_switch")]
    pub buildpkg: Option<YesNo>,

    /// Same as `--buildpkg`
    // A separate switch because a short flag never takes a value in emerge: `-bk` is `-b -k`.
    #[arg(short = 'b', overrides("--buildpkg"))]
    #[serde(skip)]
    pub(crate) buildpkg_short: bool,

    /// Space-separated atoms never to build binary packages for, whatever enables it
    #[arg(long, value_name = "ATOMS")]
    pub buildpkg_exclude: Vec<String>,

    /// Build binary packages without merging/installing them
    ///
    /// All build-time dependencies must already be satisfied on the system -- this does not
    /// resolve or install anything to make that true.
    #[arg(short = 'B', long)]
    pub buildpkgonly: bool,

    /// Use binary packages if available, otherwise fall back to source
    #[arg(short = 'k', long)]
    pub usepkg: bool,

    /// Only use binary packages, fail if none available
    #[arg(short = 'K', long)]
    pub usepkgonly: bool,

    /// Fetch binary packages for all requested packages
    ///
    /// `--getbinpkg=n` overrides `FEATURES=getbinpkg`.
    #[arg(long, require_equals, default_missing = "y", value_name = "y|n")]
    #[serde(deserialize_with = "saved_switch")]
    pub getbinpkg: Option<YesNo>,

    /// Same as `--getbinpkg`
    #[arg(short = 'g', overrides("--getbinpkg"))]
    #[serde(skip)]
    pub(crate) getbinpkg_short: bool,

    /// Only fetch binary packages, do not install
    #[arg(short = 'G', long)]
    pub getbinpkgonly: bool,

    /// Treat every atom as not-yet-installed, rebuilding the whole dependency
    /// tree from scratch rather than only what is missing or outdated.
    #[arg(short = 'e', long)]
    pub emptytree: bool,

    /// Show the dependency tree, indenting each package under the one that
    /// pulled it in, before merging.
    #[arg(short = 't', long)]
    pub tree: bool,

    /// Emit the depgraph as machine-parsable JSON instead of pretend text
    ///
    /// Takes precedence over `--tree`. Works with `-p` (including `-e`).
    #[arg(long)]
    pub json: bool,

    /// Only merge dependencies, not the specified packages themselves
    #[arg(short = 'o', long)]
    pub onlydeps: bool,

    /// Do not replace installed packages that are already the same version
    #[arg(short = 'n', long)]
    pub noreplace: bool,

    /// Build up to N packages in parallel, respecting build-dependency order (merges are still
    /// serialised)
    ///
    /// Default 1 (sequential).
    #[arg(short = 'j', long, value_name = "N")]
    pub jobs: Option<u32>,

    /// Maximum 1-minute load average allowed when starting additional parallel builds (`--jobs`
    /// > 1)
    ///
    /// Once at least one job is running, further starts wait until load drops below LOAD
    /// (matches real Portage's own load-average throttling). The first concurrent job is
    /// always allowed. Displayed on the `Jobs:` status line regardless.
    #[arg(short = 'l', long, value_name = "LOAD")]
    pub load_average: Option<f64>,

    /// Continue merging as much as possible even if some packages fail
    #[arg(
        long,
        warning = "Exists for portage parity; do not use it. A failed package must stop the run."
    )]
    pub keep_going: bool,

    /// Accepted so existing command lines keep parsing. Config writes follow
    /// `--autounmask-write` and whether this run is pretend or `--ask`; this
    /// flag is not consulted.
    #[arg(long)]
    pub autounmask: bool,

    /// Let the solver choose USE flags to satisfy REQUIRED_USE (Level C) rather than only
    /// reporting violations
    ///
    /// Off by default; flips are reported.
    #[arg(long)]
    pub autosolve_use: bool,

    /// With `-p`/`--pretend` or `-a`/`--ask`, print an "Expected time of
    /// completion" for the plan alongside the merge list, estimated from
    /// activity history (median of recent successful merges per package;
    /// wall uses the build graph + `--jobs` when blockers are available).
    /// Shown even when the plan needs USE/mask changes to proceed.
    #[arg(long = "eta")]
    pub eta: bool,

    /// With `-u`/`--update` `-D`/`--deep`: when moving a version-pinned
    /// family (e.g. upgrading `llvm` pulls `clang` along) would leave a
    /// retained package's pin broken (e.g. `lldb` still pinned to the old
    /// `llvm`), pull that package into the plan too instead of stopping
    /// halfway. Off by default: this can revert the upgrade instead if the
    /// retained package has no version satisfying the new pin.
    #[arg(long)]
    pub complete_graph: bool,

    /// Follow the build-time dependencies (DEPEND, BDEPEND) of installed packages that are kept
    ///
    /// With `--deep`, their upgrades and missing build tools enter the plan; `em depclean` keeps
    /// them installed. Unset, it is on for depclean, and for installs unless binary packages are
    /// used or `--with-bdeps-auto n` is given.
    #[arg(long, value_name = "y|n")]
    pub with_bdeps: Option<YesNo>,

    /// `n`: an install does not turn `--with-bdeps` on by itself
    #[arg(long, value_name = "y|n")]
    pub with_bdeps_auto: Option<YesNo>,

    /// Rebuild installed packages whose ebuild's dependencies changed since they were built
    ///
    /// Compares the same version's dependencies under the installed USE, ignoring slot-operator
    /// bindings; build-time classes count unless `--with-bdeps` is off (by default with binary packages).
    #[arg(long, require_equals, default_missing = "y", value_name = "y|n")]
    pub changed_deps: Option<YesNo>,

    /// Rebuild installed packages whose ebuild's SLOT or sub-slot changed since they were built
    #[arg(long, require_equals, default_missing = "y", value_name = "y|n")]
    pub changed_slot: Option<YesNo>,

    /// Exclude the specified atom from being merged
    #[arg(short = 'X', long, value_name = "ATOM")]
    pub exclude: Vec<String>,

    /// Only require RDEPEND (not DEPEND) to be satisfied in the merge target.
    /// Work-around for cross-compilation bootstrap: a still-empty target sysroot
    /// cannot yet satisfy plain DEPEND (e.g. virtual/os-headers, acct-group/root)
    /// while its own toolchain is being built. `em crossdev --setup` always applies
    /// this unconditionally; elsewhere it defaults off.
    #[arg(long = "root-deps")]
    pub root_deps: bool,
}

/// A `y`/`n` flag value
#[derive(Clone, Copy, Debug, PartialEq, Eq, winnow_args::ValueEnum, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum YesNo {
    /// Yes
    Y,
    /// No
    N,
}

/// Also reads the plain booleans older resume files saved for `with_bdeps`
impl<'de> serde::Deserialize<'de> for YesNo {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(serde::Deserialize)]
        #[serde(untagged)]
        enum Saved {
            Bool(bool),
            Text(String),
        }
        match Saved::deserialize(deserializer)? {
            Saved::Bool(true) => Ok(Self::Y),
            Saved::Bool(false) => Ok(Self::N),
            Saved::Text(t) if t == "y" => Ok(Self::Y),
            Saved::Text(t) if t == "n" => Ok(Self::N),
            Saved::Text(t) => Err(serde::de::Error::custom(format!(
                "expected y or n, got {t}"
            ))),
        }
    }
}

/// A `y`/`n` value, or a plain boolean from older resume files, where `false` meant not given
fn saved_switch<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<YesNo>, D::Error> {
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum Saved {
        Unset(Option<bool>),
        Value(YesNo),
    }
    Ok(match <Saved as serde::Deserialize>::deserialize(d)? {
        Saved::Unset(Some(true)) => Some(YesNo::Y),
        Saved::Unset(_) => None,
        Saved::Value(v) => Some(v),
    })
}

/// Long options whose `y`/`n` value may also be the next word, as in emerge's `--buildpkg n`
const DETACHED_YES_NO: [&str; 2] = ["--buildpkg", "--getbinpkg"];

/// `args` with a detached `y` or `n` after a [`DETACHED_YES_NO`] option attached to it
///
/// Any other next word is left alone, so a bare `--buildpkg` can still precede an atom.
pub fn attach_yes_no_values(args: Vec<OsString>) -> Vec<OsString> {
    let mut out = Vec::with_capacity(args.len());
    let mut words = args.into_iter().peekable();
    while let Some(word) = words.next() {
        if word == "--" {
            out.push(word);
            out.extend(words);
            break;
        }
        let optional = DETACHED_YES_NO.iter().any(|o| word == *o);
        match words.next_if(|next| optional && (next == "y" || next == "n")) {
            Some(value) => {
                let mut joined = word;
                joined.push("=");
                joined.push(value);
                out.push(joined);
            }
            None => out.push(word),
        }
    }
    out
}

impl MergeFlags {
    /// Fold the short `-b`/`-g` switches into `buildpkg`/`getbinpkg`
    pub fn resolve_short_flags(mut self) -> Self {
        if std::mem::take(&mut self.buildpkg_short) {
            self.buildpkg = Some(YesNo::Y);
        }
        if std::mem::take(&mut self.getbinpkg_short) {
            self.getbinpkg = Some(YesNo::Y);
        }
        self
    }

    /// Whether an install follows kept packages' build-time deps: emerge's `--with-bdeps`, `auto`
    /// unless binary packages are in use
    pub fn install_bdeps(&self) -> bool {
        match self.with_bdeps {
            Some(value) => value == YesNo::Y,
            None => {
                let binpkgs =
                    self.usepkg || self.usepkgonly || self.wants_getbinpkg() || self.getbinpkgonly;
                self.with_bdeps_auto != Some(YesNo::N) && !binpkgs
            }
        }
    }

    /// Whether depclean keeps installed packages' build-time deps: all but `--with-bdeps n`
    pub fn depclean_bdeps(&self) -> bool {
        self.with_bdeps != Some(YesNo::N)
    }

    /// `-b`/`--buildpkg`, given or from `FEATURES`
    pub fn wants_buildpkg(&self) -> bool {
        self.buildpkg == Some(YesNo::Y)
    }

    /// `-g`/`--getbinpkg`, given or from `FEATURES`
    pub fn wants_getbinpkg(&self) -> bool {
        self.getbinpkg == Some(YesNo::Y)
    }

    /// Whether built packages are packaged: `--buildpkg`, or `-B`, which implies it as in emerge
    pub fn makes_binpkgs(&self) -> bool {
        self.wants_buildpkg() || self.buildpkgonly
    }

    /// The `--buildpkg-exclude` atoms, one per word
    pub fn buildpkg_exclude_atoms(&self) -> impl Iterator<Item = &str> {
        self.buildpkg_exclude
            .iter()
            .flat_map(|s| s.split_whitespace())
    }

    /// Turn on `--buildpkg`/`--getbinpkg` for the `buildpkg`/`getbinpkg` words of `FEATURES`,
    /// unless the command line set them
    pub fn apply_features(&mut self, features: &str) {
        for feature in features.split_whitespace() {
            let flag = match feature {
                "buildpkg" => &mut self.buildpkg,
                "getbinpkg" => &mut self.getbinpkg,
                _ => continue,
            };
            flag.get_or_insert(YesNo::Y);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn features_turn_on_buildpkg_and_getbinpkg() {
        let mut flags = MergeFlags::default();
        flags.apply_features("binpkg-logs buildpkg-live getbinpkg");
        assert!(flags.wants_getbinpkg());
        assert!(!flags.wants_buildpkg());
        flags.apply_features("buildpkg");
        assert!(flags.wants_buildpkg());
    }

    #[test]
    fn an_explicit_n_beats_features() {
        let mut flags = MergeFlags {
            buildpkg: Some(YesNo::N),
            getbinpkg: Some(YesNo::N),
            ..MergeFlags::default()
        };
        flags.apply_features("buildpkg getbinpkg");
        assert!(!flags.wants_buildpkg());
        assert!(!flags.wants_getbinpkg());
    }

    #[test]
    fn buildpkg_and_getbinpkg_take_an_optional_attached_value() {
        let parse = |args: &[&str]| crate::cli::parse_cli(args).merge_flags();
        let bare = parse(&["em", "emerge", "-b", "-g", "app-misc/foo"]);
        assert_eq!(bare.buildpkg, Some(YesNo::Y));
        assert_eq!(bare.getbinpkg, Some(YesNo::Y));
        let off = parse(&[
            "em",
            "emerge",
            "--buildpkg=n",
            "--getbinpkg=n",
            "app-misc/foo",
        ]);
        assert_eq!(off.buildpkg, Some(YesNo::N));
        assert_eq!(off.getbinpkg, Some(YesNo::N));
        assert_eq!(parse(&["em", "emerge", "app-misc/foo"]).buildpkg, None);
    }

    #[test]
    fn short_buildpkg_and_getbinpkg_cluster_without_a_value() {
        let parse = |args: &[&str]| crate::cli::parse_cli(args).merge_flags();
        let bk = parse(&["em", "emerge", "-bk", "x/y"]);
        assert_eq!((bk.buildpkg, bk.usepkg), (Some(YesNo::Y), true));
        let gk = parse(&["em", "emerge", "-gK", "x/y"]);
        assert_eq!((gk.getbinpkg, gk.usepkgonly), (Some(YesNo::Y), true));
        let avb = parse(&["em", "-avb", "x/y"]);
        assert_eq!((avb.ask, avb.buildpkg), (true, Some(YesNo::Y)));
        let last = parse(&["em", "emerge", "-b", "--buildpkg=n", "x/y"]);
        assert_eq!(last.buildpkg, Some(YesNo::N));
        let last = parse(&["em", "emerge", "--buildpkg=n", "-b", "x/y"]);
        assert_eq!(last.buildpkg, Some(YesNo::Y));
    }

    #[test]
    fn a_detached_y_or_n_is_the_long_options_value_and_anything_else_an_atom() {
        let cli = crate::cli::parse_cli(&["em", "emerge", "--buildpkg", "n", "x/y"]);
        assert_eq!(cli.merge_flags().buildpkg, Some(YesNo::N));
        assert_eq!(cli.atoms(), ["x/y"]);
        let cli = crate::cli::parse_cli(&["em", "emerge", "--getbinpkg", "x/y"]);
        assert_eq!(cli.merge_flags().getbinpkg, Some(YesNo::Y));
        assert_eq!(cli.atoms(), ["x/y"]);
        let cli = crate::cli::parse_cli(&["em", "emerge", "--", "--buildpkg", "n"]);
        assert_eq!(cli.merge_flags().buildpkg, None);
    }

    #[test]
    fn an_old_resume_files_boolean_buildpkg_still_loads() {
        let old: MergeFlags =
            serde_json::from_str(r#"{"buildpkg": false, "getbinpkg": true}"#).unwrap();
        assert_eq!((old.buildpkg, old.getbinpkg), (None, Some(YesNo::Y)));
        let new: MergeFlags = serde_json::from_str(r#"{"buildpkg": "n"}"#).unwrap();
        assert_eq!(new.buildpkg, Some(YesNo::N));
    }

    #[test]
    fn with_bdeps_defaults_to_auto() {
        let unset = MergeFlags::default();
        assert!(unset.install_bdeps());
        assert!(unset.depclean_bdeps());

        let binpkgs = MergeFlags {
            getbinpkg: Some(YesNo::Y),
            ..MergeFlags::default()
        };
        assert!(!binpkgs.install_bdeps());
        let forced = MergeFlags {
            with_bdeps: Some(YesNo::Y),
            ..binpkgs
        };
        assert!(forced.install_bdeps());

        let no_auto = MergeFlags {
            with_bdeps_auto: Some(YesNo::N),
            ..MergeFlags::default()
        };
        assert!(!no_auto.install_bdeps());
        assert!(no_auto.depclean_bdeps());

        let off = MergeFlags {
            with_bdeps: Some(YesNo::N),
            ..MergeFlags::default()
        };
        assert!(!off.install_bdeps());
        assert!(!off.depclean_bdeps());
    }

    #[test]
    fn an_old_resume_files_boolean_with_bdeps_still_loads() {
        let old: MergeFlags = serde_json::from_str(r#"{"with_bdeps": true}"#).unwrap();
        assert_eq!(old.with_bdeps, Some(YesNo::Y));
        let new: MergeFlags = serde_json::from_str(r#"{"with_bdeps": "n"}"#).unwrap();
        assert_eq!(new.with_bdeps, Some(YesNo::N));
    }
}
