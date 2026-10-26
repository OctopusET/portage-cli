use std::io::Write;

use brush_core::builtins;

use super::die::DieFlag;

/// `econf [extra-args...]`
///
/// Runs `./configure` (or `$ECONF_SOURCE/configure`) with standard flags
/// derived from the current EAPI and portage environment variables.
#[derive(winnow_args::Args)]
#[arg(
    disable_help_flag,
    disable_version_flag,
    disable_help_subcommand,
    unknown_flags = "value"
)]
pub(crate) struct EconfCommand {
    #[arg(positional, stop_flags)]
    args: Vec<String>,
}

brush_winnow_args_adapter::impl_parse_args!(EconfCommand);

impl<SE: brush_core::ShellExtensions> builtins::Builtin<SE> for EconfCommand {
    type Error = brush_core::Error;

    async fn execute(
        self,
        context: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<brush_core::ExecutionResult, Self::Error> {
        let (stdout, stderr) = super::context_stdio(&context);
        // Capture the shared die flag before the shell is moved out of context;
        // econf self-dies on configure failure (PMS 12.3.2), so the eclasses
        // that call bare `econf` (no `|| die`) still abort the build.
        let die_flag = context.shell.builtin_state::<DieFlag>().cloned();
        // configure (and the pkg-config it spawns) must see the full build
        // environment, including any bashrc-exported overlay search paths.
        let env_vars = super::context_env(&context);
        let shell = context.shell;

        let get = |var: &str| {
            shell
                .env_str(var)
                .map(|s| s.into_owned())
                .unwrap_or_default()
        };
        let eapi: u32 = get("EAPI").parse().unwrap_or(0);
        let econf_source = {
            let s = get("ECONF_SOURCE");
            if s.is_empty() { ".".to_string() } else { s }
        };
        let eprefix = get("EPREFIX");
        let pf = get("PF");
        let chost = get("CHOST");
        let cbuild = get("CBUILD");
        let ctarget = get("CTARGET");
        let esysroot = {
            let s = get("ESYSROOT");
            if s.is_empty() { "/".to_string() } else { s }
        };
        let extra_econf = get("EXTRA_ECONF");
        // `nonfatal econf` (PORTAGE_NONFATAL) suppresses the self-die.
        let nonfatal = get("PORTAGE_NONFATAL") == "1";
        // Only a profile LIBDIR_${ABI} adds --libdir; otherwise configure keeps its own default.
        let libdir = {
            let abi = get("ABI");
            if abi.is_empty() {
                String::new()
            } else {
                get(&format!("LIBDIR_{abi}"))
            }
        };

        let user_args = self.args.clone();
        // --libdir follows the ebuild's own --exec-prefix/--prefix; CONF_PREFIX tells einstall.
        let libdir_arg = (!libdir.is_empty()
            && !user_args.iter().any(|a| a.starts_with("--libdir=")))
        .then(|| {
            let flag = |name: &str| user_args.iter().find_map(|a| a.strip_prefix(name));
            let prefix = flag("--exec-prefix=")
                .or_else(|| flag("--prefix="))
                .map_or_else(|| format!("{eprefix}/usr"), str::to_owned);
            let prefix = format!("/{}", prefix.trim_start_matches('/'));
            let mut v =
                brush_core::ShellVariable::new(brush_core::ShellValue::String(prefix.clone()));
            v.export();
            let _ = shell.set_env_global("CONF_PREFIX", v);
            squash_slashes(&format!("--libdir={prefix}/{libdir}"))
        });
        let cwd = shell.working_dir().to_path_buf();

        let exit = tokio::task::spawn_blocking(move || {
            // configure path: $ECONF_SOURCE/configure, defaulting to $S (cwd).
            let base = if econf_source == "." {
                cwd.clone()
            } else {
                std::path::PathBuf::from(&econf_source)
            };
            let configure = base.join("configure");
            if !configure.exists() {
                return 0u8;
            }

            // Probe EAPI-conditional flags from configure --help.
            let help = if eapi >= 4 {
                std::process::Command::new(&configure)
                    .arg("--help")
                    .output()
                    .ok()
                    .and_then(|o| String::from_utf8(o.stdout).ok())
                    .unwrap_or_default()
            } else {
                String::new()
            };

            let mut conf_args: Vec<String> = Vec::new();

            conf_args.push(format!("--prefix={eprefix}/usr"));
            if !cbuild.is_empty() {
                conf_args.push(format!("--build={cbuild}"));
            }
            if !chost.is_empty() {
                conf_args.push(format!("--host={chost}"));
            }
            if !ctarget.is_empty() {
                conf_args.push(format!("--target={ctarget}"));
            }
            conf_args.push(format!("--mandir={eprefix}/usr/share/man"));
            conf_args.push(format!("--infodir={eprefix}/usr/share/info"));
            conf_args.push(format!("--datadir={eprefix}/usr/share"));
            conf_args.push(format!("--sysconfdir={eprefix}/etc"));
            conf_args.push(format!("--localstatedir={eprefix}/var/lib"));

            if eapi >= 8 && help.contains("--datarootdir") {
                conf_args.push(format!("--datarootdir={eprefix}/usr/share"));
            }
            // Use word-boundary guard matching portage's pattern.
            if eapi >= 4 && contains_flag(&help, "--disable-dependency-tracking") {
                conf_args.push("--disable-dependency-tracking".to_string());
            }
            if eapi >= 5 && contains_flag(&help, "--disable-silent-rules") {
                conf_args.push("--disable-silent-rules".to_string());
            }
            // Portage requires both --enable-shared and --enable-static before adding
            // --disable-static, to avoid touching packages that don't support static builds.
            if eapi >= 8
                && contains_flag(&help, "--enable-shared")
                && contains_flag(&help, "--enable-static")
            {
                conf_args.push("--disable-static".to_string());
            }
            if eapi >= 6 {
                if help.contains("--docdir") {
                    conf_args.push(format!("--docdir={eprefix}/usr/share/doc/{pf}"));
                }
                if help.contains("--htmldir") {
                    conf_args.push(format!("--htmldir={eprefix}/usr/share/doc/{pf}/html"));
                }
            }
            if eapi >= 7 && contains_flag(&help, "--with-sysroot") {
                conf_args.push(format!("--with-sysroot={esysroot}"));
            }
            if let Some(arg) = &libdir_arg {
                conf_args.push(arg.clone());
            }

            conf_args.extend(user_args);
            // EXTRA_ECONF is split on whitespace; quoted-whitespace in values is rare
            // in practice (portage eval's it, which we can't do safely here).
            conf_args.extend(extra_econf.split_whitespace().map(str::to_owned));

            let mut cmd = std::process::Command::new(&configure);
            cmd.current_dir(&cwd).args(&conf_args);
            cmd.stdout(stdout).stderr(stderr);
            for (k, v) in &env_vars {
                cmd.env(k, v);
            }

            cmd.status()
                .map(|s| s.code().unwrap_or(1) as u8)
                .unwrap_or(127)
        })
        .await
        .unwrap_or(127);

        // PMS: econf aborts the build when configure fails. Raise the shared
        // die flag (checked by the phase driver after the phase returns) so a
        // failed configure can't silently proceed to emake/src_install and
        // merge an empty package. `exit == 0` also covers the no-configure
        // fast path above, which is intentionally a no-op.
        if exit != 0 && !nonfatal {
            let msg = format!("econf failed (configure exited {exit})");
            if let Some(flag) = &die_flag {
                flag.raise(&msg);
            }
            let _ = writeln!(context.params.stderr(shell), "die: {msg}");
        }

        Ok(brush_core::ExecutionResult::new(exit))
    }
}

fn squash_slashes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if !(c == '/' && out.ends_with('/')) {
            out.push(c);
        }
    }
    out
}

/// Whether `flag` appears in `text` at a word boundary
///
/// Matches portage's word-boundary guard: `flag` must be followed by a
/// non-identifier character (space, newline, `=`, end-of-string).
/// Prevents `--disable-dependency-tracking` from matching
/// `--disable-dependency-tracking-fast`.
fn contains_flag(text: &str, flag: &str) -> bool {
    let mut rest = text;
    while let Some(pos) = rest.find(flag) {
        let after = &rest[pos + flag.len()..];
        if after
            .chars()
            .next()
            .is_none_or(|c| !c.is_ascii_alphanumeric() && !"+_.-".contains(c))
        {
            return true;
        }
        rest = &rest[pos + 1..];
    }
    false
}
