# FEATURES parity

Status: 🟡 steps 1–3 done 2026-10-27 (see "Done"); ccache and the sandbox family are not implemented. Filed
2026-10-22 from [[portage-config-layering]] (decision D3).
Related: [[build-clean-env]] (the `keepwork`/`keeptemp`/`noclean` subset, done).

## Decision (Luca, 2026-10-22)

The ebuild environment's `FEATURES` lists what `em` provides, not make.globals' set: an
eclass testing `has network-sandbox ${FEATURES}` must not be told about a sandbox em does not
run. Matching Portage's set is this note's work, one feature at a time.

## Where em stands (grep of `features.contains(…)`, 2026-10-22)

- **Read from FEATURES:** `binpkg-request-signature`, `binpkg-signing`, `buildpkg-live`,
  `candy`, `ccache`, `compress-index`, `config-protect-if-modified` (seeded from the settings),
  `keeptemp`, `keepwork`, `noclean`, `nostrip`, `test`, `test-fail-continue`; `sandbox` only in a test.

- **Carried from the settings into the build shell** (`settings::PROVIDED_FEATURES`, 2026-10-26):
  `binpkg-multi-instance`, `buildpkg-live`, `config-protect-if-modified`, `preserve-libs`,
  `unmerge-orphans`.
  The VDB `FEATURES` therefore differs from emerge's, which records the whole make.globals set.
- **Work 3 so far:** `-preserve-libs` and `-binpkg-multi-instance` switch those off; without a
  make.globals em supplies both as its default. `distlocks` stays always on: the same lock keeps
  em's own parallel fetch tasks off one distfile.

## The make.globals defaults, checked against the code (2026-10-26)

Always on in em, no FEATURES switch:

| Feature | Where |
|---|---|
| `binpkg-docompress`, `binpkg-dostrip` | ecompress/estrip run at the tail of `src_install` (`ebuild/mod.rs` `post_process_image`), before `ebuild/binpkg.rs` packs the image |
| `binpkg-multi-instance` | `portage-binpkg` writes `cat/pn/pf-build_id.gpkg.tar` |
| `distlocks` | `portage-distfiles/src/fetch.rs` `lock_distfile` (`.<name>.portage_lockfile`) |
| `pkgdir-index-trusted` | `BinpkgIndex::open` trusts `Packages`; it scans PKGDIR only when the index is absent or empty |
| `preserve-libs` | `preserve_libs.rs`, wired into every merge |
| `protect-owned` | `ebuild/mod.rs` `check_collisions` refuses a path another installed package owns |
| `buildpkg-live`, `config-protect-if-modified`, `unmerge-orphans` | read from the settings |

Missing in em:

| Feature | State |
|---|---|
| `sandbox`, `usersandbox`, `network-sandbox`, `ipc-sandbox`, `pid-sandbox` | no phase sandbox or namespaces |
| `userpriv`, `userfetch`, `usersync` | phases, fetch and sync run as the invoking user; no drop to `portage` |
| `ebuild-locks` | no lock around unsandboxed phases |
| `merge-wait` | `--jobs` merges each package as soon as it builds (`merge/mod.rs`) |
| `merge-sync` | no fsync after merge or unmerge |
| `parallel-fetch` | no background fetch of later packages' distfiles |
| `news` | no news update or notice after sync; only `em select news` |
| `fixlafiles`, `multilib-strict`, `qa-unresolved-soname-deps` | none of these install QA passes |
| `strict` | no switch; a distfile without a digest is refused unless the repo layout or `digest` allows it |
| `unknown-features-warn` | unknown FEATURES are not reported |

No meaning for em:

| Feature | Why |
|---|---|
| `assume-digests` | em does not generate Manifests |
| `binpkg-logs`, `unmerge-logs` | em keeps only elog output in `PORTAGE_LOGDIR`; build logs stay with the work dir |

## Work

1. Done: the table above.
2. Done: `settings::PROVIDED_FEATURES` carries the always-on set and the two read features into the
   build shell when the settings enable them. The VDB `FEATURES` still omits the missing ones,
   which emerge records.
3. For each always-on one, decide whether `-feature` in make.conf must switch it off, as in
   Portage.
4. `em --info`/`portageq envvar FEATURES` show Portage's stacked value (make.globals included)
   once [[portage-config-layering]] step 2 exists; the build environment shows em's. Say so in
   `--info` if the two differ.

## Done (2026-10-27)

What the tree tests (`has X ${FEATURES}` in ebuilds and eclasses): `userpriv` 43, `usersandbox` and
`network-sandbox` 27, `ccache` 26, `preserve-libs` 20, `test` 19, `distcc` 13, `installsources` 8, `noman` 4,
`unmerge-orphans` 2, `sfperms`, `sandbox`, `nodoc` 1.

A build's `FEATURES` is now `settings::provided_features`:
- Honoured switches the configuration sets (`package.env` included): `test`, `keepwork`, `keeptemp`, `noclean`,
  `nostrip`, `binpkg-signing`, `binpkg-request-signature`. (`candy`, emerge's resolve spinner, is not implemented:
  the 2026-10-22 list above counted a profile test's sample text.)
- `preserve-libs` as Portage stacks it, so `-preserve-libs` now switches preservation off (Luca: "interesting to
  implement just to see how it is"). Without make.globals em's own globals layer is `FEATURES="preserve-libs"`.
- `binpkg-multi-instance`, always (every package gets a build id).
- `userpriv` when the build does not run as root; not under real or mapped root, as in crossdev-stages' box
  (Luca: em should not claim it there).
- Everything else is dropped: the sandbox family (em has none), `ccache` (no PATH/CCACHE_DIR setup), `distcc`,
  `installsources`, `splitdebug`, `noman`/`nodoc`/`noinfo`, `sfperms`, `test-fail-continue`.

Checked in a stage3 sandbox: a build saw `binpkg-multi-instance preserve-libs` (it had seen the config's
`parallel-install`); removing a needed library keeps it by default and deletes it with `FEATURES=-preserve-libs`.

Open: ccache support; whether `--info` should say that the build value differs from Portage's stacked one (step 4).

**`candy` (Luca, 2026-10-27):** Portage's is a scrolling-message spinner while the resolver runs; em's resolve is too
fast to need one. Instead, `FEATURES=candy` selects a richer build progress display: a sink on the activity bus
(next to `HumanStdoutSink`, see [[activity-status]]) drawing per-package/phase bars and the ETA from the duration
history, with indicatif (already a dependency, used for sync) or ratatui. Not started.
- indicatif's `MultiProgress` can keep each running build's last output line under its bar, so parallel
  (`--jobs N`) builds stay readable; the mode works for both verbose and quiet runs.
- It may become the default display once it exists.

