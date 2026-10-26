use std::collections::HashMap;

use camino::{Utf8Path, Utf8PathBuf};
use portage_repo::{Manifest, ManifestEntry};
use tokio::io::AsyncWriteExt;

use crate::error::{Error, Result};
use crate::resolver::{Distfile, RestrictGate};

/// `filename -> DIST` [`ManifestEntry`], for O(1) lookup during a fetch batch
///
/// The old approach — a linear `manifest.dist_entries().find()` scan per
/// file — is fine for one package's ~5-entry Manifest (the only case before
/// a repo-wide mirror tool existed); a combined Manifest spanning a whole
/// repo is tens of thousands of entries, where the linear scan becomes
/// O(files²), run from inside concurrently-scheduled async tasks.
#[derive(Debug, Clone, Default)]
pub struct DistDigests(HashMap<String, ManifestEntry>);

impl DistDigests {
    pub fn new() -> Self {
        Self(HashMap::new())
    }

    /// Fold `manifest`'s `DIST` entries in
    ///
    /// First entry per filename wins — matches first-owner-wins ownership
    /// when folding multiple packages' manifests into one combined index.
    pub fn extend_from_manifest(&mut self, manifest: &Manifest) {
        for entry in manifest.dist_entries() {
            if let ManifestEntry::Dist { filename, .. } = entry {
                self.0
                    .entry(filename.clone())
                    .or_insert_with(|| entry.clone());
            }
        }
    }

