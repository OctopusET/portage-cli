//! portage's `preserve-libs`: never physically delete a shared library that
//! another still-installed object's `NEEDED.ELF.2` genuinely requires and
//! that no other installed copy still provides — matching real portage's
//! `_find_libs_to_preserve`/`PreservedLibsRegistry`
//! (`portage/dbapi/vartree.py`, `util/_dyn_libs/{LinkageMapELF,
//! PreservedLibsRegistry}.py`), simplified to match by (multilib category,
//! soname) alone rather than full RPATH/RUNPATH-directory resolution. That
//! can only make `em` preserve a file real portage would have deleted,
//! never the reverse — the safe direction for a deletion guard.
//!
//! Hooked into `ebuild::unmerge_package`, the one function both `-C` and
//! ordinary version-bump replacement funnel through, so both benefit.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use camino::{Utf8Path, Utf8PathBuf};

use portage_atom::{Cpv, Dep};
use portage_vdb::{ContentsEntry, ContentsKind, InstalledPackage, Vdb};

use crate::elfscan;
use crate::util::write_atomic;

/// One parsed `NEEDED.ELF.2` line: `MACHINE;path;SONAME;RPATH;needed,csv;category`
/// — the exact format `elfscan::scan_image` writes. `pub(crate)` so
/// `revdep.rs` can reuse the same parsing instead of re-implementing it.
#[derive(Clone)]
pub(crate) struct NeededRecord {
    pub(crate) category: String,
    pub(crate) path: Utf8PathBuf,
    pub(crate) soname: Option<String>,
    pub(crate) needed: Vec<String>,
}

fn parse_needed_line(line: &str) -> Option<NeededRecord> {
    let mut parts = line.splitn(6, ';');
    let _machine = parts.next()?;
    let path = parts.next()?;
    let soname = parts.next()?;
    let _rpath = parts.next()?;
    let needed = parts.next()?;
    let category = parts.next()?;
    Some(NeededRecord {
        category: category.to_string(),
        path: Utf8PathBuf::from(path),
        soname: (!soname.is_empty()).then(|| soname.to_string()),
        needed: needed
            .split(',')
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
    })
}

pub(crate) fn package_needed(pkg: &InstalledPackage) -> Vec<NeededRecord> {
    pkg.field("NEEDED.ELF.2")
        .ok()
        .flatten()
        .map(|content| content.lines().filter_map(parse_needed_line).collect())
        .unwrap_or_default()
}

/// One `cp:slot`'s registered preserved-lib set
///
/// `paths` are stored as plain `String`s (not `Utf8PathBuf`) since `camino` doesn't
/// implement `serde` traits without an extra feature flag on a shared workspace dep, so they are
/// converted at the [`PreservedLibsRegistry`] API boundary instead.
#[derive(Debug, Default, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(from = "WireEntry", into = "(String, String, Vec<String>)")]
struct RegistryEntry {
    cpv: String,
    counter: u64,
    paths: Vec<String>,
}

/// Portage's `[cpv, "counter", [paths]]`, or the object em wrote before matching it
#[derive(serde::Deserialize)]
#[serde(untagged)]
enum WireEntry {
    Portage(String, serde_json::Value, Vec<String>),
    Legacy {
        cpv: String,
        counter: u64,
        paths: Vec<String>,
    },
}

impl From<WireEntry> for RegistryEntry {
    fn from(wire: WireEntry) -> Self {
        match wire {
            WireEntry::Portage(cpv, counter, paths) => {
                let counter = match counter {
                    serde_json::Value::String(s) => s.trim().parse().unwrap_or(0),
                    other => other.as_u64().unwrap_or(0),
                };
                Self {
                    cpv,
                    counter,
                    paths,
                }
            }
            WireEntry::Legacy {
                cpv,
                counter,
                paths,
            } => Self {
                cpv,
                counter,
                paths,
            },
        }
    }
}

impl From<RegistryEntry> for (String, String, Vec<String>) {
    fn from(e: RegistryEntry) -> Self {
        (e.cpv, e.counter.to_string(), e.paths)
    }
}

/// `var/lib/portage/preserved_libs_registry`, in portage's own format: a JSON map of
/// `"cp:slot" -> [cpv, "counter", [paths]]`, so em and emerge share one registry.
pub struct PreservedLibsRegistry {
    path: Utf8PathBuf,
    data: HashMap<String, RegistryEntry>,
    /// The file as loaded, so [`Self::store`] can send only what changed here
    /// rather than this registry's whole view of it.
    original: HashMap<String, RegistryEntry>,
}

impl PreservedLibsRegistry {
    /// Load the registry under `root` (the merge root / EROOT), or start
    /// empty if it doesn't exist yet or fails to parse.
    pub fn load(root: &Utf8Path) -> Self {
        Self::load_path(&root.join("var/lib/portage/preserved_libs_registry"))
    }

    fn load_path(path: &Utf8Path) -> Self {
        let data = Self::read(path).unwrap_or_default();
        Self {
            path: path.to_owned(),
            original: data.clone(),
            data,
        }
    }

    /// The file's entries; `None` when it exists but does not parse (empty means no entries)
    fn read(path: &Utf8Path) -> Option<HashMap<String, RegistryEntry>> {
        match std::fs::read_to_string(path) {
            Ok(s) if s.trim().is_empty() => Some(HashMap::new()),
            Ok(s) => serde_json::from_str(&s).ok(),
            Err(_) => Some(HashMap::new()),
        }
    }

    /// Whether the registry has no tracked preserved paths.
    pub(crate) fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Write the registry back out
    ///
    /// Best-effort: a failure here shouldn't abort an unmerge that already completed on disk.
    pub fn store(&self) {
        // Re-read and apply only this registry's delta. Writing `self.data`
        // wholesale would drop anything a `__worker` child registered through
        // its own one-shot path since this load; the delta also leaves a
        // concurrent writer's removals and updates intact, which a plain
        // merge of `self.data` over the file would not.
        let Some(mut disk) = Self::read(&self.path) else {
            crate::style::warn_line!("not rewriting unparsable {}", self.path);
            return;
        };
        for key in self.original.keys() {
            if !self.data.contains_key(key) {
                disk.remove(key);
            }
        }
        for (key, entry) in &self.data {
            if self.original.get(key) != Some(entry) {
                disk.insert(key.clone(), entry.clone());
            }
        }
        // The replacement keeps the old file's mode and owner (portage creates it 0660 root:portage).
        let old = std::fs::metadata(&self.path);
        match serde_json::to_string_pretty(&disk) {
            Ok(json) => {
                if let Err(e) = write_atomic(&self.path, json) {
                    crate::style::warn_line!("could not write {}: {e:#}", self.path);
                } else if let Ok(old) = old {
                    use std::os::unix::fs::MetadataExt;
                    let _ = std::os::unix::fs::chown(&self.path, Some(old.uid()), Some(old.gid()));
                    let _ = std::fs::set_permissions(&self.path, old.permissions());
                } else {
                    let perms = std::os::unix::fs::PermissionsExt::from_mode(0o644);
                    let _ = std::fs::set_permissions(&self.path, perms);
                }
            }
            Err(e) => crate::style::warn_line!("could not serialize preserved_libs_registry: {e}"),
        }
    }

