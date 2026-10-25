#!/usr/bin/env bash
# resolve-sweep.sh OUT [OPTS...] < atoms  run `emerge -pv OPTS ATOM` and
# `em -pv OPTS ATOM` per atom in a throwaway overlay, then classify.
#
# OUT/<slug>.{emerge,em} keep each tool's full output with a trailing rc=N.
#   JOBS  parallel sandboxes (default nproc-8)
set -eu
here=$(cd "$(dirname "$0")" && pwd)
out=$1
shift
mkdir -p "$out"
export here out opts="$*"
one() {
	slug=$(printf '%s %s' "$1" "$opts" | tr -c 'A-Za-z0-9._+-' _)
	"$here/sb.sh" - "
		emerge -pv --nospinner --color=n $opts $1 >/tmp/a 2>&1; echo rc=\$? >>/tmp/a
		em --color=never -pv $opts $1 >/tmp/b 2>&1; echo rc=\$? >>/tmp/b
		cat /tmp/a; echo @@SPLIT@@; cat /tmp/b" >"$out/$slug.raw" 2>&1
	sed '/^@@SPLIT@@$/,$d' "$out/$slug.raw" >"$out/$slug.emerge"
	sed '1,/^@@SPLIT@@$/d' "$out/$slug.raw" >"$out/$slug.em"
	rm -f "$out/$slug.raw"
}
export -f one
xargs -P "${JOBS:-$(($(nproc) - 8))}" -I{} bash -c 'one "$@"' _ {}
python3 -I "$here/classify.py" "$out"
