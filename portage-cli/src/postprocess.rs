//! Post-`src_install` image processing, mirroring portage's ecompress and
//! estrip steps: documentation under the `docompress` inclusion list is
//! compressed (PMS 12.3.9), library modes are fixed as portage's install QA
//! does, and ELF objects and static archives are stripped unless excluded via
//! `dostrip`/`STRIP_MASK`/`RESTRICT=strip`/`FEATURES=nostrip` (PMS 12.3.10),
//! `FEATURES=splitdebug` first saving their debug info under `/usr/lib/debug`.
//!
//! Both passes shell out, to `${PORTAGE_COMPRESS}` and `${STRIP}` (with its
//! `objcopy`/`ranlib`), rather than being Rust builtins.

use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};

/// What estrip should do, decided from FEATURES/RESTRICT/dostrip
pub enum StripMode {
    /// FEATURES=nostrip, or RESTRICT=strip with no `dostrip` opt-ins
    Disabled,
    /// Default: every ELF object in the image (minus excludes)
    All,
    /// RESTRICT=strip with explicit `dostrip <path>` opt-ins
    Only(Vec<Utf8PathBuf>),
}

pub struct PostProcess {
    pub compress_include: Vec<Utf8PathBuf>,
    pub compress_exclude: Vec<Utf8PathBuf>,
    /// e.g. `bzip2`; the on-disk suffix is derived from it
    pub compress_cmd: String,
    pub compress_flags: Vec<String>,
    pub strip: StripMode,
    pub strip_exclude: Vec<Utf8PathBuf>,
    /// `STRIP_MASK` globs over installed paths (EAPI 0-6, which lack `dostrip -x`)
    pub strip_mask: Vec<glob::Pattern>,
    pub strip_cmd: String,
    /// Flags for executables and shared objects, see [`strip_flags`]
    pub strip_flags: Vec<String>,
    /// `OBJCOPY`, else STRIP's sibling `objcopy`
    pub objcopy_cmd: String,
    /// `RANLIB`, else STRIP's sibling `ranlib`
    pub ranlib_cmd: String,
    /// FEATURES=splitdebug: where debug files go, as an installed path
    pub split_debug: Option<SplitDebug>,
    /// The build's `PATH`, so both tools resolve as they did in the phases (e.g. LLVM's
    /// slotted `llvm-strip`, which em's own startup `PATH` may predate)
    pub path: Option<String>,
}

/// FEATURES=splitdebug: keep each stripped ELF's debug info in a separate file
pub struct SplitDebug {
    /// `/usr/lib/debug${EPREFIX}`; a debug file lands at `<dir><installed path>.debug`
    pub dir: Utf8PathBuf,
    /// FEATURES=compressdebug
    pub compress: bool,
}

/// `PORTAGE_STRIP_FLAGS` when set (even empty), else binutils' safe and default flags
pub fn strip_flags(portage_strip_flags: Option<&str>) -> Vec<String> {
    portage_strip_flags
        .unwrap_or(
            "--strip-unneeded -N __gentoo_check_ldflags__ \
             -R .comment -R .GCC.command.line -R .note.gnu.gold-version",
        )
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

#[derive(Default)]
pub struct PostStats {
    pub compressed: usize,
    pub relinked: usize,
    pub stripped: usize,
}

/// Suffixes ecompress must never touch: already-compressed data plus web/doc
/// formats that compress poorly or break in place (portage's
/// PORTAGE_COMPRESS_EXCLUDE_SUFFIXES).
const SKIP_SUFFIXES: &[&str] = &[
    "gz", "bz2", "xz", "lzma", "lz", "lz4", "zst", "Z", "css", "gif", "htm", "html", "jpeg", "jpg",
    "js", "pdf", "png", "svg",
];

/// ecompress skips tiny files where the container overhead wins
const MIN_COMPRESS_SIZE: u64 = 128;

fn compress_suffix(cmd: &str) -> &'static str {
    let prog = cmd.split('/').next_back().unwrap_or(cmd);
    match prog {
        "gzip" | "pigz" => ".gz",
        "xz" => ".xz",
        "zstd" => ".zst",
        "lz4" => ".lz4",
        _ => ".bz2",
    }
}

fn has_skip_suffix(path: &Utf8Path) -> bool {
    path.extension().is_some_and(|e| SKIP_SUFFIXES.contains(&e))
}