    /// Register (or clear, if `paths` is empty) the preserved-lib set for
    /// one `cp:slot` — matching real portage's `register`/`unregister`
    /// (the same call; empty paths means unregister).
    pub fn register(&mut self, cpv: &Cpv, slot: &str, counter: u64, paths: Vec<Utf8PathBuf>) {
        let key = format!("{}:{slot}", cpv.cpn);
        if paths.is_empty() {
            self.data.remove(&key);
        } else {
            self.data.insert(
                key,
                RegistryEntry {
                    cpv: cpv.to_string(),
                    counter,
                    paths: paths.into_iter().map(Utf8PathBuf::into_string).collect(),
                },
            );
        }
    }

    /// Reclaim preserved-libs after a merge or unmerge batch:
    ///
    /// 1. Drop registry paths that a live package's CONTENTS now owns (do not
    ///    delete — the live package owns them).
    /// 2. Delete remaining preserved files whose soname is no longer needed by
    ///    any live package's `NEEDED.ELF.2`, and drop them from the registry.
    ///
    /// Best-effort: deletion failures are logged and the path stays registered.
    pub fn reclaim(&mut self, vdb: &Vdb, root: &Utf8Path) {
        if self.data.is_empty() {
            return;
        }
        self.reclaim_provided(vdb);
        self.prune_unneeded(vdb, root);
    }

    /// Drop registry paths that a currently installed package's CONTENTS now
    /// owns (without deleting — the live package owns them).
    pub fn reclaim_provided(&mut self, vdb: &Vdb) {
        if self.data.is_empty() {
            return;
        }
        let mut live_paths: HashSet<Utf8PathBuf> = HashSet::new();
        for pkg in vdb.packages() {
            if let Ok(contents) = pkg.contents() {
                for e in contents {
                    if matches!(e.kind, ContentsKind::Obj | ContentsKind::Sym) {
                        live_paths.insert(e.path);
                    }
                }
            }
        }
        let mut remove_keys = Vec::new();
        let mut updates: Vec<(String, Vec<String>)> = Vec::new();
        for (key, entry) in &self.data {
            let remaining: Vec<String> = entry
                .paths
                .iter()
                .filter(|p| !live_paths.contains(Utf8Path::new(p.as_str())))
                .cloned()
                .collect();
            if remaining.len() == entry.paths.len() {
                continue;
            }
            let reclaimed = entry.paths.len() - remaining.len();
            println!(">>> preserved-libs: reclaimed {reclaimed} path(s) from {key}");
            if remaining.is_empty() {
                remove_keys.push(key.clone());
            } else {
                updates.push((key.clone(), remaining));
            }
        }
        self.apply_prune(remove_keys, updates);
    }

    /// Delete preserved files no live package still needs via `DT_NEEDED`, directly or
    /// through other preserved files it keeps
    fn prune_unneeded(&mut self, vdb: &Vdb, root: &Utf8Path) {
        if self.data.is_empty() {
            return;
        }
        // (multilib category, soname) still required by something installed.
        let mut needed: HashSet<(String, String)> = HashSet::new();
        for pkg in vdb.packages() {
            for rec in package_needed(&pkg) {
                for n in rec.needed {
                    needed.insert((rec.category.clone(), n));
                }
            }
        }
        // Preserved libraries by path, and the subset `needed` reaches.
        let mut sonamed: HashMap<&str, ((String, String), Vec<String>)> = HashMap::new();
        for path_s in self.data.values().flat_map(|e| &e.paths) {
            let abs = root.join(path_s.trim_start_matches('/'));
            if let Some(info) = elfscan::scan_file(abs.as_std_path())
                && let Some(soname) = info.soname
            {
                sonamed.insert(path_s, ((info.category, soname), info.needed));
            }
        }
        let mut reached: HashSet<&str> = HashSet::new();
        loop {
            let before = reached.len();
            for (path, (key, deps)) in &sonamed {
                if !reached.contains(path) && needed.contains(key) {
                    reached.insert(path);
                    needed.extend(deps.iter().map(|n| (key.0.clone(), n.clone())));
                }
            }
            if reached.len() == before {
                break;
            }
        }

        let mut remove_keys = Vec::new();
        let mut updates: Vec<(String, Vec<String>)> = Vec::new();
        for (key, entry) in &self.data {
            let mut remaining = Vec::new();
            for path_s in &entry.paths {
                let path = Utf8Path::new(path_s);
                let rel = path_s.trim_start_matches('/');
                let abs = root.join(rel);
                if !abs.exists() {
                    println!(">>> preserved-libs: dropped missing {path}");
                    continue;
                }
                // Unreadable, non-ELF or soname-less: keep (safe direction).
                let still_needed =
                    !sonamed.contains_key(path_s.as_str()) || reached.contains(path_s.as_str());
                if still_needed {
                    remaining.push(path_s.clone());
                    continue;
                }
                // Orphan preserved file — remove from disk.
                match std::fs::remove_file(abs.as_std_path()) {
                    Ok(()) => {
                        println!(">>> preserved-libs: removed unused {path}");
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        println!(">>> preserved-libs: dropped missing {path}");
                    }
                    Err(e) => {
                        crate::style::warn_line!("preserved-libs: could not remove {path}: {e}");
                        remaining.push(path_s.clone());
                    }
                }
            }
            if remaining.len() == entry.paths.len() {
                continue;
            }
            if remaining.is_empty() {
                remove_keys.push(key.clone());
            } else {
                updates.push((key.clone(), remaining));
            }
        }
        self.apply_prune(remove_keys, updates);
    }

    /// Apply a prune pass's decisions: drop the fully-emptied keys, then
    /// shrink the partially-pruned ones to their surviving paths.
    fn apply_prune(&mut self, remove_keys: Vec<String>, updates: Vec<(String, Vec<String>)>) {
        for key in remove_keys {
            self.data.remove(&key);
        }
        for (key, remaining) in updates {
            if let Some(entry) = self.data.get_mut(&key) {
                entry.paths = remaining;
            }
        }
    }