    pub fn get(&self, filename: &str) -> Option<&ManifestEntry> {
        self.0.get(filename)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl From<&Manifest> for DistDigests {
    fn from(manifest: &Manifest) -> Self {
        let mut digests = Self::new();
        digests.extend_from_manifest(manifest);
        digests
    }
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Strategy for downloading a distfile
///
/// `Builtin` uses the embedded reqwest client.  `Command` shells out to an
/// external program using the same template variables as Portage's
/// `FETCHCOMMAND` / `RESUMECOMMAND` make.conf settings.
#[derive(Debug, Clone, Default)]
pub enum FetchStrategy {
    /// Built-in reqwest HTTP client (default)
    #[default]
    Builtin,
    /// External command template
    ///
    /// Template variables (same as Portage):
    /// - `${URI}` — the full download URL
    /// - `${FILE}` — just the filename
    /// - `${DISTDIR}` — the distfiles directory path
    Command(String),
}

/// Fetch and resume configuration
#[derive(Debug, Clone)]
pub struct FetchConfig {
    /// Primary fetch strategy.  Defaults to `Builtin`
    pub strategy: FetchStrategy,
    /// Fallback command template used when the primary strategy fails
    pub fallback_command: Option<String>,
    /// Resume command template (`RESUMECOMMAND`)
    pub resume_command: Option<String>,
    /// Maximum number of distfiles fetched concurrently.  Defaults to 4
    pub max_concurrent: usize,
    /// Accept an already-present file on **size alone**, skipping the full
    /// hash.
    ///
    /// Default `false` — every present file is fully re-verified. Set
    /// `true` for a repo-wide mirror tool: re-hashing a multi-hundred-GB
    /// mirror on every run would turn a "nothing to do" pass into a
    /// full-disk scan. Matches real `emirrordist`'s own default.
    pub trust_existing_size: bool,
    /// Accept a distfile with no `DIST` entry, checking only that it is non-empty
    ///
    /// Default `true`. A package build sets it from the repo's `allow-missing-manifests`,
    /// `FEATURES=digest` and `EBUILD_SKIP_MANIFEST`, refusing unverifiable files otherwise.
    pub allow_missing_digests: bool,
}

impl Default for FetchConfig {
    fn default() -> Self {
        Self {
            strategy: FetchStrategy::default(),
            fallback_command: None,
            resume_command: None,
            max_concurrent: 4,
            trust_existing_size: false,
            allow_missing_digests: true,
        }
    }
}

impl FetchConfig {
    /// Build from `make.conf`-style environment/config values
    pub fn from_make_conf(fetch_command: Option<String>, resume_command: Option<String>) -> Self {
        match fetch_command {
            Some(cmd) => Self {
                strategy: FetchStrategy::Command(cmd),
                resume_command,
                ..Self::default()
            },
            None => Self {
                strategy: FetchStrategy::Builtin,
                resume_command,
                ..Self::default()
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Fetcher
// ---------------------------------------------------------------------------

/// Outcome of a single fetch operation
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchStatus {
    /// File was already present and passed manifest verification
    AlreadyPresent,
    /// File was downloaded and verified successfully
    Downloaded,
    /// RESTRICT=fetch — the distfile must not be auto-fetched
    ///
    /// The caller should run the ebuild's `pkg_nofetch` phase, which prints
    /// manual download instructions.
    FetchRestricted,
}

/// Downloads and verifies distfiles
#[derive(Clone)]
pub struct Fetcher {
    client: reqwest::Client,
    distdir: Utf8PathBuf,
    /// Read-only distfile locations searched before downloading
    /// (`PORTAGE_RO_DISTDIRS` semantics — e.g. the system distdir when the
    /// writable one is a per-user directory).
    ro_distdirs: Vec<Utf8PathBuf>,
    config: FetchConfig,
    /// Package-level `RESTRICT=fetch`/`mirror` (PMS 7.3.2)
    restrict: RestrictGate,
}

impl Fetcher {
    pub fn new(distdir: Utf8PathBuf, config: FetchConfig) -> Self {
        // Send a User-Agent: some mirrors (e.g. freedesktop.org's Apache)
        // return HTTP 403 for requests with an empty/missing UA, mirroring how
        // portage's default wget/curl FETCHCOMMAND always identifies itself.
        // Per-read idle timeout, not a whole-request one, so a stalled server moves on to the
        // next URL without capping large downloads; 60s matches wget's `-T 60` in make.globals.
        let client = reqwest::Client::builder()
            .user_agent(concat!("em/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(std::time::Duration::from_secs(60))
            .read_timeout(std::time::Duration::from_secs(60))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            client,
            distdir,
            ro_distdirs: Vec::new(),
            config,
            restrict: RestrictGate::default(),
        }
    }

    /// Add read-only locations consulted for already-present distfiles
    pub fn with_ro_distdirs(mut self, dirs: Vec<Utf8PathBuf>) -> Self {
        self.ro_distdirs = dirs;
        self
    }

    /// Apply package-level `RESTRICT=fetch` / `RESTRICT=mirror`
    pub fn with_restrict(mut self, restrict: RestrictGate) -> Self {
        self.restrict = restrict;
        self
    }

    /// Fetch a single distfile, verifying it against `manifest`
    ///
    /// If the file already exists and passes verification it is not
    /// re-downloaded.  If a partial file is present a resume is attempted.
    ///
    /// Builds a [`DistDigests`] from `manifest` for this one call — thin
    /// wrapper over [`Self::fetch_distfile_digests`]. A caller fetching many
    /// files against the same manifest (i.e. [`Self::fetch_all`]) builds the
    /// index once instead of once per file.
    pub async fn fetch_distfile(&self, df: &Distfile, manifest: &Manifest) -> Result<FetchStatus> {
        self.fetch_distfile_digests(df, &DistDigests::from(manifest))
            .await
    }

    /// Same as [`Self::fetch_distfile`], but takes a pre-built [`DistDigests`]
    /// index instead of re-deriving one from a [`Manifest`] every call.
    pub async fn fetch_distfile_digests(
        &self,
        df: &Distfile,
        digests: &DistDigests,
    ) -> Result<FetchStatus> {
        let dest = self.distdir.join(&df.filename);

        // Held for the whole fetch, present-check included, against other em tasks, em processes
        // and emerge sharing this DISTDIR.
        let _lock = lock_distfile(&dest).await;

        let manifest_entry = digests.get(&df.filename);
        if manifest_entry.is_none() && !self.config.allow_missing_digests {
            return Err(Error::Verify {
                filename: df.filename.clone(),
                reason: "no DIST entry in the Manifest".to_owned(),
            });
        }

        // Fast path: already present and valid (writable dir first, then the
        // read-only locations).
        for dir in std::iter::once(&self.distdir).chain(self.ro_distdirs.iter()) {
            let candidate = dir.join(&df.filename);
            if !candidate.exists() {
                continue;
            }
            let valid = match manifest_entry {
                Some(entry) if self.config.trust_existing_size => {
                    dist_size(entry).is_some_and(|size| current_size(&candidate) == size)
                }
                _ => verify_ok(manifest_entry, &candidate),
            };
            if !valid {
                continue;
            }
            // Found in a read-only distdir, not the writable DISTDIR: expose it
            // in DISTDIR (portage symlinks RO distfiles in) so unpack/eapply —
            // which only look in DISTDIR — find it. Without this, em reports
            // "already present" for a file the build then can't open (e.g.
            // bash's `bash53-NNN` patches under /var/cache/distfiles).
            if dir != &self.distdir {
                link_into_distdir(&candidate, &dest);
            }
            return Ok(FetchStatus::AlreadyPresent);
        }

        // PMS 7.3.2: `fetch+`/`mirror+` exempt a URI from RESTRICT=fetch.
        // A file already in DISTDIR was handled above; missing + not exempt
        // means the caller should run pkg_nofetch.
        if self.restrict.fetch && !RestrictGate::uri_exempts_fetch(df.restriction.as_deref()) {
            return Ok(FetchStatus::FetchRestricted);
        }

        if df.urls.is_empty() {
            return Err(Error::AllFailed {
                filename: df.filename.clone(),
            });
        }

        // Try each URL in order.
        let mut last_err = None;
        for url in &df.urls {
            let result = self
                .fetch_one_url(&self.config.strategy, url, &dest, manifest_entry)
                .await;
            match result {
                Ok(()) => return Ok(FetchStatus::Downloaded),
                Err(e) => {
                    eprintln!("fetch: {url}: {e}");
                    last_err = Some(e);
                }
            }
        }

        // Primary strategy exhausted — try fallback command if configured.
        if let Some(cmd_template) = &self.config.fallback_command {
            let fallback = FetchStrategy::Command(cmd_template.clone());
            for url in &df.urls {
                match self
                    .fetch_one_url(&fallback, url, &dest, manifest_entry)
                    .await
                {
                    Ok(()) => return Ok(FetchStatus::Downloaded),
                    Err(e) => last_err = Some(e),
                }
            }
        }

        Err(last_err.unwrap_or(Error::AllFailed {
            filename: df.filename.clone(),
        }))
    }

    /// Fetch all distfiles in parallel, returning per-file results in **input
    /// order**.
    ///
    /// Up to `config.max_concurrent` downloads run simultaneously.
    /// Each result is paired with the originating [`Distfile`] reference.
    /// Callers that only need the embedded `Distfile` can ignore order; callers
    /// that zip against a parallel side table (owner CPV, etc.) must rely on
    /// this guarantee or key by `Distfile::filename`.
    ///
    /// Builds one [`DistDigests`] index from `manifest` for the whole batch —
    /// thin wrapper over [`Self::fetch_all_digests`].
    pub async fn fetch_all<'a>(
        &self,
        distfiles: &'a [Distfile],
        manifest: &Manifest,
    ) -> Vec<(&'a Distfile, Result<FetchStatus>)> {
        self.fetch_all_digests(distfiles, &DistDigests::from(manifest))
            .await
    }

    /// Same as [`Self::fetch_all`], but takes a pre-built [`DistDigests`]
    /// index — the primitive a repo-wide mirror tool needs, where the
    /// combined digest set spans many packages' manifests and would be far
    /// too expensive to rebuild from scratch per file.
    ///
    /// Results are returned in **input order** (`StreamExt::buffered`), not
    /// completion order (`buffer_unordered` would scramble them).
    pub async fn fetch_all_digests<'a>(
        &self,
        distfiles: &'a [Distfile],
        digests: &DistDigests,
    ) -> Vec<(&'a Distfile, Result<FetchStatus>)> {
        use futures_util::StreamExt;
        use std::sync::Arc;

        let fetcher = Arc::new(self.clone());
        let digests = Arc::new(digests.clone());
        let max = self.config.max_concurrent.max(1);

        futures_util::stream::iter(distfiles)
            .map(|df| {
                let fetcher = Arc::clone(&fetcher);
                let digests = Arc::clone(&digests);
                async move {
                    let r = fetcher.fetch_distfile_digests(df, &digests).await;
                    (df, r)
                }
            })
            .buffered(max)
            .collect()
            .await
    }

    // -----------------------------------------------------------------------
    // Internal
    // -----------------------------------------------------------------------

    /// Download `url` with `strategy` to `dest`'s [`download_path`], verify it, then rename it over `dest`
    async fn fetch_one_url(
        &self,
        strategy: &FetchStrategy,
        url: &str,
        dest: &Utf8Path,
        manifest_entry: Option<&ManifestEntry>,
    ) -> Result<()> {
        let temp = download_path(dest);
        match strategy {
            FetchStrategy::Builtin => self.fetch_builtin(url, &temp, manifest_entry).await?,
            FetchStrategy::Command(template) => {
                self.fetch_command(template, url, &temp, manifest_entry)
                    .await?
            }
        }
        rename_into_place(&temp, dest)
    }

    /// Run `FETCHCOMMAND` into `temp`, or `RESUMECOMMAND` when `temp` is a plausible partial
    async fn fetch_command(
        &self,
        template: &str,
        url: &str,
        temp: &Utf8Path,
        manifest_entry: Option<&ManifestEntry>,
    ) -> Result<()> {
        let resume = self
            .config
            .resume_command
            .as_deref()
            .filter(|_| is_resumable(manifest_entry.and_then(dist_size), current_size(temp)));
        if resume.is_none() {
            let _ = std::fs::remove_file(temp.as_std_path());
        }
        self.run_command(
            resume.unwrap_or(template),
            url,
            temp.file_name().unwrap_or(""),
        )
        .await?;
        verify_or_discard(manifest_entry, temp)
    }

    async fn fetch_builtin(
        &self,
        url: &str,
        dest: &Utf8Path,
        manifest_entry: Option<&ManifestEntry>,
    ) -> Result<()> {
        let expected_size = manifest_entry.and_then(dist_size);
        let existing_size = current_size(dest);

        // Try to resume a *plausible* partial first (cheap when it is a genuine
        // prefix). If the resume produces a verified file we are done; otherwise
        // we fall through. The resume is never trusted on its own.
        if is_resumable(expected_size, existing_size)
            && self
                .resume_partial(url, dest, existing_size, manifest_entry)
                .await?
        {
            return Ok(());
        }

        // Either there was nothing worth resuming, or the resume did not yield a
        // valid file. Discard whatever is on disk and download the whole file
        // fresh — a corrupt/short/HTML leftover must never linger to be Ranged
        // into on the next URL or run (the psmisc-class failure).
        let _ = std::fs::remove_file(dest.as_std_path());
        self.download_full(url, dest, manifest_entry).await
    }

    /// Resume a partial via `RESUMECOMMAND` (if set) or an HTTP `Range` request
    ///
    /// Returns `Ok(true)` only when the resumed file verifies against the
    /// manifest; `Ok(false)` means "couldn't resume — download fresh instead".
    async fn resume_partial(
        &self,
        url: &str,
        dest: &Utf8Path,
        existing_size: u64,
        manifest_entry: Option<&ManifestEntry>,
    ) -> Result<bool> {
        if let Some(resume_tmpl) = &self.config.resume_command {
            let ran = self
                .run_command(resume_tmpl, url, dest.file_name().unwrap_or(""))
                .await
                .is_ok();
            return Ok(ran && verify_ok(manifest_entry, dest));
        }

        let response = self
            .client
            .get(url)
            .header("Range", format!("bytes={existing_size}-"))
            .send()
            .await
            .map_err(|e| Error::Network {
                url: url.to_owned(),
                source: e,
            })?;

        // Only a 206 actually continues the partial; a 200 means the server
        // ignored Range and is resending from byte 0 — let the fresh path own
        // that (it truncates), and reject an HTML error/redirect body outright.
        if response.status() != reqwest::StatusCode::PARTIAL_CONTENT || is_html(&response) {
            return Ok(false);
        }
        let mut file = tokio::fs::OpenOptions::new()
            .append(true)
            .open(dest.as_std_path())
            .await
            .map_err(|e| Error::Io {
                path: dest.to_path_buf().into_std_path_buf(),
                source: e,
            })?;
        stream_to_file(url, response, &mut file, dest).await?;
        Ok(verify_ok(manifest_entry, dest))
    }

    /// Download the entire file fresh (no `Range`), rejecting obvious
    /// non-file bodies and verifying against the manifest.
    ///
    /// A body that fails verification is removed so it can't masquerade as
    /// a resumable partial next time.
    async fn download_full(
        &self,
        url: &str,
        dest: &Utf8Path,
        manifest_entry: Option<&ManifestEntry>,
    ) -> Result<()> {
        let response = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| Error::Network {
                url: url.to_owned(),
                source: e,
            })?;

        let status = response.status();
        if !status.is_success() {
            return Err(Error::Http {
                url: url.to_owned(),
                status: status.as_u16(),
            });
        }
        // A distfile is never HTML; a 2xx `text/html` body is an error/redirect
        // page (e.g. a SourceForge "file not found"/mirror picker), not the
        // archive — caching it would fail verification on every retry forever.
        if is_html(&response) {
            return Err(Error::Verify {
                filename: dest.file_name().unwrap_or("?").to_owned(),
                reason: "server returned an HTML body, not the distfile".to_owned(),
            });
        }

        let mut file = tokio::fs::File::create(dest.as_std_path())
            .await
            .map_err(|e| Error::Io {
                path: dest.to_path_buf().into_std_path_buf(),
                source: e,
            })?;
        stream_to_file(url, response, &mut file, dest).await?;
        verify_or_discard(manifest_entry, dest)
    }

    /// Execute a FETCHCOMMAND/RESUMECOMMAND template
    ///
    /// Template substitution: `${URI}` → url, `${FILE}` → filename,
    /// `${DISTDIR}` → distdir path.  The expanded command is run via `sh -c`.
    async fn run_command(&self, template: &str, url: &str, filename: &str) -> Result<()> {
        let cmd = template
            .replace("${URI}", url)
            .replace("${FILE}", filename)
            .replace("${DISTDIR}", self.distdir.as_str());

        let status = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&cmd)
            .status()
            .await
            .map_err(|e| Error::CommandSpawn { source: e })?;

        if status.success() {
            Ok(())
        } else {
            Err(Error::Command {
                command: cmd,
                code: status.code().unwrap_or(-1),
            })
        }
    }
}

/// Suffix of an in-progress download; portage uses the same one, so either tool resumes the other's partial
const DOWNLOAD_SUFFIX: &str = ".__download__";

/// Whether a DISTDIR entry is an in-progress (or interrupted) download, never a distfile
pub fn is_partial_download_name(name: &str) -> bool {
    name.ends_with(DOWNLOAD_SUFFIX)
}

/// Suffix of portage's distlocks lock file, `.<name>.portage_lockfile`
const LOCK_SUFFIX: &str = ".portage_lockfile";

/// Whether a DISTDIR entry is a fetch lock file, never a distfile
pub fn is_fetch_lock_name(name: &str) -> bool {
    name.starts_with('.') && name.ends_with(LOCK_SUFFIX)
}

/// Whether a fetch currently holds the lock for the distfile the partial download at `partial` belongs to
pub fn partial_download_is_live(partial: &Utf8Path) -> bool {
    partial
        .as_str()
        .strip_suffix(DOWNLOAD_SUFFIX)
        .is_some_and(|dest| lock_path(Utf8Path::new(dest)).as_std_path().exists())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Where `dest` is downloaded to before it is verified and renamed into place
fn download_path(dest: &Utf8Path) -> Utf8PathBuf {
    Utf8PathBuf::from(format!("{dest}{DOWNLOAD_SUFFIX}"))
}

fn rename_into_place(temp: &Utf8Path, dest: &Utf8Path) -> Result<()> {
    std::fs::rename(temp.as_std_path(), dest.as_std_path()).map_err(|e| Error::Io {
        path: dest.to_path_buf().into_std_path_buf(),
        source: e,
    })
}

/// Current on-disk size of `dest`, or 0 when it is absent or unreadable
fn current_size(dest: &Utf8Path) -> u64 {
    std::fs::metadata(dest.as_std_path())
        .map(|m| m.len())
        .unwrap_or(0)
}

/// The manifest's recorded size for a distfile entry (`None` for non-`Dist`)
fn dist_size(entry: &ManifestEntry) -> Option<u64> {
    match entry {
        ManifestEntry::Dist { size, .. } => Some(*size),
        _ => None,
    }
}

/// Exclusive fetch lock on a distfile; dropping it unlinks the lock file, then releases the lock
struct DistLock {
    path: Utf8PathBuf,
    _file: std::fs::File,
}

impl Drop for DistLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(self.path.as_std_path());
    }
}

/// Path of portage's distlocks lock file for `dest` (`.<name>.portage_lockfile` beside it)
fn lock_path(dest: &Utf8Path) -> Utf8PathBuf {
    let name = dest.file_name().unwrap_or("");
    dest.with_file_name(format!(".{name}{LOCK_SUFFIX}"))
}

/// Acquire the exclusive lock guarding `dest`, or `None` when it cannot be taken (read-only DISTDIR)
///
/// `flock` excludes other em tasks and processes; the POSIX record lock is what portage's distlocks
/// take, and the two kinds do not conflict on Linux. Where record locks fail, portage falls back to `flock` too.
async fn lock_distfile(dest: &Utf8Path) -> Option<DistLock> {
    let path = lock_path(dest);
    tokio::task::spawn_blocking(move || {
        loop {
            let f = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(path.as_std_path())
                .ok()?;
            rustix::fs::flock(&f, rustix::fs::FlockOperation::LockExclusive).ok()?;
            while rustix::fs::fcntl_lock(&f, rustix::fs::FlockOperation::LockExclusive)
                == Err(rustix::io::Errno::INTR)
            {}
            // The previous holder unlinks the file before releasing it; retry on the new one.
            let held = rustix::fs::fstat(&f).ok()?;
            if let Ok(now) = rustix::fs::stat(path.as_std_path())
                && (now.st_dev, now.st_ino) == (held.st_dev, held.st_ino)
            {
                return Some(DistLock { path, _file: f });
            }
        }
    })
    .await
    .ok()
    .flatten()
}

/// A leftover file is a resumable partial only when its size is a *strict*
/// prefix of the target: present, and smaller than the known manifest size.
///
/// Without a known size we never resume (a blind `Range` onto an unknown
/// body is how a corrupt cache wedges every retry); a complete-but-wrong
/// file (`>=` expected) is refetched fresh, not appended to.
fn is_resumable(expected_size: Option<u64>, existing_size: u64) -> bool {
    matches!(expected_size, Some(exp) if existing_size > 0 && existing_size < exp)
}

/// Whether a response carries an HTML body — never a distfile, so it's an
/// error/redirect page to be rejected rather than saved.
fn is_html(response: &reqwest::Response) -> bool {
    response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|ct| {
            ct.trim_start()
                .to_ascii_lowercase()
                .starts_with("text/html")
        })
        .unwrap_or(false)
}

/// Check `dest` against its `DIST` entry; with none, only a non-empty regular file passes
fn check_distfile(
    manifest_entry: Option<&ManifestEntry>,
    dest: &Utf8Path,
) -> std::result::Result<(), String> {
    match manifest_entry {
        Some(entry) => entry
            .verify_file(dest.as_std_path())
            .map_err(|e| e.to_string()),
        None if std::fs::metadata(dest.as_std_path()).is_ok_and(|m| m.is_file() && m.len() > 0) => {
            Ok(())
        }
        None => Err("empty file with no DIST entry to verify against".to_owned()),
    }
}

fn verify_ok(manifest_entry: Option<&ManifestEntry>, dest: &Utf8Path) -> bool {
    check_distfile(manifest_entry, dest).is_ok()
}

/// Verify `dest`; on failure delete it (so it can't be treated as a resumable
/// partial later) and return the error.
fn verify_or_discard(manifest_entry: Option<&ManifestEntry>, dest: &Utf8Path) -> Result<()> {
    check_distfile(manifest_entry, dest).map_err(|reason| {
        let _ = std::fs::remove_file(dest.as_std_path());
        Error::Verify {
            filename: dest.file_name().unwrap_or("?").to_owned(),
            reason,
        }
    })
}

/// Stream a response body into `file` to completion, then flush
async fn stream_to_file(
    url: &str,
    response: reqwest::Response,
    file: &mut tokio::fs::File,
    dest: &Utf8Path,
) -> Result<()> {
    use futures_util::StreamExt;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| Error::Network {
            url: url.to_owned(),
            source: e,
        })?;
        file.write_all(&chunk).await.map_err(|e| Error::Io {
            path: dest.to_path_buf().into_std_path_buf(),
            source: e,
        })?;
    }
    file.flush().await.map_err(|e| Error::Io {
        path: dest.to_path_buf().into_std_path_buf(),
        source: e,
    })?;
    Ok(())
}