/// Whether `rel` (an absolute installed path like `/usr/share/man/...`) sits
/// under one of `prefixes`, matching on whole components.
fn under_any(rel: &Utf8Path, prefixes: &[Utf8PathBuf]) -> bool {
    prefixes.iter().any(|p| rel.starts_with(p))
}

/// All entries below `image_dir` as (absolute installed path, on-disk path)
fn walk(image_dir: &Utf8Path) -> Result<Vec<(Utf8PathBuf, Utf8PathBuf)>> {
    let mut out = Vec::new();
    let mut queue = std::collections::VecDeque::new();
    queue.push_back(image_dir.to_path_buf());
    while let Some(dir) = queue.pop_front() {
        let read_dir = std::fs::read_dir(dir.as_std_path())
            .with_context(|| format!("reading image dir {dir}"))?;
        for entry in read_dir {
            let entry = entry.context("reading dir entry")?;
            let path: Utf8PathBuf = entry
                .path()
                .try_into()
                .map_err(|_| anyhow::anyhow!("non-UTF-8 path in image"))?;
            let rel = path
                .strip_prefix(image_dir)
                .map_err(|_| anyhow::anyhow!("path escape: {path}"))?;
            let installed = Utf8PathBuf::from("/").join(rel);
            let meta = std::fs::symlink_metadata(path.as_std_path())?;
            if meta.is_dir() {
                queue.push_back(path);
            } else {
                out.push((installed, path));
            }
        }
    }
    Ok(out)
}

/// Normalize `.`/`..` in an absolute installed path (no filesystem access)
fn normalize(path: &Utf8Path) -> Utf8PathBuf {
    let mut parts: Vec<&str> = Vec::new();
    for c in path.components() {
        match c.as_str() {
            "/" | "." => {}
            ".." => {
                parts.pop();
            }
            p => parts.push(p),
        }
    }
    let mut out = Utf8PathBuf::from("/");
    for p in parts {
        out.push(p);
    }
    out
}

/// A command for `program`, run with the build's `PATH`
fn tool(program: &str, cfg: &PostProcess) -> std::process::Command {
    let mut cmd = std::process::Command::new(program);
    if let Some(path) = &cfg.path {
        cmd.env("PATH", path);
    }
    cmd
}