    /// Every path currently tracked as preserved, across all packages —
    /// these no longer appear in any live package's CONTENTS, so callers
    /// needing their link metadata must re-scan them directly
    /// ([`elfscan::scan_file`]).
    pub(crate) fn all_paths(&self) -> impl Iterator<Item = Utf8PathBuf> {
        self.data
            .values()
            .flat_map(|e| e.paths.iter())
            .map(Utf8PathBuf::from)
    }
}

/// The `@preserved-rebuild` special set: every installed package whose own
/// `NEEDED.ELF.2` still requires a soname that no live package provides
/// anymore, but that `registry` is only keeping on disk because of that
/// requirement — real portage's `PreservedLibraryConsumerSet`. Simplified
/// the same way [`find_libs_to_preserve`] already is: matched by (multilib
/// category, soname) alone, no RPATH/RUNPATH directory disambiguation.
///
/// That costs little in practice — a registry entry only exists because
/// the replacing package's own copy dropped that exact soname, so a live
/// duplicate provider of the identical soname essentially never coexists.
///
/// A preserved lib is never counted as its own consumer, matching real
/// portage's `consumers.difference_update(libs)`.
///
/// `root` resolves each registry path to a real file, same convention
/// [`build_link_graph`] already uses for its own registry rescan.
pub fn preserved_rebuild_atoms(
    vdb: &Vdb,
    registry: &PreservedLibsRegistry,
    root: &Utf8Path,
) -> Vec<Dep> {
    let preserved_paths: HashSet<Utf8PathBuf> = registry.all_paths().collect();
    if preserved_paths.is_empty() {
        return Vec::new();
    }

    let mut preserved_provides: HashSet<(String, String)> = HashSet::new();
    for path in &preserved_paths {
        let rel = path.as_str().trim_start_matches('/');
        if let Some(info) = elfscan::scan_file(root.join(rel).as_std_path())
            && let Some(soname) = info.soname
        {
            preserved_provides.insert((info.category, soname));
        }
    }
    if preserved_provides.is_empty() {
        return Vec::new();
    }

    let mut atoms: BTreeMap<String, Dep> = BTreeMap::new();
    for pkg in vdb.packages() {
        for rec in package_needed(&pkg) {
            if preserved_paths.contains(&rec.path) {
                continue;
            }
            let consumes_preserved = rec
                .needed
                .iter()
                .any(|n| preserved_provides.contains(&(rec.category.clone(), n.clone())));
            if !consumes_preserved {
                continue;
            }
            let Ok(slot) = pkg.slot_main() else { continue };
            let text = format!("{}:{slot}", pkg.cpv().cpn);
            if let std::collections::btree_map::Entry::Vacant(e) = atoms.entry(text.clone())
                && let Ok(dep) = Dep::parse(&text)
            {
                e.insert(dep);
            }
        }
    }
    atoms.into_values().collect()
}

/// A library kept on disk because [`find_libs_to_preserve`] found it still
/// has external consumers.
pub struct PreserveEntry {
    pub path: Utf8PathBuf,
    /// The soname symlink pointing at `path`, if one exists in the same
    /// package's contents (kept alongside — it's the only symlink form a
    /// consumer's dynamic loader actually looks up by name).
    pub soname_symlink: Option<Utf8PathBuf>,
    /// Files (belonging to other, still-installed packages) whose
    /// `DT_NEEDED` requires this library's soname — for the user-facing
    /// report.
    pub consumers: Vec<Utf8PathBuf>,
}

/// The workspace-wide `(category, soname)` provider/consumer index —
/// everything [`find_libs_to_preserve`] needs to decide what to preserve,
/// minus the packages currently being replaced. Expensive to build (a full
/// VDB scan), cheap to query and update as a merge changes its package set.
pub struct LinkGraph {
    providers: HashSet<(String, String)>,
    consumer_files: HashMap<(String, String), Vec<Utf8PathBuf>>,
    provider_counts: HashMap<(String, String), usize>,
    package_records: HashMap<Cpv, Vec<NeededRecord>>,
    preserved_providers: HashSet<(String, String)>,
}

impl LinkGraph {
    fn new() -> Self {
        Self {
            providers: HashSet::new(),
            consumer_files: HashMap::new(),
            provider_counts: HashMap::new(),
            package_records: HashMap::new(),
            preserved_providers: HashSet::new(),
        }
    }

    fn refresh_providers(&mut self) {
        self.providers.clear();
        self.providers.extend(self.provider_counts.keys().cloned());
        self.providers
            .extend(self.preserved_providers.iter().cloned());
    }

    /// Add a package's dynamic-link records to the graph.
    pub(crate) fn add_package(&mut self, pkg: &InstalledPackage) {
        self.remove_package(pkg.cpv());
        let records = package_needed(pkg);
        for rec in &records {
            if let Some(soname) = &rec.soname {
                let key = (rec.category.clone(), soname.clone());
                *self.provider_counts.entry(key.clone()).or_insert(0) += 1;
                self.providers.insert(key);
            }
            for needed in &rec.needed {
                self.consumer_files
                    .entry((rec.category.clone(), needed.clone()))
                    .or_default()
                    .push(rec.path.clone());
            }
        }
        self.package_records.insert(pkg.cpv().clone(), records);
    }

    /// Remove a package's dynamic-link records from the graph.
    pub(crate) fn remove_package(&mut self, cpv: &Cpv) {
        let Some(records) = self.package_records.remove(cpv) else {
            return;
        };
        for rec in records {
            if let Some(soname) = rec.soname {
                let key = (rec.category.clone(), soname);
                let remove_provider = if let Some(count) = self.provider_counts.get_mut(&key) {
                    *count -= 1;
                    *count == 0
                } else {
                    false
                };
                if remove_provider {
                    self.provider_counts.remove(&key);
                    if !self.preserved_providers.contains(&key) {
                        self.providers.remove(&key);
                    }
                }
            }
            for needed in rec.needed {
                let key = (rec.category.clone(), needed);
                let remove_consumers = self.consumer_files.get_mut(&key).is_some_and(|files| {
                    if let Some(index) = files.iter().position(|path| path == &rec.path) {
                        files.remove(index);
                    }
                    files.is_empty()
                });
                if remove_consumers {
                    self.consumer_files.remove(&key);
                }
            }
        }
    }

