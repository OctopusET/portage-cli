//! `docompress` / `dostrip` install-phase helpers (PMS 12.3.9 / 12.3.10)
//!
//! These record which installed paths the post-`src_install` pass should
//! compress or strip (and which to exclude). Rather than round-tripping the
//! lists through shell variables, the builtins push directly into an
//! `Arc`-shared state that the merge driver snapshots after the install
//! phase — the same approach as [`DieFlag`](super::die::DieFlag).

use std::sync::{Arc, Mutex};

use brush_core::builtins;

#[derive(Default)]
struct Lists {
    compress: Vec<String>,
    compress_exclude: Vec<String>,
    strip: Vec<String>,
    strip_exclude: Vec<String>,
}

/// Cross-subshell accumulator for the ecompress/estrip path lists
///
/// Shared (`Arc`) with every clone of the shell, so `docompress`/`dostrip`
/// calls made inside `src_install` — even from a subshell or helper
/// pipeline — land in the same lists the driver reads afterwards.
#[derive(Clone, Default)]
pub(crate) struct InstallPaths(Arc<Mutex<Lists>>);

/// Snapshot of the four path lists, handed to the post-install pass
#[derive(Default)]
pub struct InstallPathLists {
    /// `docompress` paths
    pub compress: Vec<String>,
    /// `docompress -x` paths
    pub compress_exclude: Vec<String>,
    /// `dostrip` paths
    pub strip: Vec<String>,
    /// `dostrip -x` paths
    pub strip_exclude: Vec<String>,
}

impl InstallPaths {
    fn with<R>(&self, f: impl FnOnce(&mut Lists) -> R) -> R {
        let mut guard = self.0.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut guard)
    }

    /// Copy out the accumulated lists for the post-install pass
    pub(crate) fn snapshot(&self) -> InstallPathLists {
        self.with(|l| InstallPathLists {
            compress: l.compress.clone(),
            compress_exclude: l.compress_exclude.clone(),
            strip: l.strip.clone(),
            strip_exclude: l.strip_exclude.clone(),
        })
    }
}

/// `docompress [-x] <path>...`  (PMS 12.3.9)
///
/// Without `-x`, marks paths for compression; with `-x`, exempts them.
#[derive(winnow_args::Args)]
#[arg(disable_help_flag, disable_version_flag, disable_help_subcommand)]
pub(crate) struct DocompressCommand {
    /// Add to the exclusion list instead of the inclusion list
    #[arg(short = 'x')]
    exclude: bool,
    #[arg(positional, stop_flags)]
    paths: Vec<String>,
}

brush_winnow_args_adapter::impl_parse_args!(DocompressCommand);

impl<SE: brush_core::ShellExtensions> builtins::Builtin<SE> for DocompressCommand {
    type Error = brush_core::Error;

    async fn execute(
        self,
        context: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<brush_core::ExecutionResult, Self::Error> {
        if let Some(paths) = context.shell.builtin_state::<InstallPaths>() {
            paths.with(|l| {
                let list = if self.exclude {
                    &mut l.compress_exclude
                } else {
                    &mut l.compress
                };
                list.extend(self.paths.iter().cloned());
            });
        }
        Ok(brush_core::ExecutionResult::new(0))
    }
}

/// `dostrip [-x] <path>...`  (PMS 12.3.10, EAPI 7+)
///
/// Without `-x`, marks paths for stripping; with `-x`, exempts them.
#[derive(winnow_args::Args)]
#[arg(disable_help_flag, disable_version_flag, disable_help_subcommand)]
pub(crate) struct DostripCommand {
    /// Add to the exclusion list instead of the inclusion list
    #[arg(short = 'x')]
    exclude: bool,
    #[arg(positional, stop_flags)]
    paths: Vec<String>,
}

brush_winnow_args_adapter::impl_parse_args!(DostripCommand);

impl<SE: brush_core::ShellExtensions> builtins::Builtin<SE> for DostripCommand {
    type Error = brush_core::Error;

    async fn execute(
        self,
        context: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<brush_core::ExecutionResult, Self::Error> {
        if let Some(paths) = context.shell.builtin_state::<InstallPaths>() {
            paths.with(|l| {
                let list = if self.exclude {
                    &mut l.strip_exclude
                } else {
                    &mut l.strip
                };
                list.extend(self.paths.iter().cloned());
            });
        }
        Ok(brush_core::ExecutionResult::new(0))
    }
}