/// Run both passes over the image; returns counts for the caller to report
pub fn post_process_image(image_dir: &Utf8Path, cfg: &PostProcess) -> Result<PostStats> {
    let mut stats = PostStats::default();
    if !image_dir.exists() {
        return Ok(stats);
    }
    let entries = walk(image_dir)?;

    // -- strip the GNU info `dir` index --------------------------------
    // The `/usr/share/info/dir` index is regenerated by install-info and is
    // never owned by a package; portage drops it from every image. Left in, it
    // collides between any two info-installing packages merged into the same
    // ROOT (e.g. readline vs libidn2 → `/usr/share/info/dir.bz2`).
    let is_info_dir = |installed: &Utf8Path| {
        installed
            .parent()
            .is_some_and(|p| p.as_str().ends_with("/share/info"))
            && installed
                .file_name()
                .is_some_and(|n| n == "dir" || n.starts_with("dir."))
    };
    let entries: Vec<_> = entries
        .into_iter()
        .filter(|(installed, on_disk)| {
            if is_info_dir(installed) {
                let _ = std::fs::remove_file(on_disk.as_std_path());
                false
            } else {
                true
            }
        })
        .collect();

    fix_library_modes(&entries)?;

    // -- ecompress -----------------------------------------------------
    let suffix = compress_suffix(&cfg.compress_cmd);
    let in_compress_scope = |installed: &Utf8Path| {
        under_any(installed, &cfg.compress_include) && !under_any(installed, &cfg.compress_exclude)
    };
    // Installed paths that end up compressed, for the symlink fixup below.
    let mut compressed: std::collections::HashSet<Utf8PathBuf> = std::collections::HashSet::new();
    for (installed, on_disk) in &entries {
        if !in_compress_scope(installed) || has_skip_suffix(installed) {
            continue;
        }
        let meta = std::fs::symlink_metadata(on_disk.as_std_path())?;
        if !meta.is_file() || meta.len() < MIN_COMPRESS_SIZE {
            continue;
        }
        let status = tool(&cfg.compress_cmd, cfg)
            .args(&cfg.compress_flags)
            .arg("-f")
            .arg("--")
            .arg(on_disk.as_std_path())
            .status()
            .with_context(|| format!("running {}", cfg.compress_cmd))?;
        if status.success() {
            compressed.insert(installed.clone());
            stats.compressed += 1;
        } else {
            crate::style::warn_line!("ecompress: {} failed on {installed}", cfg.compress_cmd);
        }
    }

    // Symlinks pointing at a file we just compressed must be renamed and
    // retargeted (man-page cross references: `foo.1 -> bar.1` becomes
    // `foo.1.bz2 -> bar.1.bz2`).
    for (installed, on_disk) in &entries {
        if !in_compress_scope(installed) {
            continue;
        }
        let Ok(meta) = std::fs::symlink_metadata(on_disk.as_std_path()) else {
            continue; // already renamed
        };
        if !meta.is_symlink() {
            continue;
        }
        let Ok(target) = std::fs::read_link(on_disk.as_std_path()) else {
            continue;
        };
        let Some(target) = camino::Utf8Path::from_path(&target) else {
            continue;
        };
        let resolved = if target.is_absolute() {
            normalize(target)
        } else {
            let dir = installed.parent().unwrap_or(Utf8Path::new("/"));
            normalize(&dir.join(target))
        };
        if !compressed.contains(&resolved) {
            continue;
        }
        let new_link = Utf8PathBuf::from(format!("{on_disk}{suffix}"));
        let new_target = format!("{target}{suffix}");
        std::os::unix::fs::symlink(&new_target, new_link.as_std_path())
            .with_context(|| format!("relinking {installed}"))?;
        std::fs::remove_file(on_disk.as_std_path())?;
        stats.relinked += 1;
    }

    // -- estrip --------------------------------------------------------
    let strip_scope: Option<&[Utf8PathBuf]> = match &cfg.strip {
        StripMode::Disabled => return Ok(stats),
        StripMode::All => None,
        StripMode::Only(paths) if paths.is_empty() => return Ok(stats),
        StripMode::Only(paths) => Some(paths),
    };
    for (installed, on_disk) in &entries {
        if let Some(includes) = strip_scope
            && !under_any(installed, includes)
        {
            continue;
        }
        if under_any(installed, &cfg.strip_exclude)
            || cfg.strip_mask.iter().any(|m| m.matches(installed.as_str()))
        {
            continue;
        }
        let Ok(meta) = std::fs::symlink_metadata(on_disk.as_std_path()) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let elf = is_strippable_elf(on_disk);
        // Archive members are not final links, so splitdebug keeps their debug info in place.
        let archive = !elf
            && cfg.split_debug.is_none()
            && installed.as_str().ends_with(".a")
            && is_ar_archive(on_disk);
        if !elf && !archive {
            continue;
        }

        // Image files may be installed read-only; strip needs to rewrite in
        // place (portage chmods and restores the same way).
        use std::os::unix::fs::PermissionsExt;
        let perms = meta.permissions();
        let writable = perms.mode() & 0o200 != 0;
        if !writable {
            let mut p = perms.clone();
            p.set_mode(perms.mode() | 0o200);
            let _ = std::fs::set_permissions(on_disk.as_std_path(), p);
        }
        let ok = if archive {
            run(tool(&cfg.strip_cmd, cfg).arg("-g").arg(on_disk), installed)
                && run(tool(&cfg.ranlib_cmd, cfg).arg(on_disk), installed)
        } else {
            if let Some(split) = &cfg.split_debug
                && !installed.as_str().ends_with(".debug")
            {
                save_debug(image_dir, installed, on_disk, &perms, split, cfg)?;
            }
            run(
                tool(&cfg.strip_cmd, cfg)
                    .args(&cfg.strip_flags)
                    .arg(on_disk),
                installed,
            )
        };
        if !writable {
            let _ = std::fs::set_permissions(on_disk.as_std_path(), perms);
        }
        if ok {
            stats.stripped += 1;
        }
    }

    Ok(stats)
}

/// Run one tool on `installed`; a failure, a missing tool included, only warns, as in estrip
fn run(cmd: &mut std::process::Command, installed: &Utf8Path) -> bool {
    let program = cmd.get_program().to_string_lossy().into_owned();
    match cmd.status() {
        Ok(status) if status.success() => true,
        Ok(_) => {
            crate::style::warn_line!("estrip: {program} failed on {installed}");
            false
        }
        Err(e) => {
            crate::style::warn_line!("estrip: {program} failed on {installed}: {e}");
            false
        }
    }
}

