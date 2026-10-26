pub use anyhow::Result;

/// The user answered "No" to an `--ask` prompt, or closed its input
///
/// The prompt already said so; the `em` entry point exits 130 quietly, as emerge does.
#[derive(Debug, thiserror::Error)]
#[error("declined at the --ask prompt")]
pub struct Declined;

/// A `--pretend` resolve that surfaced required USE/mask changes
///
/// The detailed change block was already printed (by the depgraph), so this
/// is carried as an error purely to drive a non-zero exit through the
/// normal `Result` flow. The `em` binary entry point recognises it and
/// exits `1` *quietly* — no `error:` prefix — matching `emerge -p`, where
/// the printed block is the message. When the staged-build driver adds
/// step context, that context is still shown.
#[derive(Debug, thiserror::Error)]
#[error("USE/mask changes are required to proceed (see above)")]
pub struct ConfigChangesNeeded;

/// An atom on the command line, or every target, resolved to nothing mergeable/queryable
///
/// Each failure already printed its own `!!!` warning (unresolvable atom,
/// ambiguous name with its "pass -u" hint, etc.), so — same pattern as
/// [`ConfigChangesNeeded`] — this is carried purely to drive a non-zero exit
/// through the normal `Result` flow, without a final generic "no valid
/// atoms" line that adds nothing beyond what the warnings above it already
/// said.
#[derive(Debug, thiserror::Error)]
#[error("no valid atoms (see warnings above)")]
pub struct NoValidAtoms;

/// The resolver found no installable plan; the message is the explanation shown to the user
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ResolveFailed(pub String);

/// A requested atom has nothing installable: no ebuild, or every version masked
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct UnsatisfiableTarget(pub String);

/// `em grep` found no matches — same pattern as [`NoValidAtoms`]: drives a
/// quiet, non-zero exit (grep convention) without an extra "no matches"
/// line on top of the (empty) search output.
#[derive(Debug, thiserror::Error)]
#[error("no matches")]
pub struct NoMatches;
