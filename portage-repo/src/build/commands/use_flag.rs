use std::io::Write;

use brush_core::builtins;
use portage_metadata::Eapi;

use super::die::DieFlag;
use super::output;

// ── use ───────────────────────────────────────────────────────────────────────

/// `use <flag>`  (PMS 12.3.1)
///
/// Returns 0 if flag is present as a whole word in `$USE`, 1 otherwise.
#[derive(winnow_args::Args)]
#[arg(
    disable_help_flag,
    disable_version_flag,
    disable_help_subcommand,
    unknown_flags = "value"
)]
pub(crate) struct UseCommand {
    #[arg(positional)]
    flag: String,
}

brush_winnow_args_adapter::impl_parse_args!(UseCommand);

impl<SE: brush_core::ShellExtensions> builtins::Builtin<SE> for UseCommand {
    type Error = brush_core::Error;

    async fn execute(
        self,
        context: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<brush_core::ExecutionResult, Self::Error> {
        if let Some(died) = check_iuse_effective(&context, &self.flag) {
            return Ok(died);
        }
        let enabled = use_flag_enabled(context.shell, &self.flag);
        Ok(brush_core::ExecutionResult::new(u8::from(!enabled)))
    }
}

// ── usev ──────────────────────────────────────────────────────────────────────

/// `usev <flag> [true-val]`  (PMS 12.3.6)
///
/// If flag is set: prints flag (or true-val if given) and returns 0.
/// If flag is unset: prints nothing and returns 1.
#[derive(winnow_args::Args)]
#[arg(
    disable_help_flag,
    disable_version_flag,
    disable_help_subcommand,
    unknown_flags = "value"
)]
pub(crate) struct UsevCommand {
    #[arg(positional)]
    flag: String,
    #[arg(positional)]
    true_val: Option<String>,
}

brush_winnow_args_adapter::impl_parse_args!(UsevCommand);

impl<SE: brush_core::ShellExtensions> builtins::Builtin<SE> for UsevCommand {
    type Error = brush_core::Error;

    async fn execute(
        self,
        context: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<brush_core::ExecutionResult, Self::Error> {
        if let Some(died) = check_iuse_effective(&context, &self.flag) {
            return Ok(died);
        }
        let shell = context.shell;
        if use_flag_enabled(shell, &self.flag) {
            // Default output is the flag name without any leading `!`.
            let out = self
                .true_val
                .as_deref()
                .unwrap_or_else(|| self.flag.strip_prefix('!').unwrap_or(&self.flag));
            let _ = writeln!(context.params.stdout(shell), "{out}");
            Ok(brush_core::ExecutionResult::success())
        } else {
            Ok(brush_core::ExecutionResult::new(1))
        }
    }
}

// ── usex ──────────────────────────────────────────────────────────────────────

/// `usex <flag> [true-str [false-str [true-suffix [false-suffix]]]]`  (PMS 12.3.7)
///
/// Prints `${true-str}${true-suffix}` (defaults: "yes", "") if flag is set,
/// or `${false-str}${false-suffix}` (defaults: "no", "") if not.
/// Returns 0 if flag is set, 1 otherwise.
#[derive(winnow_args::Args)]
#[arg(
    disable_help_flag,
    disable_version_flag,
    disable_help_subcommand,
    unknown_flags = "value"
)]
pub(crate) struct UsexCommand {
    #[arg(positional)]
    flag: String,
    #[arg(positional)]
    true_str: Option<String>,
    #[arg(positional)]
    false_str: Option<String>,
    #[arg(positional)]
    true_suffix: Option<String>,
    #[arg(positional)]
    false_suffix: Option<String>,
}

brush_winnow_args_adapter::impl_parse_args!(UsexCommand);

impl<SE: brush_core::ShellExtensions> builtins::Builtin<SE> for UsexCommand {
    type Error = brush_core::Error;

    async fn execute(
        self,
        context: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<brush_core::ExecutionResult, Self::Error> {
        if let Some(died) = check_iuse_effective(&context, &self.flag) {
            return Ok(died);
        }
        let shell = context.shell;
        if use_flag_enabled(shell, &self.flag) {
            let s = self.true_str.as_deref().unwrap_or("yes");
            let sfx = self.true_suffix.as_deref().unwrap_or("");
            let _ = writeln!(context.params.stdout(shell), "{s}{sfx}");
            Ok(brush_core::ExecutionResult::success())
        } else {
            let s = self.false_str.as_deref().unwrap_or("no");
            let sfx = self.false_suffix.as_deref().unwrap_or("");
            let _ = writeln!(context.params.stdout(shell), "{s}{sfx}");
            Ok(brush_core::ExecutionResult::new(1))
        }
    }
}

// ── use_enable / use_with ─────────────────────────────────────────────────────

/// `use_enable <flag> [feature [value]]`  (PMS 12.3.8)
///
/// Outputs `--enable-feature[=value]` or `--disable-feature`.
#[derive(winnow_args::Args)]
#[arg(
    disable_help_flag,
    disable_version_flag,
    disable_help_subcommand,
    unknown_flags = "value"
)]
pub(crate) struct UseEnableCommand {
    #[arg(positional)]
    flag: String,
    #[arg(positional)]
    feature: Option<String>,
    #[arg(positional)]
    val: Option<String>,
}

brush_winnow_args_adapter::impl_parse_args!(UseEnableCommand);

impl<SE: brush_core::ShellExtensions> builtins::Builtin<SE> for UseEnableCommand {
    type Error = brush_core::Error;