/// Expose a distfile found in a read-only distdir under the writable `dest`
/// (in DISTDIR), so the build's unpack/eapply — which only consult DISTDIR —
/// can open it.
///
/// Best-effort, mirroring portage: prefer a symlink to the RO copy, fall
/// back to a hard link, then a copy; replaces any stale entry.
fn link_into_distdir(src: &Utf8Path, dest: &Utf8Path) {
    if let Some(parent) = dest.parent() {
        let _ = std::fs::create_dir_all(parent.as_std_path());
    }
    let _ = std::fs::remove_file(dest.as_std_path());
    if std::os::unix::fs::symlink(src.as_std_path(), dest.as_std_path()).is_ok() {
        return;
    }
    if std::fs::hard_link(src.as_std_path(), dest.as_std_path()).is_ok() {
        return;
    }
    let _ = std::fs::copy(src.as_std_path(), dest.as_std_path());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_only_strict_size_partials() {
        // A genuine under-size partial is resumable.
        assert!(is_resumable(Some(1000), 400));
        // Nothing on disk yet → download fresh.
        assert!(!is_resumable(Some(1000), 0));
        // Complete-but-wrong (corrupt full file) → refetch fresh, don't append.
        assert!(!is_resumable(Some(1000), 1000));
        // Over-size garbage → refetch fresh.
        assert!(!is_resumable(Some(1000), 1500));
        // Unknown manifest size → never blind-resume.
        assert!(!is_resumable(None, 400));
        // The psmisc case: a 139 KB body vs a 432 KB target is *size*-plausible, so
        // a resume is attempted — but the caller always falls back to a fresh
        // download (and discards) when that resume fails to verify.
        assert!(is_resumable(Some(432208), 139065));
    }

    #[test]
    fn link_into_distdir_symlinks_ro_copy_and_replaces_stale() {
        let tmp = tempfile::tempdir().unwrap();
        let base = Utf8Path::from_path(tmp.path()).unwrap();
        let ro = base.join("ro");
        let dist = base.join("dist");
        std::fs::create_dir_all(ro.as_std_path()).unwrap();
        std::fs::create_dir_all(dist.as_std_path()).unwrap();

        let src = ro.join("bash53-001");
        std::fs::write(src.as_std_path(), b"PATCH").unwrap();
        let dest = dist.join("bash53-001");

        // Fresh DISTDIR: a symlink to the RO copy is created and readable.
        link_into_distdir(&src, &dest);
        let meta = std::fs::symlink_metadata(dest.as_std_path()).unwrap();
        assert!(meta.file_type().is_symlink(), "should be a symlink");
        assert_eq!(std::fs::read(dest.as_std_path()).unwrap(), b"PATCH");

        // A stale DISTDIR entry is replaced (not left pointing elsewhere).
        std::fs::remove_file(dest.as_std_path()).unwrap();
        std::fs::write(dest.as_std_path(), b"STALE").unwrap();
        link_into_distdir(&src, &dest);
        assert_eq!(std::fs::read(dest.as_std_path()).unwrap(), b"PATCH");
    }

    fn dist_entry(filename: &str, content: &[u8]) -> ManifestEntry {
        use blake2::{Blake2b512, Digest};
        ManifestEntry::Dist {
            filename: filename.to_string(),
            size: content.len() as u64,
            hashes: vec![(
                "BLAKE2B".to_string(),
                hex::encode(Blake2b512::digest(content)),
            )],
        }
    }

    #[test]
    fn dist_digests_first_manifest_wins_on_duplicate_filename() {
        let mut digests = DistDigests::new();
        let first = Manifest {
            entries: vec![dist_entry("foo.tar.gz", b"first")],
        };
        let second = Manifest {
            entries: vec![dist_entry("foo.tar.gz", b"second-and-longer")],
        };
        digests.extend_from_manifest(&first);
        digests.extend_from_manifest(&second);

        assert_eq!(digests.len(), 1);
        let ManifestEntry::Dist { size, .. } = digests.get("foo.tar.gz").unwrap() else {
            unreachable!()
        };
        assert_eq!(*size, "first".len() as u64);
    }

    #[test]
    fn dist_digests_skips_non_dist_entries() {
        let mut digests = DistDigests::new();
        digests.extend_from_manifest(&Manifest {
            entries: vec![ManifestEntry::Timestamp {
                value: "2026-01-01T00:00:00Z".to_string(),
            }],
        });
        assert!(digests.is_empty());
    }

    #[tokio::test]
    async fn trust_existing_size_accepts_wrong_content_right_size() {
        let tmp = tempfile::tempdir().unwrap();
        let distdir = Utf8Path::from_path(tmp.path()).unwrap().to_path_buf();
        let filename = "foo.tar.gz";
        let real_content = b"the real content".to_vec();
        // Present file has the right *size* but different content (a
        // single flipped byte, guaranteeing identical length) — under
        // trust_existing_size this must still count as present, without ever
        // touching `df.urls` (left empty here to prove no network path runs).
        let mut wrong_content = real_content.clone();
        wrong_content[0] = b'X';
        std::fs::write(distdir.join(filename).as_std_path(), &wrong_content).unwrap();

        let mut digests = DistDigests::new();
        digests.extend_from_manifest(&Manifest {
            entries: vec![dist_entry(filename, &real_content)],
        });

        let fetcher = Fetcher::new(
            distdir,
            FetchConfig {
                trust_existing_size: true,
                ..Default::default()
            },
        );
        let df = Distfile {
            filename: filename.to_string(),
            urls: vec![],
            restriction: None,
        };
        let status = fetcher.fetch_distfile_digests(&df, &digests).await.unwrap();
        assert_eq!(status, FetchStatus::AlreadyPresent);
    }

    #[tokio::test]
    async fn trust_existing_size_false_falls_through_to_a_fetch_attempt() {
        let tmp = tempfile::tempdir().unwrap();
        let distdir = Utf8Path::from_path(tmp.path()).unwrap().to_path_buf();
        let filename = "foo.tar.gz";
        let real_content = b"the real content".to_vec();
        // Same length, single flipped byte — same rationale as the
        // trust_existing_size test above.
        let mut wrong_content = real_content.clone();
        wrong_content[0] = b'X';
        std::fs::write(distdir.join(filename).as_std_path(), &wrong_content).unwrap();

        let mut digests = DistDigests::new();
        digests.extend_from_manifest(&Manifest {
            entries: vec![dist_entry(filename, &real_content)],
        });

        // Default config: full hash verification, wrong content fails it, and
        // with no URLs to try next there is nothing left but to report failure
        // — never a silent "AlreadyPresent" on bad content.
        let fetcher = Fetcher::new(distdir, FetchConfig::default());
        let df = Distfile {
            filename: filename.to_string(),
            urls: vec![],
            restriction: None,
        };
        let err = fetcher
            .fetch_distfile_digests(&df, &digests)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::AllFailed { .. }));
    }

    #[test]
    fn is_partial_download_name_recognizes_the_suffix() {
        assert!(is_partial_download_name("foo.tar.gz.__download__"));
        assert!(!is_partial_download_name("foo.tar.gz"));
        assert!(!is_partial_download_name("layout.conf"));
    }

    fn command_fetcher(distdir: &Utf8Path, script: &str) -> Fetcher {
        Fetcher::new(
            distdir.to_path_buf(),
            FetchConfig::from_make_conf(Some(script.to_string()), None),
        )
    }

    fn one_url(filename: &str) -> Distfile {
        Distfile {
            filename: filename.to_string(),
            urls: vec![format!("http://example.invalid/{filename}")],
            restriction: None,
        }
    }

    #[tokio::test]
    async fn fetchcommand_writes_the_download_name_then_renames() {
        let tmp = tempfile::tempdir().unwrap();
        let distdir = Utf8Path::from_path(tmp.path()).unwrap().to_path_buf();
        let filename = "foo.tar.gz";
        let digests = DistDigests::from(&Manifest {
            entries: vec![dist_entry(filename, b"payload")],
        });
        let fetcher = command_fetcher(
            &distdir,
            r#"case "${FILE}" in *.__download__) printf payload > "${DISTDIR}/${FILE}" ;; *) exit 1 ;; esac"#,
        );
        let status = fetcher
            .fetch_distfile_digests(&one_url(filename), &digests)
            .await
            .unwrap();
        assert_eq!(status, FetchStatus::Downloaded);
        assert_eq!(std::fs::read(distdir.join(filename)).unwrap(), b"payload");
        assert!(!download_path(&distdir.join(filename)).exists());
    }

    // Another fetcher's file under the final name must survive a download of ours that fails to verify.
    #[tokio::test]
    async fn failed_verify_never_touches_the_final_name() {
        let tmp = tempfile::tempdir().unwrap();
        let distdir = Utf8Path::from_path(tmp.path()).unwrap().to_path_buf();
        let filename = "foo.tar.gz";
        let dest = distdir.join(filename);
        std::fs::write(&dest, b"other").unwrap();
        let digests = DistDigests::from(&Manifest {
            entries: vec![dist_entry(filename, b"payload")],
        });
        let fetcher = command_fetcher(&distdir, r#"printf truncat > "${DISTDIR}/${FILE}""#);
        let err = fetcher
            .fetch_distfile_digests(&one_url(filename), &digests)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Verify { .. }));
        assert_eq!(std::fs::read(&dest).unwrap(), b"other");
        assert!(!download_path(&dest).exists());
    }

    // Regression: two independent fetch calls for the same filename (e.g. one
    // package built for two different roots) used to race writing the same
    // DISTDIR path with no coordination at all. `lock_distfile` must actually
    // block a second acquisition while the first is held.
    #[tokio::test]
    async fn lock_distfile_serializes_concurrent_acquisitions() {
        let tmp = tempfile::tempdir().unwrap();
        let distdir = Utf8Path::from_path(tmp.path()).unwrap().to_path_buf();
        let dest = distdir.join("foo.tar.gz");

        let first = lock_distfile(&dest).await.expect("first lock");
        let dest2 = dest.clone();
        let second = tokio::spawn(async move { lock_distfile(&dest2).await });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            !second.is_finished(),
            "second acquisition must block while the first holds the lock"
        );

        drop(first);
        let acquired = second.await.unwrap();
        assert!(
            acquired.is_some(),
            "second acquisition must succeed once the first releases"
        );
        let lock_file = distdir.join(".foo.tar.gz.portage_lockfile");
        assert!(lock_file.exists());
        drop(acquired);
        assert!(!lock_file.exists(), "the lock file is removed on release");
    }

    #[tokio::test]
    async fn restrict_fetch_without_exemption_is_fetch_restricted() {
        let tmp = tempfile::tempdir().unwrap();
        let distdir = Utf8Path::from_path(tmp.path()).unwrap().to_path_buf();
        let fetcher = Fetcher::new(distdir, FetchConfig::default()).with_restrict(RestrictGate {
            fetch: true,
            mirror: true,
        });
        let df = Distfile {
            filename: "secret.tar.gz".to_string(),
            urls: vec!["http://127.0.0.1:1/secret.tar.gz".to_string()],
            restriction: None,
        };
        let status = fetcher
            .fetch_distfile_digests(&df, &DistDigests::new())
            .await
            .unwrap();
        assert_eq!(status, FetchStatus::FetchRestricted);
    }

    #[tokio::test]
    async fn fetch_plus_exempts_restrict_fetch() {
        let tmp = tempfile::tempdir().unwrap();
        let distdir = Utf8Path::from_path(tmp.path()).unwrap().to_path_buf();
        let fetcher = Fetcher::new(distdir, FetchConfig::default()).with_restrict(RestrictGate {
            fetch: true,
            mirror: true,
        });
        let df = Distfile {
            filename: "secret.tar.gz".to_string(),
            urls: vec![],
            restriction: Some("fetch".to_string()),
        };
        let err = fetcher
            .fetch_distfile_digests(&df, &DistDigests::new())
            .await
            .unwrap_err();
        assert!(
            matches!(err, Error::AllFailed { .. }),
            "fetch+ must attempt a download, not return FetchRestricted"
        );
    }

    #[tokio::test]
    async fn missing_dist_entry_is_refused_unless_allowed() {
        let tmp = tempfile::tempdir().unwrap();
        let distdir = Utf8Path::from_path(tmp.path()).unwrap().to_path_buf();
        std::fs::write(distdir.join("foo.tar.gz"), b"placed").unwrap();
        let strict = FetchConfig {
            allow_missing_digests: false,
            ..Default::default()
        };
        let err = Fetcher::new(distdir.clone(), strict)
            .fetch_distfile_digests(&one_url("foo.tar.gz"), &DistDigests::new())
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Verify { .. }));

        let status = Fetcher::new(distdir, FetchConfig::default())
            .fetch_distfile_digests(&one_url("foo.tar.gz"), &DistDigests::new())
            .await
            .unwrap();
        assert_eq!(status, FetchStatus::AlreadyPresent);
    }

    #[tokio::test]
    async fn empty_unverified_file_is_not_present() {
        let tmp = tempfile::tempdir().unwrap();
        let distdir = Utf8Path::from_path(tmp.path()).unwrap().to_path_buf();
        std::fs::write(distdir.join("foo.tar.gz"), b"").unwrap();
        let df = Distfile {
            filename: "foo.tar.gz".to_string(),
            urls: vec![],
            restriction: None,
        };
        let err = Fetcher::new(distdir, FetchConfig::default())
            .fetch_distfile_digests(&df, &DistDigests::new())
            .await
            .unwrap_err();
        assert!(matches!(err, Error::AllFailed { .. }));
    }

    #[tokio::test]
    async fn restrict_fetch_already_present_is_ok() {
        let tmp = tempfile::tempdir().unwrap();
        let distdir = Utf8Path::from_path(tmp.path()).unwrap().to_path_buf();
        let filename = "secret.tar.gz";
        std::fs::write(distdir.join(filename).as_std_path(), b"placed").unwrap();
        let fetcher = Fetcher::new(distdir, FetchConfig::default()).with_restrict(RestrictGate {
            fetch: true,
            mirror: true,
        });
        let df = Distfile {
            filename: filename.to_string(),
            urls: vec!["http://127.0.0.1:1/secret.tar.gz".to_string()],
            restriction: None,
        };
        let status = fetcher
            .fetch_distfile_digests(&df, &DistDigests::new())
            .await
            .unwrap();
        assert_eq!(status, FetchStatus::AlreadyPresent);
    }
}
