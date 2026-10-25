#!/usr/bin/env bash
# sb.sh UPPER CMD...  run CMD as root in an overlay of the parity stage3.
#
# Rootless: `unshare --map-auto` maps the subuid range so portage's
# chown(portage:portage) works, bwrap mounts the overlay. UPPER is a directory
# that keeps the run's changes, or `-` for a throwaway tmpfs.
#
#   PARITY_DIR  base/, prep/, gentoo, distfiles/ (default ~/.cache/em-parity)
#   EM          em binary bound at /usr/local/bin/em (default target/quick/em)
#   LOWER       extra lower layers, colon-separated, lowest first
set -eu
REPO_ROOT=$(cd "$(dirname "$0")/../.." && pwd)
P=${PARITY_DIR:-$HOME/.cache/em-parity}
EM=${EM:-$REPO_ROOT/target/quick/em}
up=$1
shift

low=(--overlay-src "$P/base")
[[ -d $P/prep/u && ${PREP:-} != 1 ]] && low+=(--overlay-src "$P/prep/u")
IFS=: read -ra extra <<<"${LOWER:-}"
for l in "${extra[@]}"; do low+=(--overlay-src "$l"); done
if [[ $up == - ]]; then
	ov=("${low[@]}" --tmp-overlay /)
else
	mkdir -p "$up/u" "$up/w"
	ov=("${low[@]}" --overlay "$up/u" "$up/w" /)
fi

exec unshare --user --map-root-user --map-auto --fork -- \
	bwrap --unshare-pid --unshare-ipc --die-with-parent --clearenv \
	"${ov[@]}" --proc /proc --dev /dev --tmpfs /tmp --tmpfs /dev/shm \
	--ro-bind "$(readlink -f "$P/gentoo")" /var/db/repos/gentoo \
	--bind "$P/distfiles" /var/cache/distfiles \
	--ro-bind "$EM" /usr/local/bin/em \
	--ro-bind /etc/resolv.conf /etc/resolv.conf \
	--setenv HOME /root --setenv TERM dumb --setenv LANG C.UTF-8 \
	--setenv PATH /usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
	/bin/bash -c "$*"