    /// Refresh the providers contributed by registry-carried preserved files.
    pub(crate) fn refresh_preserved(&mut self, registry: &PreservedLibsRegistry, root: &Utf8Path) {
        self.preserved_providers.clear();
        for path in registry.all_paths() {
            let rel = path.as_str().trim_start_matches('/');
            if let Some(info) = elfscan::scan_file(root.join(rel).as_std_path())
                && let Some(soname) = info.soname
            {
                self.preserved_providers.insert((info.category, soname));
            }
        }
        self.refresh_providers();
    }
}

/// Build the [`LinkGraph`] for a removal batch: every installed package
/// **not** in `exclude`, plus every registry-carried preserved lib from a
/// previous run (re-scanned directly via [`elfscan::scan_file`] — no
/// longer part of any live package's CONTENTS/NEEDED.ELF.2).
///
/// `exclude` is every cpv this same `-C`/depclean/replace invocation is
/// already committed to removing (so a multi-atom `em -C a b`, where `a`
/// needs a lib only `b` provides, doesn't falsely think `b` still
/// provides it).
pub fn build_link_graph(
    vdb: &Vdb,
    exclude: &HashSet<Cpv>,
    registry: &PreservedLibsRegistry,
    root: &Utf8Path,
) -> LinkGraph {
    let mut graph = LinkGraph::new();
    for pkg in vdb.packages() {
        if !exclude.contains(pkg.cpv()) {
            graph.add_package(&pkg);
        }
    }
    graph.refresh_preserved(registry, root);
    graph
}

/// Preserve-libs state retained while one merge changes several VDB packages.
///
/// Reclamation is deferred until the batch ends; keeping a preserved file on disk
/// until then is safe and lets a later replacement still use it as a provider.
pub(crate) struct MergePreserveState {
    root: Utf8PathBuf,
    vdb: Vdb,
    registry: PreservedLibsRegistry,
    graph: Option<LinkGraph>,
    dirty: bool,
}

impl MergePreserveState {
    pub(crate) fn load(root: &Utf8Path, vdb: &Vdb) -> Self {
        Self {
            root: root.to_path_buf(),
            vdb: vdb.clone(),
            registry: PreservedLibsRegistry::load(root),
            graph: None,
            dirty: false,
        }
    }

    /// Return the graph with `excluded` removed and the live registry.
    pub(crate) fn graph_and_registry(
        &mut self,
        excluded: &Cpv,
    ) -> (&LinkGraph, &mut PreservedLibsRegistry) {
        if self.graph.is_none() {
            self.graph = Some(build_link_graph(
                &self.vdb,
                &HashSet::new(),
                &self.registry,
                &self.root,
            ));
        }
        let graph = self.graph.as_mut().expect("graph initialized above");
        graph.remove_package(excluded);
        (graph, &mut self.registry)
    }

    pub(crate) fn package_removed(&mut self, cpv: &Cpv) {
        self.dirty = true;
        if let Some(graph) = &mut self.graph {
            graph.remove_package(cpv);
            graph.refresh_preserved(&self.registry, &self.root);
        }
    }

    pub(crate) fn package_added(&mut self, pkg: &InstalledPackage) {
        if let Some(graph) = &mut self.graph {
            graph.add_package(pkg);
        }
    }

    /// Reclaim and persist the registry after the merge has finished changing
    /// the VDB.
    pub(crate) fn finish(&mut self) {
        let had_entries = !self.registry.is_empty();
        self.registry.reclaim(&self.vdb, &self.root);
        if self.dirty || had_entries || !self.registry.is_empty() {
            self.registry.store();
        }
        self.graph = None;
        self.dirty = false;
    }
}

/// Which of `old_contents`' own files must survive `old_pkg`'s removal: those an object
/// outside the batch `graph` needs via `DT_NEEDED`, directly or through another preserved
/// file, with no other installed copy of that soname. Reads only `old_pkg`'s own
/// `NEEDED.ELF.2`, so one `graph` serves the whole batch (see [`build_link_graph`]).
pub fn find_libs_to_preserve(
    graph: &LinkGraph,
    old_pkg: &InstalledPackage,
    old_contents: &[ContentsEntry],
) -> Vec<PreserveEntry> {
    let records = package_needed(old_pkg);
    // External consumers are queued first, so a library both kinds need reports those.
    let mut queue: VecDeque<(&NeededRecord, Vec<Utf8PathBuf>)> = VecDeque::new();
    for rec in &records {
        let Some(soname) = &rec.soname else { continue };
        let key = (rec.category.clone(), soname.clone());
        // Another installed copy already provides it -> nothing orphaned.
        if graph.providers.contains(&key) {
            continue;
        }
        // Nobody outside this package needs it -> nothing to preserve.
        if let Some(consumers) = graph.consumer_files.get(&key) {
            queue.push_back((rec, consumers.clone()));
        }
    }

    let mut preserve = Vec::new();
    let mut seen: HashSet<&Utf8Path> = HashSet::new();
    while let Some((rec, consumers)) = queue.pop_front() {
        if !seen.insert(&rec.path) {
            continue;
        }
        let Some(entry) = old_contents
            .iter()
            .find(|e| e.kind == ContentsKind::Obj && e.path == rec.path)
        else {
            continue;
        };
        let soname = rec.soname.as_deref().unwrap_or_default();
        let soname_symlink = old_contents
            .iter()
            .find(|e| e.kind == ContentsKind::Sym && e.path.file_name() == Some(soname))
            .map(|e| e.path.clone());
        preserve.push(PreserveEntry {
            path: entry.path.clone(),
            soname_symlink,
            consumers,
        });
        for needed in &rec.needed {
            if graph
                .providers
                .contains(&(rec.category.clone(), needed.clone()))
            {
                continue;
            }
            for dep in &records {
                if dep.category == rec.category && dep.soname.as_ref() == Some(needed) {
                    queue.push_back((dep, vec![rec.path.clone()]));
                }
            }
        }
    }
    preserve
}