    async fn execute(
        self,
        context: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<brush_core::ExecutionResult, Self::Error> {
        if let Some(died) = check_iuse_effective(&context, &self.flag) {
            return Ok(died);
        }
        let shell = context.shell;
        // PMS/portage's use_enable() resolves the feature name with bash
        // `${2:-$1}` — an explicitly empty `feature` arg (e.g. `use_enable foo
        // ''`) falls back to `flag` exactly like an omitted one, so filter
        // empty before `unwrap_or` (which only catches `None`).
        let feature = self
            .feature
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(&self.flag);
        let out = if use_flag_enabled(shell, &self.flag) {
            match &self.val {
                Some(v) => format!("--enable-{feature}={v}"),
                None => format!("--enable-{feature}"),
            }
        } else {
            format!("--disable-{feature}")
        };
        let _ = writeln!(context.params.stdout(shell), "{out}");
        Ok(brush_core::ExecutionResult::success())
    }
}

/// `use_with <flag> [feature [value]]`  (PMS 12.3.9)
///
/// Outputs `--with-feature[=value]` or `--without-feature`.
#[derive(winnow_args::Args)]
#[arg(
    disable_help_flag,
    disable_version_flag,
    disable_help_subcommand,
    unknown_flags = "value"
)]
pub(crate) struct UseWithCommand {
    #[arg(positional)]
    flag: String,
    #[arg(positional)]
    feature: Option<String>,
    #[arg(positional)]
    val: Option<String>,
}

brush_winnow_args_adapter::impl_parse_args!(UseWithCommand);

impl<SE: brush_core::ShellExtensions> builtins::Builtin<SE> for UseWithCommand {
    type Error = brush_core::Error;

    async fn execute(
        self,
        context: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<brush_core::ExecutionResult, Self::Error> {
        if let Some(died) = check_iuse_effective(&context, &self.flag) {
            return Ok(died);
        }
        let shell = context.shell;
        // See the matching comment in UseEnableCommand: an explicitly empty
        // `feature` arg must fall back to `flag`, same as an omitted one.
        let feature = self
            .feature
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(&self.flag);
        let out = if use_flag_enabled(shell, &self.flag) {
            match &self.val {
                Some(v) => format!("--with-{feature}={v}"),
                None => format!("--with-{feature}"),
            }
        } else {
            format!("--without-{feature}")
        };
        let _ = writeln!(context.params.stdout(shell), "{out}");
        Ok(brush_core::ExecutionResult::success())
    }
}

// ── shared helpers ────────────────────────────────────────────────────────────

/// Returns true if `flag` appears as a whole word in the shell's `$USE`
pub(crate) fn use_flag_enabled<SE: brush_core::ShellExtensions>(
    shell: &brush_core::Shell<SE>,
    flag: &str,
) -> bool {
    flag_in_use(shell.env_str("USE").as_deref().unwrap_or(""), flag)
}

/// A `use*` query on a flag outside `IUSE_EFFECTIVE` dies from EAPI 5 and is a
/// QA notice before (PMS table 12.20); `Some` carries the die status.
///
/// Skipped when `IUSE_EFFECTIVE` is unset (metadata generation: PMS 11.1.1
/// leaves the set undefined there).
fn check_iuse_effective<SE: brush_core::ShellExtensions>(
    context: &brush_core::ExecutionContext<'_, SE>,
    flag: &str,
) -> Option<brush_core::ExecutionResult> {
    let shell = &*context.shell;
    let name = flag.strip_prefix('!').unwrap_or(flag);
    let effective = shell.env_str("IUSE_EFFECTIVE")?;
    if effective.is_empty() || effective.split_whitespace().any(|f| f == name) {
        return None;
    }
    let eapi = shell
        .env_str("EAPI")
        .and_then(|s| s.parse::<Eapi>().ok())
        .unwrap_or(Eapi::Zero);
    if !eapi.use_query_errors_outside_iuse_effective() {
        let var = |v: &str| shell.env_str(v).unwrap_or_default();
        let pkg = format!("{}/{}", var("CATEGORY"), var("PF"));
        let msg = format!("QA Notice: USE Flag '{name}' not in IUSE for {pkg}");
        output::eqawarn(context, &msg);
        return None;
    }
    let msg = format!("USE flag '{name}' not in IUSE_EFFECTIVE");
    if let Some(die) = shell.builtin_state::<DieFlag>() {
        die.raise(&msg);
    }
    let _ = writeln!(context.params.stderr(shell), "die: {msg}");
    Some(brush_core::ExecutionResult::new(1))
}

/// Whether `flag` is satisfied by the space-separated `use_str`
///
/// PMS 12.3.5/6/7: a leading `!` negates — `use !foo` is true when `foo` is
/// *disabled*. Without this, e.g. coreutils' `$(usev !caps --disable-libcap)`
/// never emits `--disable-libcap`, so configure autodetects and links the host's
/// libcap despite `USE=-caps` (a from-scratch ROOT then lacks it).
fn flag_in_use(use_str: &str, flag: &str) -> bool {
    let (name, negate) = match flag.strip_prefix('!') {
        Some(rest) => (rest, true),
        None => (flag, false),
    };
    let present = use_str.split_whitespace().any(|f| f == name);
    present ^ negate
}

#[cfg(test)]
mod tests {
    use super::flag_in_use;

    #[test]
    fn plain_and_negated_flags() {
        let use_str = "acl nls xattr";
        // Plain: present vs absent.
        assert!(flag_in_use(use_str, "acl"));
        assert!(!flag_in_use(use_str, "caps"));
        // Negated: true when the flag is disabled (the coreutils `!caps` case).
        assert!(flag_in_use(use_str, "!caps"));
        assert!(!flag_in_use(use_str, "!acl"));
        // Empty USE.
        assert!(!flag_in_use("", "acl"));
        assert!(flag_in_use("", "!acl"));
    }
}
