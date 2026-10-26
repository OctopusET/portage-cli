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
- dev-java/qdox <-> dev-java/jmock: em reports a hard cycle, emerge resolves it.
- `||` choice differs: pulseaudio-daemon (emerge) vs pipewire (em) for media-libs/pulseaudio-qt,
  clang (emerge) vs gcc (em) for gnustep-apps/projectcenter, virtual/w3m pulled only by em.
- Both fail: emerge stops at the first unsatisfiable REQUIRED_USE, em completes the USE cascade
  (known for multi-target; also seen single-target).
- em prints "Installed packages kept although masked" for unrelated installed packages
  (gnupg, elfutils, libxcrypt keyworded ~amd64 after the tree moved); emerge stays silent.
- `-v` old-version column: emerge `[4.1.2-r7:4.1.2::gentoo, ...]`, em `[4.1.2-r7, ...]`
  (`convert_myoldbest`: slot/subslot and repo at verbosity 3).
- done `7b605e56`: missing second space before the size when a line has no USE column.
- `em --color=n` rejected (emerge takes y/n).

## Merge (`merge-diff.sh`, 16 packages)

- VDB `USE` not filtered to IUSE_EFFECTIVE.
- VDB `DEFINED_PHASES` includes em's default stubs.
- VDB `*DEPEND`/`LICENSE`/`RESTRICT` written raw: not USE-reduced, `:=` not bound.
- No implicit `>=sys-libs/glibc-<installed>` in RDEPEND (portage `_inject_libc_dep`).
- VDB lacks `CC`, `CXX`, `FEATURES`, `REPO_REVISIONS`.
- setgid lost: app-misc/screen `/usr/bin/screen-4.9.1` 2755 (emerge) vs 0755 (em).
- sys-apps/pciutils `/usr/lib64/libpci.so.3.15.0` 0755 (emerge) vs 0644 (em).
- econf argument order: em puts `--libdir` after `--localstatedir`, portage after
  `--with-sysroot`; it ends up in curl-config and other embedded configure lines.
- Global counter in `var/db/pkg/COUNTER`; portage uses `/var/cache/edb/counter`.
- env-update: no `/etc/csh.env`, no `/etc/environment.d/10-gentoo-env.conf`, `profile.env`
  lacks `export GCC_SPECS=''`.
- No `/usr/share/info/dir` regeneration after merging info pages.
- `/var/lib/portage/config` (config-protect-if-modified md5s) never written.
- `preserved_libs_registry` written 0600 with em's own JSON; portage writes 0660 `{}`.
- Two tools fetching one distfile into a shared DISTDIR: emerge's verified file was
  truncated under it; em writes the final name in place.
- SIZE: portage measures `${D}` before compression, em after; left as is.