/// `set` (the tool's variable) or else `derived`, when the build `PATH` has it, else `bare`,
/// as estrip falls back to the unprefixed tool
pub fn binutil(set: Option<String>, derived: String, bare: &str, path: Option<&str>) -> String {
    let name = set.unwrap_or(derived);
    let found = if name.contains('/') {
        Utf8Path::new(&name).is_file()
    } else {
        let path = path.map_or_else(|| std::env::var("PATH").unwrap_or_default(), str::to_string);
        std::env::split_paths(&path).any(|dir| dir.join(&name).is_file())
    };
    if found { name } else { bare.to_string() }
}

/// Copy `on_disk`'s debug info to the splitdebug tree and link the object to it
fn save_debug(
    image_dir: &Utf8Path,
    installed: &Utf8Path,
    on_disk: &Utf8Path,
    perms: &std::fs::Permissions,
    split: &SplitDebug,
    cfg: &PostProcess,
) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let rel = format!("{}{installed}.debug", split.dir);
    let debug = image_dir.join(rel.trim_start_matches('/'));
    if let Some(dir) = debug.parent() {
        std::fs::create_dir_all(dir.as_std_path()).with_context(|| format!("creating {dir}"))?;
    }
    let mut keep = tool(&cfg.objcopy_cmd, cfg);
    keep.arg("--only-keep-debug");
    if split.compress {
        keep.arg("--compress-debug-sections");
    }
    if !run(keep.arg(on_disk).arg(&debug), installed) {
        return Ok(());
    }
    // A setuid/setgid object's symbols stay unreadable to group and others.
    if let Ok(meta) = std::fs::metadata(debug.as_std_path()) {
        let mut mode = meta.permissions().mode() & !0o113;
        if perms.mode() & 0o6000 != 0 {
            mode &= !0o044;
        }
        let _ =
            std::fs::set_permissions(debug.as_std_path(), std::fs::Permissions::from_mode(mode));
    }
    run(
        tool(&cfg.objcopy_cmd, cfg)
            .arg(format!("--add-gnu-debuglink={debug}"))
            .arg(on_disk),
        installed,
    );
    Ok(())
}

fn is_ar_archive(path: &Utf8Path) -> bool {
    use std::io::Read;
    let mut magic = [0u8; 8];
    std::fs::File::open(path.as_std_path()).is_ok_and(|mut f| f.read_exact(&mut magic).is_ok())
        && &magic == b"!<arch>\n"
}

/// Shared libraries directly in a library directory are made executable, static archives and
/// libtool files are not, as portage's install QA does
fn fix_library_modes(entries: &[(Utf8PathBuf, Utf8PathBuf)]) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    for (installed, on_disk) in entries {
        let (Some(dir), Some(name)) = (installed.parent(), installed.file_name()) else {
            continue;
        };
        let in_lib_dir = dir.file_name().is_some_and(|d| d.starts_with("lib"))
            && match dir.parent().map(Utf8Path::as_str) {
                Some("/" | "/usr") => true,
                Some(p) => p.starts_with("/opt/") && p.matches('/').count() == 2,
                None => false,
            };
        if !in_lib_dir {
            continue;
        }
        let exec = if name.ends_with(".so") || name.contains(".so.") {
            true
        } else if name.ends_with(".a") || name.ends_with(".la") {
            false
        } else {
            continue;
        };
        let meta = std::fs::symlink_metadata(on_disk.as_std_path())?;
        if !meta.is_file() {
            continue;
        }
        let mode = meta.permissions().mode();
        let want = if exec { mode | 0o111 } else { mode & !0o111 };
        if want != mode {
            std::fs::set_permissions(on_disk.as_std_path(), std::fs::Permissions::from_mode(want))
                .with_context(|| format!("chmod {installed}"))?;
        }
    }
    Ok(())
}

