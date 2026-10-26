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
            let arg = squash_slashes(&format!("--libdir={prefix}/{libdir}"));
            (prefix, arg)
        });
        let cwd = shell.working_dir().to_path_buf();
        // configure path: $ECONF_SOURCE/configure, defaulting to $S (cwd).
        let configure = if econf_source == "." {
            cwd.join("configure")
        } else {
            std::path::Path::new(&econf_source).join("configure")
        };
        if !configure.exists() {
            return Ok(brush_core::ExecutionResult::new(0));
        }

        // Not PMS, but what portage's econf does to every package.
        let config_shell = get("CONFIG_SHELL");
        if !config_shell.is_empty()
            && let Err(e) = rewrite_sh_shebang(&configure, &config_shell)
        {
            let _ = writeln!(
                context.params.stderr(shell),
                "econf: {}: {e}",
                configure.display()
            );
        }
        let gnuconfig = std::path::PathBuf::from(format!("{eprefix}/usr/share/gnuconfig"));
        let workdir = get("WORKDIR");
        if gnuconfig.exists() && !workdir.is_empty() {
            let workdir = std::path::Path::new(&workdir);
            let quiet = get("PORTAGE_QUIET") == "1";
            for updated in refresh_gnuconfig(workdir, &gnuconfig) {
                if !quiet {
                    let rel = updated.strip_prefix(workdir).unwrap_or(&updated);
                    let name = updated.file_name().unwrap_or_default().to_string_lossy();
                    let _ = writeln!(
                        context.params.stderr(shell),
                        " * econf: updating {} with {}/{name}",
                        rel.display(),
                        gnuconfig.display()
                    );
                }
            }
        }
        if let Some((prefix, _)) = &libdir_arg {
            let mut v =
                brush_core::ShellVariable::new(brush_core::ShellValue::String(prefix.clone()));
            v.export();
            let _ = shell.set_env_global("CONF_PREFIX", v);
        }

        let exit = tokio::task::spawn_blocking(move || {
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
            if let Some((_, arg)) = &libdir_arg {
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

/// Point a `#!/bin/sh` `configure` at `config_shell`, keeping its mode and mtime
fn rewrite_sh_shebang(configure: &std::path::Path, config_shell: &str) -> std::io::Result<()> {
    let content = std::fs::read(configure)?;
    let Some(rest) = content.strip_prefix(b"#!") else {
        return Ok(());
    };
    let Some(after) = rest.trim_ascii_start().strip_prefix(b"/bin/sh") else {
        return Ok(());
    };
    if after.first().is_some_and(|c| !c.is_ascii_whitespace()) {
        return Ok(());
    }
    let meta = std::fs::metadata(configure)?;
    let tmp = tmp_sibling(configure);
    let mut new = format!("#!{config_shell}").into_bytes();
    new.extend_from_slice(after);
    std::fs::write(&tmp, new)?;
    std::fs::set_permissions(&tmp, meta.permissions())?;
    std::fs::File::options()
        .write(true)
        .open(&tmp)?
        .set_modified(meta.modified()?)?;
    std::fs::rename(&tmp, configure)
}

/// Replace every regular `config.guess`/`config.sub` under `workdir` with
/// `gnuconfig`'s copy, returning the files replaced
///
/// Each copy lands under a temporary name first, so a parallel build never reads half a script.
fn refresh_gnuconfig(
    workdir: &std::path::Path,
    gnuconfig: &std::path::Path,
) -> Vec<std::path::PathBuf> {
    let mut updated = Vec::new();
    let mut dirs = vec![workdir.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(ty) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if ty.is_dir() {
                dirs.push(path);
                continue;
            }
            let name = entry.file_name();
            if !ty.is_file() || (name != "config.guess" && name != "config.sub") {
                continue;
            }
            let tmp = tmp_sibling(&path);
            if std::fs::copy(gnuconfig.join(&name), &tmp).is_ok()
                && std::fs::rename(&tmp, &path).is_ok()
            {
                updated.push(path);
            } else {
                let _ = std::fs::remove_file(&tmp);
            }
        }
    }
    updated.sort();
    updated
}

fn tmp_sibling(path: &std::path::Path) -> std::path::PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".em-tmp.{}", std::process::id()));
    path.with_file_name(name)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gnuconfig_replaces_regular_files_at_any_depth() {
        let tmp = tempfile::tempdir().unwrap();
        let gnuconfig = tmp.path().join("gnuconfig");
        std::fs::create_dir(&gnuconfig).unwrap();
        std::fs::write(gnuconfig.join("config.guess"), "new guess").unwrap();
        std::fs::write(gnuconfig.join("config.sub"), "new sub").unwrap();
        let work = tmp.path().join("work");
        let deep = work.join("pkg-1/build-aux");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("config.sub"), "old").unwrap();
        std::fs::write(work.join("pkg-1/config.guess.in"), "untouched").unwrap();
        std::os::unix::fs::symlink("elsewhere", work.join("pkg-1/config.guess")).unwrap();

        let updated = refresh_gnuconfig(&work, &gnuconfig);

        assert_eq!(updated, vec![deep.join("config.sub")]);
        assert_eq!(
            std::fs::read_to_string(deep.join("config.sub")).unwrap(),
            "new sub"
        );
        assert!(
            work.join("pkg-1/config.guess").is_symlink(),
            "symlinks are left alone"
        );
        assert_eq!(
            std::fs::read_dir(&deep).unwrap().count(),
            1,
            "no temp file left"
        );
    }

    #[test]
    fn config_shell_rewrites_only_a_bin_sh_shebang_and_keeps_mtime() {
        let tmp = tempfile::tempdir().unwrap();
        let configure = tmp.path().join("configure");
        std::fs::write(&configure, "#! /bin/sh -e\necho hi\n").unwrap();
        let mtime = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        std::fs::File::options()
            .write(true)
            .open(&configure)
            .unwrap()
            .set_modified(mtime)
            .unwrap();

        rewrite_sh_shebang(&configure, "/bin/bash").unwrap();
        assert_eq!(
            std::fs::read_to_string(&configure).unwrap(),
            "#!/bin/bash -e\necho hi\n"
        );
        assert_eq!(
            std::fs::metadata(&configure).unwrap().modified().unwrap(),
            mtime
        );

        std::fs::write(&configure, "#!/bin/sharp\n").unwrap();
        rewrite_sh_shebang(&configure, "/bin/bash").unwrap();
        assert_eq!(
            std::fs::read_to_string(&configure).unwrap(),
            "#!/bin/sharp\n"
        );
    }
}
