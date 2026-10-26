//! `em maint binhost` — regenerate the binpkg `Packages` index
//!
//! Thin CLI wrapper: resolves `PKGDIR`/`CHOST` and reports the result. The
//! actual scan/checksum/write logic lives in `portage_binpkg::regen`.

use anyhow::{Result, bail};

use crate::binpkg::resolve_pkgdir;
use crate::cli::Cli;

/// Dispatch `em maint binhost`
pub async fn run(globals: &Cli) -> Result<()> {
    let pkgdir = resolve_pkgdir(globals).await;
    if !pkgdir.exists() {
        bail!("PKGDIR does not exist: {}", pkgdir);
    }
    let index = crate::binpkg::cli_index_config(globals).await;
    let _lock = crate::binpkg::lock_pkgdir(&pkgdir)?;
    let (count, skipped) = portage_binpkg::index_pkgdir(&pkgdir, &index)?;
    println!(
        "emaint binhost: indexed {} package(s){} -> {}/Packages",
        count,
        if skipped > 0 {
            format!(", skipped {skipped}")
        } else {
            String::new()
        },
        pkgdir
    );
    Ok(())
}
