#!/usr/bin/env bash
# mkbase.sh STAGE3_TARBALL TREE  build PARITY_DIR from a stage3 and a ::gentoo tree.
#
# The stage3 is unpacked inside the same user namespace sb.sh uses, so file
# owners land on the mapped subuids. The tree is copied, so syncing TREE does
# not change results mid-run. prep/ holds the one-off global updates pass.
set -eu
P=${PARITY_DIR:-$HOME/.cache/em-parity}
stage=$(readlink -f "$1")
tree=$(readlink -f "$2")
[[ -e $P/base ]] && { echo "$P/base exists" >&2; exit 1; }
mkdir -p "$P/base" "$P/distfiles"
unshare --user --map-root-user --map-auto -- cp -a "$tree" "$P/tree"
ln -sfn tree "$P/gentoo"
(cd "$P/base" && unshare --user --map-root-user --map-auto --fork -- \
	tar -xpf "$stage" --xattrs-include='*.*' --numeric-owner --exclude='./dev/*')
PREP=1 "$(dirname "$0")/sb.sh" "$P/prep" 'emerge -p sys-libs/zlib >/dev/null'