/// Real-portage-style report: `>>> package: <cpv>` / ` * preserved: <path>`
/// / `      used by <path> (<owning cpv>)`, shown both for a real run and
/// under `-p`.
pub fn report_preserved(cpv: &Cpv, preserved: &[PreserveEntry], vdb: &Vdb) {
    const MAX_DISPLAY: usize = 3;
    if preserved.is_empty() {
        return;
    }
    println!(">>> package: {cpv}");
    for entry in preserved {
        crate::style::einfo_line!("preserved: {}", entry.path);
        if let Some(sym) = &entry.soname_symlink {
            crate::style::einfo_line!("preserved: {sym}");
        }
        for c in entry.consumers.iter().take(MAX_DISPLAY) {
            let owner = vdb
                .owner(c)
                .map(|p| p.cpv().to_string())
                .unwrap_or_else(|| "unknown".to_string());
            println!("      used by {c} ({owner})");
        }
        if entry.consumers.len() > MAX_DISPLAY {
            println!(
                "      used by {} other file(s)",
                entry.consumers.len() - MAX_DISPLAY
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contents(entries: &[(ContentsKind, &str, Option<&str>)]) -> Vec<ContentsEntry> {
        entries
            .iter()
            .map(|(kind, path, target)| ContentsEntry {
                kind: *kind,
                path: Utf8PathBuf::from(*path),
                md5: None,
                mtime: None,
                target: target.map(Utf8PathBuf::from),
            })
            .collect()
    }

    #[test]
    fn parse_needed_line_roundtrips_scan_image_format() {
        let rec = parse_needed_line("X86_64;/usr/lib64/libfoo.so.1;libfoo.so.1;;libc.so.6;x86_64")
            .unwrap();
        assert_eq!(rec.category, "x86_64");
        assert_eq!(rec.path, Utf8PathBuf::from("/usr/lib64/libfoo.so.1"));
        assert_eq!(rec.soname.as_deref(), Some("libfoo.so.1"));
        assert_eq!(rec.needed, vec!["libc.so.6"]);
    }

    #[test]
    fn parse_needed_line_handles_empty_soname_and_needed() {
        let rec = parse_needed_line("X86_64;/usr/bin/tool;;;;x86_64").unwrap();
        assert_eq!(rec.soname, None);
        assert!(rec.needed.is_empty());
    }

    #[test]
    fn registry_register_then_unregister_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::try_from(dir.path().to_owned()).unwrap();
        let cpv = Cpv::parse("sys-libs/foo-1.0").unwrap();

        let mut reg = PreservedLibsRegistry::load(&root);
        reg.register(
            &cpv,
            "0",
            1,
            vec![Utf8PathBuf::from("/usr/lib64/libfoo.so.1")],
        );
        reg.store();

        let reloaded = PreservedLibsRegistry::load(&root);
        assert_eq!(reloaded.all_paths().count(), 1);

        let mut reg2 = PreservedLibsRegistry::load(&root);
        reg2.register(&cpv, "0", 1, vec![]);
        reg2.store();
        let reloaded2 = PreservedLibsRegistry::load(&root);
        assert_eq!(reloaded2.all_paths().count(), 0);
    }

    #[test]
    fn registry_reads_and_writes_portage_entries_and_never_clobbers_garbage() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::try_from(dir.path().to_owned()).unwrap();
        let file = root.join("var/lib/portage/preserved_libs_registry");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(
            &file,
            r#"{"sys-libs/foo:0": ["sys-libs/foo-1.0", "7", ["/usr/lib64/libfoo.so.1"]]}"#,
        )
        .unwrap();
        let mut reg = PreservedLibsRegistry::load(&root);
        assert_eq!(reg.all_paths().count(), 1);
        let bar = Cpv::parse("dev-libs/bar-2").unwrap();
        reg.register(
            &bar,
            "0",
            9,
            vec![Utf8PathBuf::from("/usr/lib64/libbar.so.2")],
        );
        reg.store();
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(json["sys-libs/foo:0"][1], "7");
        assert_eq!(json["dev-libs/bar:0"][0], "dev-libs/bar-2");

        std::fs::write(&file, "not json").unwrap();
        let mut reg = PreservedLibsRegistry::load(&root);
        reg.register(
            &bar,
            "0",
            9,
            vec![Utf8PathBuf::from("/usr/lib64/libbar.so.2")],
        );
        reg.store();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "not json");
    }

    #[test]
    fn store_keeps_keys_another_load_wrote_after_this_one() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::try_from(dir.path().to_owned()).unwrap();
        let first = Cpv::parse("sys-libs/foo-1.0").unwrap();
        let second = Cpv::parse("sys-libs/bar-1.0").unwrap();
        let third = Cpv::parse("sys-libs/baz-1.0").unwrap();

        let mut older = PreservedLibsRegistry::load(&root);
        older.register(
            &first,
            "0",
            1,
            vec![Utf8PathBuf::from("/usr/lib/libfoo.so")],
        );
        older.store();

        let mut other = PreservedLibsRegistry::load(&root);
        other.register(
            &second,
            "0",
            1,
            vec![Utf8PathBuf::from("/usr/lib/libbar.so")],
        );
        other.store();

        older.register(
            &third,
            "0",
            1,
            vec![Utf8PathBuf::from("/usr/lib/libbaz.so")],
        );
        older.store();

        let reloaded = PreservedLibsRegistry::load(&root);
        let paths: Vec<_> = reloaded.all_paths().map(|p| p.to_string()).collect();
        assert!(paths.iter().any(|p| p.ends_with("libfoo.so")), "{paths:?}");
        assert!(paths.iter().any(|p| p.ends_with("libbar.so")), "{paths:?}");
        assert!(paths.iter().any(|p| p.ends_with("libbaz.so")), "{paths:?}");
    }

    #[test]
    fn reclaim_provided_drops_paths_owned_by_live_packages() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::try_from(tmp.path().to_owned()).unwrap();
        let vdb_root = root.join("var/db/pkg");
        // Live package that "provides" the previously preserved path.
        write_fake_package(
            vdb_root.as_std_path(),
            "sys-libs",
            "libfoo-2.0",
            "obj /usr/lib64/libfoo.so.1 aaaa 0\n",
            "X86_64;/usr/lib64/libfoo.so.1;libfoo.so.1;;;x86_64\n",
        );
        let mut reg = PreservedLibsRegistry::load(&root);
        reg.register(
            &Cpv::parse("sys-libs/libfoo-1.0").unwrap(),
            "0",
            1,
            vec![
                Utf8PathBuf::from("/usr/lib64/libfoo.so.1"),
                Utf8PathBuf::from("/usr/lib64/liborphan.so.1"),
            ],
        );
        let vdb = Vdb::open(&vdb_root).unwrap();
        reg.reclaim(&vdb, &root);
        // libfoo path reclaimed (live-owned); orphan has no on-disk ELF so
        // prune_unneeded keeps it (safe direction) unless missing entirely.
        let paths: HashSet<_> = reg.all_paths().collect();
        assert!(!paths.contains(&Utf8PathBuf::from("/usr/lib64/libfoo.so.1")));
        // Missing file is dropped by prune_unneeded's NotFound branch.
        assert!(!paths.contains(&Utf8PathBuf::from("/usr/lib64/liborphan.so.1")));
    }

    #[test]
    fn reclaim_keeps_what_a_kept_preserved_lib_needs() {
        // libc and the loader it needs, as mapped into this test binary.
        let maps = std::fs::read_to_string("/proc/self/maps").unwrap_or_default();
        let mapped: Vec<&str> = maps
            .lines()
            .filter_map(|l| l.split_whitespace().nth(5))
            .collect();
        let host = || {
            let libc = *mapped.iter().find(|p| p.ends_with("/libc.so.6"))?;
            let info = elfscan::scan_file(std::path::Path::new(libc))?;
            let loader = *info.needed.iter().find_map(|n| {
                mapped
                    .iter()
                    .find(|p| Utf8Path::new(p).file_name() == Some(n.as_str()))
            })?;
            Some((libc, loader, info.category))
        };
        let Some((libc, loader, category)) = host() else {
            let glibc = cfg!(all(target_os = "linux", target_env = "gnu"));
            assert!(!glibc, "libc or its loader not found in this glibc process");
            return;
        };

        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::try_from(tmp.path().to_owned()).unwrap();
        std::fs::create_dir_all(root.join("opt/old")).unwrap();
        let mut paths = Vec::new();
        for (src, name) in [(libc, "libc.so.6"), (loader, "loader.so")] {
            std::fs::copy(src, root.join("opt/old").join(name)).unwrap();
            paths.push(Utf8PathBuf::from(format!("/opt/old/{name}")));
        }
        let vdb_root = root.join("var/db/pkg");
        write_fake_package(
            vdb_root.as_std_path(),
            "app-misc",
            "consumer-1.0",
            "obj /usr/bin/consumer bbbb 0\n",
            &format!("X86_64;/usr/bin/consumer;;;libc.so.6;{category}\n"),
        );
        let mut reg = PreservedLibsRegistry::load(&root);
        reg.register(&Cpv::parse("sys-libs/old-1.0").unwrap(), "0", 1, paths);
        reg.reclaim(&Vdb::open(&vdb_root).unwrap(), &root);
        let kept: HashSet<_> = reg.all_paths().map(|p| p.into_string()).collect();
        assert!(kept.contains("/opt/old/libc.so.6"), "{kept:?}");
        assert!(kept.contains("/opt/old/loader.so"), "{kept:?}");
    }

    #[test]
    fn reclaim_drops_missing_orphan_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::try_from(tmp.path().to_owned()).unwrap();
        let mut reg = PreservedLibsRegistry::load(&root);
        reg.register(
            &Cpv::parse("sys-libs/old-1.0").unwrap(),
            "0",
            1,
            vec![Utf8PathBuf::from("/usr/lib64/gone.so.1")],
        );
        // Empty VDB — nothing needs the path, and the file is missing.
        let vdb_root = root.join("var/db/pkg");
        std::fs::create_dir_all(&vdb_root).unwrap();
        let vdb = Vdb::open(&vdb_root).unwrap();
        reg.reclaim(&vdb, &root);
        assert_eq!(reg.all_paths().count(), 0);
    }

    #[test]
    fn contents_shape_is_well_formed() {
        // Exercises the `contents()` test helper itself so it's covered
        // independently of the VDB-backed tests below.
        let c = contents(&[(ContentsKind::Dir, "/usr/lib64", None)]);
        assert_eq!(c.len(), 1);
    }

    // Write one fake installed package into `vdb_root/<cat>/<pf>/`: a
    // `CONTENTS` file (real portage line format) and a `NEEDED.ELF.2` file
    // (the exact format [`parse_needed_line`] parses).
    fn write_fake_package(
        vdb_root: &std::path::Path,
        cat: &str,
        pf: &str,
        contents_lines: &str,
        needed_lines: &str,
    ) {
        let dir = vdb_root.join(cat).join(pf);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("CONTENTS"), contents_lines).unwrap();
        std::fs::write(dir.join("NEEDED.ELF.2"), needed_lines).unwrap();
    }

    #[test]
    fn preserves_lib_still_needed_by_another_installed_package() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::try_from(tmp.path().to_owned()).unwrap();
        let vdb_root = root.join("var/db/pkg");

        write_fake_package(
            vdb_root.as_std_path(),
            "sys-libs",
            "libfoo-1.0",
            "obj /usr/lib64/libfoo.so.1 aaaa 0\n",
            "X86_64;/usr/lib64/libfoo.so.1;libfoo.so.1;;;x86_64\n",
        );
        write_fake_package(
            vdb_root.as_std_path(),
            "app-misc",
            "consumer-1.0",
            "obj /usr/bin/consumer bbbb 0\n",
            "X86_64;/usr/bin/consumer;;;libfoo.so.1;x86_64\n",
        );

        let vdb = Vdb::open(&vdb_root).unwrap();
        let libfoo = vdb
            .category("sys-libs")
            .unwrap()
            .package("libfoo-1.0")
            .unwrap();
        let old_contents = libfoo.contents().unwrap();
        let exclude: HashSet<Cpv> = std::iter::once(libfoo.cpv().clone()).collect();
        let registry = PreservedLibsRegistry::load(&root);

        let graph = build_link_graph(&vdb, &exclude, &registry, &root);
        let preserved = find_libs_to_preserve(&graph, &libfoo, &old_contents);

        assert_eq!(preserved.len(), 1);
        assert_eq!(
            preserved[0].path,
            Utf8PathBuf::from("/usr/lib64/libfoo.so.1")
        );
        assert_eq!(
            preserved[0].consumers,
            vec![Utf8PathBuf::from("/usr/bin/consumer")]
        );
    }

    // What removing `sys-libs/libfoo-1.0` from the VDB under `root` would preserve.
    fn preserve_libfoo(root: &Utf8Path) -> Vec<PreserveEntry> {
        let vdb = Vdb::open(root.join("var/db/pkg")).unwrap();
        let libfoo = vdb
            .category("sys-libs")
            .unwrap()
            .package("libfoo-1.0")
            .unwrap();
        let exclude: HashSet<Cpv> = std::iter::once(libfoo.cpv().clone()).collect();
        let graph = build_link_graph(&vdb, &exclude, &PreservedLibsRegistry::load(root), root);
        find_libs_to_preserve(&graph, &libfoo, &libfoo.contents().unwrap())
    }

    #[test]
    fn link_graph_tracks_package_updates_for_merge_batch() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::try_from(tmp.path().to_owned()).unwrap();
        let vdb_root = root.join("var/db/pkg");
        write_fake_package(
            vdb_root.as_std_path(),
            "sys-libs",
            "libfoo-1.0",
            "obj /usr/lib64/libfoo.so.1 aaaa 0\n",
            "X86_64;/usr/lib64/libfoo.so.1;libfoo.so.1;;;x86_64\n",
        );
        write_fake_package(
            vdb_root.as_std_path(),
            "app-misc",
            "consumer-1.0",
            "obj /usr/bin/consumer bbbb 0\n",
            "X86_64;/usr/bin/consumer;;;libfoo.so.1;x86_64\n",
        );
        let vdb = Vdb::open(&vdb_root).unwrap();
        let libfoo = vdb
            .category("sys-libs")
            .unwrap()
            .package("libfoo-1.0")
            .unwrap();
        let old_contents = libfoo.contents().unwrap();
        let mut graph = build_link_graph(
            &vdb,
            &HashSet::new(),
            &PreservedLibsRegistry::load(&root),
            &root,
        );

        graph.remove_package(libfoo.cpv());
        write_fake_package(
            vdb_root.as_std_path(),
            "app-misc",
            "consumer-2.0",
            "obj /usr/bin/consumer2 bbbb 0\n",
            "X86_64;/usr/bin/consumer2;;;libfoo.so.1;x86_64\n",
        );
        let consumer = vdb
            .category("app-misc")
            .unwrap()
            .package("consumer-2.0")
            .unwrap();
        graph.add_package(&consumer);
        assert_eq!(
            find_libs_to_preserve(&graph, &libfoo, &old_contents)
                .first()
                .map(|entry| entry.path.as_str()),
            Some("/usr/lib64/libfoo.so.1")
        );
        let first_consumer = vdb
            .category("app-misc")
            .unwrap()
            .package("consumer-1.0")
            .unwrap();
        graph.remove_package(first_consumer.cpv());
        assert_eq!(
            find_libs_to_preserve(&graph, &libfoo, &old_contents).len(),
            1
        );
        graph.remove_package(consumer.cpv());
        assert!(find_libs_to_preserve(&graph, &libfoo, &old_contents).is_empty());
        graph.add_package(&consumer);

        write_fake_package(
            vdb_root.as_std_path(),
            "sys-libs",
            "libfoo-2.0",
            "obj /usr/lib64/libfoo.so.1 cccc 0\n",
            "X86_64;/usr/lib64/libfoo.so.1;libfoo.so.1;;;x86_64\n",
        );
        let replacement = vdb
            .category("sys-libs")
            .unwrap()
            .package("libfoo-2.0")
            .unwrap();
        graph.add_package(&replacement);
        assert!(find_libs_to_preserve(&graph, &libfoo, &old_contents).is_empty());
        write_fake_package(
            vdb_root.as_std_path(),
            "sys-libs",
            "libfoo-3.0",
            "obj /usr/lib64/libfoo.so.1 dddd 0\n",
            "X86_64;/usr/lib64/libfoo.so.1;libfoo.so.1;;;x86_64\n",
        );
        let second_replacement = vdb
            .category("sys-libs")
            .unwrap()
            .package("libfoo-3.0")
            .unwrap();
        graph.add_package(&second_replacement);
        graph.remove_package(replacement.cpv());
        assert!(find_libs_to_preserve(&graph, &libfoo, &old_contents).is_empty());
        graph.remove_package(second_replacement.cpv());
        assert_eq!(
            find_libs_to_preserve(&graph, &libfoo, &old_contents).len(),
            1
        );
    }

    #[test]
    fn merge_preserve_state_persists_registry_at_batch_finish() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::try_from(tmp.path().to_owned()).unwrap();
        let vdb_root = root.join("var/db/pkg");
        std::fs::create_dir_all(&vdb_root).unwrap();
        let path = root.join("usr/lib64/libfoo.so");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"not an ELF file").unwrap();
        let vdb = Vdb::open(&vdb_root).unwrap();
        let cpv = Cpv::parse("sys-libs/foo-1.0").unwrap();
        let mut state = MergePreserveState::load(&root, &vdb);
        {
            let (_graph, registry) = state.graph_and_registry(&cpv);
            registry.register(
                &cpv,
                "0",
                1,
                vec![Utf8PathBuf::from("/usr/lib64/libfoo.so")],
            );
        }
        state.package_removed(&cpv);
        assert!(
            !root
                .join("var/lib/portage/preserved_libs_registry")
                .exists()
        );
        state.finish();

        let reloaded = PreservedLibsRegistry::load(&root);
        assert_eq!(reloaded.all_paths().count(), 1);
    }

    #[test]
    #[ignore = "explicit performance benchmark"]
    fn benchmark_reused_preserve_state() {
        use std::hint::black_box;
        use std::time::{Duration, Instant};

        for count in [32usize, 64, 128] {
            let tmp = tempfile::tempdir().unwrap();
            let root = Utf8PathBuf::try_from(tmp.path().to_owned()).unwrap();
            let vdb_root = root.join("var/db/pkg");
            for i in 0..count {
                let pf = format!("lib{i:03}-1.0");
                let path = format!("/usr/lib64/lib{i:03}.so.1");
                write_fake_package(
                    vdb_root.as_std_path(),
                    "bench-libs",
                    &pf,
                    &format!("obj {path} aaaa 0\n"),
                    &format!("X86_64;{path};lib{i:03}.so.1;;;x86_64\n"),
                );
            }
            let vdb = Vdb::open(&vdb_root).unwrap();
            let category = vdb.category("bench-libs").unwrap();
            let packages: Vec<_> = (0..count)
                .map(|i| category.package(&format!("lib{i:03}-1.0")).unwrap())
                .collect();

            let mut baseline = Vec::new();
            let mut reused = Vec::new();
            for _ in 0..3 {
                let start = Instant::now();
                for pkg in &packages {
                    let mut registry = PreservedLibsRegistry::load(&root);
                    let exclude = HashSet::from([pkg.cpv().clone()]);
                    let graph = build_link_graph(&vdb, &exclude, &registry, &root);
                    black_box(graph.providers.len());
                    registry.reclaim(&vdb, &root);
                    registry.store();
                }
                baseline.push(start.elapsed());

                let start = Instant::now();
                let mut state = MergePreserveState::load(&root, &vdb);
                for pkg in &packages {
                    {
                        let (graph, _registry) = state.graph_and_registry(pkg.cpv());
                        black_box(graph.providers.len());
                    }
                    state.package_removed(pkg.cpv());
                    state.package_added(pkg);
                }
                state.finish();
                reused.push(start.elapsed());
            }
            let median = |mut values: Vec<Duration>| {
                values.sort();
                values[values.len() / 2]
            };
            let baseline = median(baseline);
            let reused = median(reused);
            println!(
                "preserve-state {count}: baseline {baseline:?}, reused {reused:?}, reduction {:.1}%",
                (1.0 - reused.as_secs_f64() / baseline.as_secs_f64()) * 100.0
            );
        }
    }

    #[test]
    fn preserves_the_in_package_providers_of_a_preserved_lib() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::try_from(tmp.path().to_owned()).unwrap();
        let vdb_root = root.join("var/db/pkg");
        write_fake_package(
            vdb_root.as_std_path(),
            "sys-libs",
            "libfoo-1.0",
            "obj /usr/lib64/libfoo.so.1 aaaa 0\nobj /usr/lib64/libfoo-core.so.1 aaaa 0\n\
             obj /usr/lib64/libunused.so.1 aaaa 0\n",
            "X86_64;/usr/lib64/libfoo.so.1;libfoo.so.1;;libfoo-core.so.1;x86_64\n\
             X86_64;/usr/lib64/libfoo-core.so.1;libfoo-core.so.1;;;x86_64\n\
             X86_64;/usr/lib64/libunused.so.1;libunused.so.1;;;x86_64\n",
        );
        write_fake_package(
            vdb_root.as_std_path(),
            "app-misc",
            "consumer-1.0",
            "obj /usr/bin/consumer bbbb 0\n",
            "X86_64;/usr/bin/consumer;;;libfoo.so.1;x86_64\n",
        );
        let preserved = preserve_libfoo(&root);
        let got: Vec<(&str, Vec<&str>)> = preserved
            .iter()
            .map(|e| {
                (
                    e.path.as_str(),
                    e.consumers.iter().map(|c| c.as_str()).collect(),
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                ("/usr/lib64/libfoo.so.1", vec!["/usr/bin/consumer"]),
                (
                    "/usr/lib64/libfoo-core.so.1",
                    vec!["/usr/lib64/libfoo.so.1"]
                ),
            ]
        );
    }

    #[test]
    fn does_not_preserve_lib_with_no_external_consumers() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::try_from(tmp.path().to_owned()).unwrap();
        let vdb_root = root.join("var/db/pkg");

        write_fake_package(
            vdb_root.as_std_path(),
            "sys-libs",
            "libfoo-1.0",
            "obj /usr/lib64/libfoo.so.1 aaaa 0\n",
            "X86_64;/usr/lib64/libfoo.so.1;libfoo.so.1;;;x86_64\n",
        );
        assert!(preserve_libfoo(&root).is_empty());
    }

    // A real system `.so` to drive `preserved_rebuild_atoms` tests without
    // hand-synthesizing an ELF file — same precedent as `elfscan.rs`'s own
    // tests (skip, don't fail, if the host doesn't have it).
    fn real_system_lib() -> Option<(Utf8PathBuf, elfscan::ElfInfo)> {
        for candidate in ["/usr/lib64/libz.so.1", "/lib64/libz.so.1"] {
            let path = Utf8PathBuf::from(candidate);
            if let Some(info) = elfscan::scan_file(path.as_std_path())
                && info.soname.is_some()
            {
                return Some((path, info));
            }
        }
        None
    }

    #[test]
    fn preserved_rebuild_flags_a_real_consumer() {
        let Some((lib_path, info)) = real_system_lib() else {
            return;
        };
        let soname = info.soname.unwrap();

        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::try_from(tmp.path().to_owned()).unwrap();
        let vdb_root = root.join("var/db/pkg");
        write_fake_package(
            vdb_root.as_std_path(),
            "app-misc",
            "consumer-1.0",
            "obj /usr/bin/consumer bbbb 0\n",
            &format!("X86_64;/usr/bin/consumer;;;{soname};{}\n", info.category),
        );
        std::fs::write(
            vdb_root.join("app-misc").join("consumer-1.0").join("SLOT"),
            "0",
        )
        .unwrap();
        let vdb = Vdb::open(&vdb_root).unwrap();

        let mut reg = PreservedLibsRegistry::load(&root);
        reg.register(
            &Cpv::parse("sys-libs/libfoo-1.0").unwrap(),
            "0",
            1,
            vec![lib_path],
        );

        // Resolve registry paths against the real root ("/"), independent of
        // the fake VDB's own tempdir root.
        let atoms = preserved_rebuild_atoms(&vdb, &reg, Utf8Path::new("/"));
        assert_eq!(atoms.len(), 1);
        assert_eq!(atoms[0].to_string(), "app-misc/consumer:0");
    }

    #[test]
    fn preserved_rebuild_excludes_the_preserved_lib_as_its_own_consumer() {
        let Some((lib_path, info)) = real_system_lib() else {
            return;
        };
        let soname = info.soname.unwrap();

        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::try_from(tmp.path().to_owned()).unwrap();
        let vdb_root = root.join("var/db/pkg");
        // A package whose own NEEDED.ELF.2 record IS the preserved path
        // itself (self-referential, exercises only the exclusion branch).
        write_fake_package(
            vdb_root.as_std_path(),
            "sys-libs",
            "libfoo-1.0",
            "obj /usr/lib64/libz.so.1 aaaa 0\n",
            &format!("X86_64;{lib_path};{soname};;{soname};{}\n", info.category),
        );
        let vdb = Vdb::open(&vdb_root).unwrap();

        let mut reg = PreservedLibsRegistry::load(&root);
        reg.register(
            &Cpv::parse("sys-libs/libfoo-1.0").unwrap(),
            "0",
            1,
            vec![lib_path],
        );

        let atoms = preserved_rebuild_atoms(&vdb, &reg, Utf8Path::new("/"));
        assert!(atoms.is_empty());
    }

    #[test]
    fn preserved_rebuild_empty_registry_yields_no_atoms() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::try_from(tmp.path().to_owned()).unwrap();
        let vdb_root = root.join("var/db/pkg");
        std::fs::create_dir_all(&vdb_root).unwrap();
        let vdb = Vdb::open(&vdb_root).unwrap();
        let reg = PreservedLibsRegistry::load(&root);

        let atoms = preserved_rebuild_atoms(&vdb, &reg, &root);
        assert!(atoms.is_empty());
    }
}
