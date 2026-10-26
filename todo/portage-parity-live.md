# Portage parity: live harness findings (2026-10-26)

Found with `test-scripts/parity/` against stage3-amd64-openrc-20261025 and the 2026-10-25 tree.
Source-audit items are in [[portage-parity-audit]]. Status: open unless marked done.

## Resolve (`resolve-sweep.sh`)

Sample of 491 targets (every 40th cp), `-pv`, after `e4713b74`: both succeed 272 (same 144,
order 113, display 8, set 3), one side fails 4, both fail 219 (mostly REQUIRED_USE/USE changes
needed under the default profile).

- done `e4713b74`: kept installed packages were expanded with the ebuild's current-USE deps
  without `--deep` (nghttp2/nghttp3/ngtcp2 via installed curl in 25% of targets).
- Same with `--deep`: kept packages must use their VDB deps, not the ebuild's (step 2,
  `SolverInstalledPackage` gains per-class VDB deps; needs shape confirmation).
- `dev-lang/go` BDEPEND `|| ( >=dev-lang/go-1.24.6 >=dev-lang/go-bootstrap-1.24.6 )`: em picks
  the branch only the package itself satisfies, so preflight fails; emerge pulls go-bootstrap.
  Full sweep: 119 targets (every Go package). Cause: `choose_version` falls back to `max()`, the
  first branch, which only the package being built satisfies; portage demotes it as circular
  (`dep_check.py` `circular_atom`). Fix shape: demote a DEPEND/BDEPEND choice branch that depends
  on its owner node unless host-satisfied, root-stamped for cross builds.
- `|| ( a b )` whose alternative is masked or unkeyworded: `provider/mod.rs` strips the unknown
  dep from the Choice branch and keeps it as an empty branch, which `virtual_satisfied_on_broot`
  then counts as satisfied, so the BDEPEND edge vanishes (dev-java/lzma `|| ( 7zip p7zip )`,
  rust-r1 slot groups). Fix shape: drop such a branch when a sibling survives.
- dev-java/qdox <-> dev-java/jmock: em reports a hard cycle, emerge resolves it (17 targets).
- `||` choice differs: pulseaudio-daemon (emerge) vs pipewire (em) for media-libs/pulseaudio-qt,
  clang (emerge) vs gcc (em) for gnustep-apps/projectcenter, virtual/w3m pulled only by em.
- Both fail: emerge stops at the first unsatisfiable REQUIRED_USE, em completes the USE cascade
  (known for multi-target; also seen single-target).
- em prints "Installed packages kept although masked" for unrelated installed packages
  (gnupg, elfutils, libxcrypt keyworded ~amd64 after the tree moved); emerge stays silent.
  Portage never masks an installed package by KEYWORDS/PROPERTIES/RESTRICT/CHOST
  (`Package._eval_visibility`), only by package.mask, LICENSE, invalid metadata or EAPI. em
  also drops a dep satisfied only by such an installed package and fails with autounmask
  (`a_masked_installed_package_is_reported_when_nothing_replaces_it` pins exit 1); emerge
  exits 0. Resolver change, needs confirmation.
- `-v` old-version column: emerge `[4.1.2-r7:4.1.2::gentoo, ...]`, em `[4.1.2-r7, ...]`
  (`convert_myoldbest`: slot/subslot and repo at verbosity 3).
- done `7b605e56`: missing second space before the size when a line has no USE column.
- `em --color=n` rejected (emerge takes y/n).

## Merge (`merge-diff.sh`, 16 packages)

- done: VDB `USE` filtered to IUSE_EFFECTIVE (phase env too).
- done: VDB `DEFINED_PHASES` no longer includes em's default stubs.
- done: VDB `*DEPEND`/`LICENSE`/`RESTRICT`/`PROPERTIES` USE-reduced with `:=` bound (binpkg: as built).
- done: implicit `>=<libc provider>` RDEPEND for packages with NEEDED.ELF.2.
- done: VDB `CC`, `CXX`, `FEATURES`, `REPO_REVISIONS`.
- done: setgid lost on merge (chown before chmod).
- done `49fcae32`: sys-apps/pciutils `libpci.so.3.15.0` 0755 (emerge) vs 0644 (em); portage's
  install QA makes `.so` in library dirs executable and `.a`/`.la` not.
- done `40bb85d8`: econf argument order (`--libdir` after the probed flags, only for
  LIBDIR_${ABI}, under the ebuild's `--exec-prefix`/`--prefix`; CONF_PREFIX exported).
- done `6bddd350`: global counter in `var/db/pkg/COUNTER`; portage uses `/var/cache/edb/counter`.
- done: env-update writes `/etc/csh.env`, `/etc/environment.d/10-gentoo-env.conf`; content matches
  emerge except the header comment.
- done `9acddd23`: `/usr/share/info/dir` regenerated after a merge (mtimedb `info` record).
- `/var/lib/portage/config` (config-protect-if-modified md5s) never written.
- done `2cb80dc4`: `preserved_libs_registry` written 0600 in em's own JSON shape; now portage's
  `[cpv, "counter", [paths]]`, keeping the file's mode/owner, never rewriting an unparsable file.
- done: distfiles download to `.__download__` and rename after verify, under portage's distlocks lock.
- Remaining after the fix batch (17 packages): NEEDED/NEEDED.ELF.2 line order; `metadata` (portage's
  VDB field cache, safe to omit: em replaces entry dirs by rename); `/var/lib/portage/config`
  (config-protect-if-modified md5s); binpkg extraction drops owners (`tar --no-same-owner`).
- After both batches (frozen tree, 17 packages + config-protect/unmerge/binpkg scenarios): VDB keys,
  files, modes and owners match except: VDB FEATURES (D3, [[features-parity]]); binaries that embed
  `__FILE__` paths, because em builds under `.../portage/host/<cat>/<pf>` (intentional per-root
  builddirs, `work_root_key`); empty `/var/lib/portage/{config,preserved_libs_registry}` portage
  creates up front.
- done `eec77774`: an existing dir or dir symlink (merged-usr `/usr/sbin -> bin`) was re-chowned;
  new dirs did not get the image's mode.
- done `c0506725`: info dir index regenerated after unmerges too.
- SIZE: portage measures `${D}` before compression, em after; left as is.

## Follow-ups left by the fix batches

Each needs a cascading signature change (confirm the shape first) or is a known gap:

- `::repo` in package.accept_keywords/license/properties/accept_restrict is ignored: needs the
  candidate's repo through `KeywordAccept`/`AcceptOverlay` (~10 production call sites).
- Binpkg USE reuse ignores IUSE changes in the current ebuild: same kind of signature change.
- Config memory (`/var/lib/portage/config`): not locked against a concurrent emerge; stale
  entries are not dropped on unmerge (portage does, `vartree.py`).
- INSTALL_MASK for nodoc/noman/noinfo is `/usr/share/...`; portage uses `${EPREFIX}/usr/share/...`.
- repos.conf `[DEFAULT]` `sync-*` keys are not inherited; portage's shipped repos.conf sets
  `sync-rsync-verify-metamanifest` there for `[gentoo]`.
- binrepos.conf `verify-signature = false` cannot be told from unset.
- `em -C <ambiguous name>`: em refuses when several regular packages match; portage picks one.