/// True for ET_EXEC/ET_DYN ELF objects (shared libraries and executables)
/// Relocatable objects (.o, .ko) are left alone: `--strip-unneeded` on those
/// can drop relocation symbols they still need.
fn is_strippable_elf(path: &Utf8Path) -> bool {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path.as_std_path()) else {
        return false;
    };
    let mut head = [0u8; 18];
    if f.read_exact(&mut head).is_err() {
        return false;
    }
    if &head[..4] != b"\x7fELF" {
        return false;
    }
    let e_type = match head[5] {
        1 => u16::from_le_bytes([head[16], head[17]]), // ELFDATA2LSB
        2 => u16::from_be_bytes([head[16], head[17]]), // ELFDATA2MSB
        _ => return false,
    };
    matches!(e_type, 2 | 3) // ET_EXEC | ET_DYN
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Utf8Path, data: &[u8]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path.as_std_path(), data).unwrap();
    }

    fn default_cfg() -> PostProcess {
        PostProcess {
            compress_include: vec![
                Utf8PathBuf::from("/usr/share/doc"),
                Utf8PathBuf::from("/usr/share/info"),
                Utf8PathBuf::from("/usr/share/man"),
            ],
            compress_exclude: vec![],
            compress_cmd: "bzip2".into(),
            compress_flags: vec!["-9".into()],
            strip: StripMode::All,
            strip_exclude: vec![],
            strip_mask: vec![],
            strip_cmd: "strip".into(),
            strip_flags: strip_flags(None),
            objcopy_cmd: "objcopy".into(),
            ranlib_cmd: "ranlib".into(),
            split_debug: None,
            path: None,
        }
    }

    /// Fake strip/objcopy/ranlib that log their argv; objcopy writes its output file.
    fn fake_tools(dir: &Utf8Path) -> (PostProcess, Utf8PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let log = dir.join("argv.log");
        for name in ["strip", "objcopy", "ranlib"] {
            let script = dir.join(format!("fake-{name}"));
            let mut body = format!("#!/bin/sh\necho \"{name} $*\" >> '{log}'\n");
            if name == "objcopy" {
                body.push_str("[ \"$1\" = --only-keep-debug ] && for a; do out=$a; done && : > \"$out\"\nexit 0\n");
            }
            write(&script, body.as_bytes());
            std::fs::set_permissions(script.as_std_path(), std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }
        let mut cfg = default_cfg();
        cfg.strip_cmd = dir.join("fake-strip").into_string();
        cfg.objcopy_cmd = dir.join("fake-objcopy").into_string();
        cfg.ranlib_cmd = dir.join("fake-ranlib").into_string();
        (cfg, log)
    }

    fn tiny_elf() -> Vec<u8> {
        let mut elf = vec![0u8; 64];
        elf[..4].copy_from_slice(b"\x7fELF");
        elf[5] = 1;
        elf[16] = 3;
        elf
    }

    fn logged(log: &Utf8Path, image: &Utf8Path) -> Vec<String> {
        std::fs::read_to_string(log.as_std_path())
            .unwrap_or_default()
            .lines()
            .map(|l| l.replace(&format!("{image}/"), ""))
            .collect()
    }

    #[test]
    fn strip_flags_default_unless_portage_strip_flags_is_set() {
        assert_eq!(strip_flags(None)[0], "--strip-unneeded");
        assert_eq!(strip_flags(Some("-s -v")), ["-s", "-v"]);
        assert!(strip_flags(Some("")).is_empty());
    }

    #[test]
    fn archives_are_stripped_of_debug_info_and_reindexed() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::try_from(tmp.path().to_path_buf()).unwrap();
        let (mut cfg, log) = fake_tools(&root);
        cfg.strip_flags = vec!["--custom".into()];
        let image = root.join("image");
        write(&image.join("usr/lib64/libx.a"), b"!<arch>\nmembers");
        write(&image.join("usr/lib64/libx.so"), &tiny_elf());
        write(&image.join("usr/lib64/fake.a"), b"not an archive");

        assert_eq!(post_process_image(&image, &cfg).unwrap().stripped, 2);
        let mut calls = logged(&log, &image);
        calls.sort();
        assert_eq!(
            calls,
            [
                "ranlib usr/lib64/libx.a",
                "strip --custom usr/lib64/libx.so",
                "strip -g usr/lib64/libx.a",
            ]
        );
    }

    #[test]
    fn a_missing_tool_warns_instead_of_failing_the_install() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::try_from(tmp.path().to_path_buf()).unwrap();
        let (mut cfg, log) = fake_tools(&root);
        cfg.ranlib_cmd = root.join("eu-ranlib").into_string();
        let image = root.join("image");
        write(&image.join("usr/lib64/libx.a"), b"!<arch>\nmembers");
        write(&image.join("usr/lib64/libx.so"), &tiny_elf());

        assert_eq!(post_process_image(&image, &cfg).unwrap().stripped, 1);
        assert_eq!(logged(&log, &image).len(), 2);
    }

    #[test]
    fn binutil_falls_back_to_the_bare_tool_off_path() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = Utf8PathBuf::try_from(tmp.path().to_path_buf()).unwrap();
        write(&dir.join("x86_64-pc-linux-gnu-ranlib"), b"");
        let path = Some(dir.as_str());
        let pick = |set: Option<&str>, derived: &str| {
            binutil(set.map(str::to_string), derived.to_string(), "ranlib", path)
        };
        assert_eq!(
            pick(None, "x86_64-pc-linux-gnu-ranlib"),
            "x86_64-pc-linux-gnu-ranlib"
        );
        assert_eq!(pick(None, "eu-ranlib"), "ranlib");
        assert_eq!(
            pick(Some("my-ranlib"), "x86_64-pc-linux-gnu-ranlib"),
            "ranlib"
        );
    }

    #[test]
    fn splitdebug_saves_debug_info_before_stripping() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::try_from(tmp.path().to_path_buf()).unwrap();
        let (mut cfg, log) = fake_tools(&root);
        cfg.strip_flags = vec![];
        cfg.split_debug = Some(SplitDebug {
            dir: Utf8PathBuf::from("/usr/lib/debug"),
            compress: true,
        });
        let image = root.join("image");
        write(&image.join("usr/bin/tool"), &tiny_elf());
        write(&image.join("usr/lib64/libx.a"), b"!<arch>\nmembers");

        assert_eq!(post_process_image(&image, &cfg).unwrap().stripped, 1);
        assert!(image.join("usr/lib/debug/usr/bin/tool.debug").is_file());
        assert_eq!(
            logged(&log, &image),
            [
                "objcopy --only-keep-debug --compress-debug-sections usr/bin/tool \
                 usr/lib/debug/usr/bin/tool.debug",
                "objcopy --add-gnu-debuglink=usr/lib/debug/usr/bin/tool.debug usr/bin/tool",
                "strip usr/bin/tool",
            ]
        );
    }

    #[test]
    fn strip_mask_excludes_matching_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::try_from(tmp.path().to_path_buf()).unwrap();
        let (mut cfg, log) = fake_tools(&root);
        cfg.strip_mask = vec![glob::Pattern::new("*/plugins/*.so").unwrap()];
        let image = root.join("image");
        write(&image.join("usr/lib64/app/plugins/a.so"), &tiny_elf());
        write(&image.join("usr/lib64/libapp.so"), &tiny_elf());

        assert_eq!(post_process_image(&image, &cfg).unwrap().stripped, 1);
        assert_eq!(logged(&log, &image).len(), 1);
        assert!(logged(&log, &image)[0].ends_with("usr/lib64/libapp.so"));
    }

    #[test]
    fn library_modes_follow_the_file_kind_in_library_dirs_only() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let image = Utf8PathBuf::try_from(tmp.path().join("image")).unwrap();
        let files = [
            ("usr/lib64/libfoo.so.1.2", 0o644, 0o755),
            ("usr/lib64/libfoo.a", 0o755, 0o644),
            ("opt/x/lib/libbar.so", 0o644, 0o755),
            ("usr/lib64/sub/libbaz.so", 0o644, 0o644),
            ("usr/share/lib.so", 0o644, 0o644),
        ];
        for (rel, mode, _) in files {
            let path = image.join(rel);
            write(&path, b"x");
            std::fs::set_permissions(path.as_std_path(), std::fs::Permissions::from_mode(mode))
                .unwrap();
        }
        fix_library_modes(&walk(&image).unwrap()).unwrap();
        for (rel, _, want) in files {
            let mode = std::fs::metadata(image.join(rel).as_std_path())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, want, "{rel}");
        }
    }

    #[test]
    fn compresses_man_pages_and_retargets_symlinks() {
        // `post_process_image` spawns `compress_cmd`/`strip_cmd` (`bzip2`/
        // `strip`), which resolve off the process-wide `PATH` — see
        // `crate::test_support::path_lock`'s doc comment.
        let _path_lock = crate::test_support::path_lock();
        let tmp = tempfile::tempdir().unwrap();
        let image = Utf8PathBuf::try_from(tmp.path().join("image")).unwrap();
        let man = image.join("usr/share/man/man1");
        write(&man.join("foo.1"), &[b'x'; 4096]);
        std::os::unix::fs::symlink("foo.1", man.join("bar.1").as_std_path()).unwrap();
        // Outside the inclusion list: untouched.
        write(&image.join("usr/bin/tool"), &[b'y'; 4096]);
        // Too small to be worth compressing.
        write(&man.join("tiny.1"), b"x");

        let stats = post_process_image(&image, &default_cfg()).unwrap();

        assert_eq!(stats.compressed, 1);
        assert_eq!(stats.relinked, 1);
        assert!(man.join("foo.1.bz2").exists());
        assert!(!man.join("foo.1").exists());
        assert_eq!(
            std::fs::read_link(man.join("bar.1.bz2").as_std_path())
                .unwrap()
                .to_str()
                .unwrap(),
            "foo.1.bz2"
        );
        assert!(man.join("bar.1").as_std_path().symlink_metadata().is_err());
        assert!(image.join("usr/bin/tool").exists());
        assert!(man.join("tiny.1").exists());
    }

    #[test]
    fn docompress_exclude_is_honored() {
        let _path_lock = crate::test_support::path_lock();
        let tmp = tempfile::tempdir().unwrap();
        let image = Utf8PathBuf::try_from(tmp.path().join("image")).unwrap();
        write(
            &image.join("usr/share/doc/pkg-1/html/index.txt"),
            &[b'x'; 4096],
        );
        write(&image.join("usr/share/doc/pkg-1/README"), &[b'x'; 4096]);

        let mut cfg = default_cfg();
        cfg.compress_exclude
            .push(Utf8PathBuf::from("/usr/share/doc/pkg-1/html"));
        let stats = post_process_image(&image, &cfg).unwrap();

        assert_eq!(stats.compressed, 1);
        assert!(image.join("usr/share/doc/pkg-1/html/index.txt").exists());
        assert!(image.join("usr/share/doc/pkg-1/README.bz2").exists());
    }

    #[test]
    fn strips_only_in_dostrip_scope() {
        let _path_lock = crate::test_support::path_lock();
        let tmp = tempfile::tempdir().unwrap();
        let image = Utf8PathBuf::try_from(tmp.path().join("image")).unwrap();
        // Not ELF: never touched, even in scope.
        write(&image.join("usr/bin/script"), b"#!/bin/sh\n");

        let mut cfg = default_cfg();
        cfg.strip = StripMode::Only(vec![Utf8PathBuf::from("/usr/bin")]);
        let stats = post_process_image(&image, &cfg).unwrap();
        assert_eq!(stats.stripped, 0);
    }

    #[test]
    fn strip_resolves_through_the_build_path() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::try_from(tmp.path().to_path_buf()).unwrap();
        let bin = root.join("llvm/bin");
        write(
            &bin.join("only-in-build-path-strip"),
            b"#!/bin/sh\nexit 0\n",
        );
        let script = bin.join("only-in-build-path-strip");
        std::fs::set_permissions(
            script.as_std_path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        let mut elf = vec![0u8; 64];
        elf[..4].copy_from_slice(b"\x7fELF");
        elf[5] = 1;
        elf[16] = 3;
        let image = root.join("image");
        write(&image.join("usr/lib64/libx.so"), &elf);

        let mut cfg = default_cfg();
        cfg.strip_cmd = "only-in-build-path-strip".into();
        cfg.path = Some(format!("{bin}:/usr/bin:/bin"));
        assert_eq!(post_process_image(&image, &cfg).unwrap().stripped, 1);
    }

    #[test]
    fn elf_detection_wants_exec_or_dyn() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = Utf8PathBuf::try_from(tmp.path().to_path_buf()).unwrap();
        let mut elf = vec![0u8; 64];
        elf[..4].copy_from_slice(b"\x7fELF");
        elf[5] = 1; // little-endian
        elf[16] = 3; // ET_DYN
        write(&dir.join("lib.so"), &elf);
        assert!(is_strippable_elf(&dir.join("lib.so")));
        elf[16] = 1; // ET_REL
        write(&dir.join("obj.o"), &elf);
        assert!(!is_strippable_elf(&dir.join("obj.o")));
        write(&dir.join("plain"), b"not elf");
        assert!(!is_strippable_elf(&dir.join("plain")));
    }
}
