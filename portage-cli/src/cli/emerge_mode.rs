//! `em emerge`'s own mode switches — `-s`/`-S`/`-O`/`-C`/`-c`/`-P`/`-W`/`-r`.
//!
//! Flattened onto [`super::EmergeArgs`] only. Not global: `-s`/`-C`/`-c` are
//! emerge actions. Prefix-position `em -r` reaches these via the emerge retry.

/// Emerge's own action-selecting mode switches.
#[derive(winnow_args::Args, Debug, Clone, Default, PartialEq)]
#[arg(
    next_help_heading = "Actions",
    heading("Actions", help = "Pick a different operation than a merge.")
)]
pub struct EmergeModeArgs {
    /// Search package names (each argument is a pattern)
    ///
    /// Deliberately separate from the `em search` applet: this is emerge's
    /// own `-s`, emerge-style output; `em search` is the equery-style
    /// applet (`--all`/`--desc`/`--name-only`/`--homepage`). Same split as
    /// real Portage's `emerge -s` vs `equery`, not accidental duplication.
    // Drives `crate::search::run_emerge_style`; the applet drives `crate::search::run`.
    #[arg(short = 's', long)]
    pub search: bool,

    /// Search package names and descriptions
    ///
    /// Same split as `-s`/`em search` above: emerge-style output here,
    /// `em search --desc` is the equery-style applet.
    #[arg(short = 'S', long)]
    pub searchdesc: bool,

    /// Skip dependency resolution and only merge specified packages
    #[arg(short = 'O', long)]
    pub nodeps: bool,

    /// Remove the matching installed packages completely, without regard to dependencies
    ///
    /// Matches every installed slot/version of each atom. For removing unneeded dependencies
    /// too, use `depclean` instead.
    #[arg(short = 'C', long, effect = "destructive")]
    pub unmerge: bool,

    /// Remove installed packages that are not needed by @world (with no
    /// atoms, cleans everything unreachable; with atoms, only considers
    /// removing those, protecting everything else). Unlike `-C`, this walks
    /// the installed dependency graph first — matches real emerge's safe
    /// alternative to `-C`.
    ///
    /// Same behavior as the `em depclean [atoms]` applet — this flag exists
    /// for scripting convenience within a single `emerge`-style invocation.
    // crate::depclean::run forwards to run_with_targets — identical
    // implementation, not merely similar behavior.
    #[arg(short = 'c', long, effect = "destructive")]
    pub depclean: bool,

    /// Remove all but the highest installed version of each atom given (of
    /// every package without atoms), keeping older versions another installed
    /// package still depends on unless `--nodeps` is given.
    #[arg(short = 'P', long, effect = "destructive")]
    pub prune: bool,

    /// Remove atoms and/or `@set`s from the world file, without unmerging anything
    ///
    /// `--deselect=n` instead keeps a removal (`-C`/`-P`/`-c`) from dropping what it removed
    /// from the world file.
    #[arg(
        short = 'W',
        long,
        require_equals,
        default_missing = "y",
        value_name = "y|n"
    )]
    pub deselect: Option<super::YesNo>,

    /// Resume the last saved merge (see `em maint cleanresume` to discard it instead)
    ///
    /// Atoms are not accepted together with this flag — the package list comes from the saved
    /// state. Combine with other flags (e.g. `-r -X stuck/atom`) to adjust the resumed run.
    #[arg(short = 'r', long)]
    pub resume: bool,

    /// Print system/build info (`emerge --info` workalike), and how each installed atom was built
    #[arg(
        long,
        long_help = "`emerge --info` workalike. Print system/build info: profile, CHOST/CFLAGS/FEATURES/USE (with USE_EXPAND groups like VIDEO_CARDS broken out), ACCEPT_KEYWORDS/ACCEPT_LICENSE, and configured repositories. Each atom given adds the USE and the CHOST/CFLAGS/CXXFLAGS/FEATURES/LDFLAGS its installed versions were built with, where those differ from today's. Combine with `--json` for structured output, or `-v` to also list every known `@name` set and its resolved atoms (neither has a real-emerge equivalent)."
    )]
    pub info: bool,
}
