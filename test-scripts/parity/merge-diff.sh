#!/usr/bin/env bash
# merge-diff.sh ATOM [EM_OPTS...]  merge ATOM with emerge and with em on the
# same dependency layer, then diff what each left on disk.
#
# Layout under PARITY_DIR/merge/<slug>/: deps/ (emerge --onlydeps, shared
# lower layer), emerge/ and em/ (one upper each, logs in *.log). deps/ and a
# successful emerge/ are kept for the next run; em/ is always redone.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
P=${PARITY_DIR:-$HOME/.cache/em-parity}
atom=$1
shift
R=$P/merge/$(printf '%s' "$atom" | tr -c 'A-Za-z0-9._+-' _)
mkdir -p "$R"

if [[ ! -e $R/deps.ok ]]; then
	# Fetch first: two tools downloading one distfile at once corrupt it.
	"$here/sb.sh" "$R/deps" "emerge -f --nodeps --nospinner --color=n $atom &&
		emerge -1 --onlydeps --with-bdeps=y --nospinner --color=n $atom" \
		>"$R/deps.log" 2>&1 || { echo "deps failed: $R/deps.log" >&2; exit 1; }
	touch "$R/deps.ok"
fi

if [[ -d $R/em ]]; then
	unshare --user --map-root-user --map-auto -- find "$R/em" -delete
fi
rc=0
if [[ ! -e $R/emerge.ok ]]; then
	LOWER=$R/deps/u "$here/sb.sh" "$R/emerge" "emerge -1 --nodeps --nospinner --color=n $atom" \
		>"$R/emerge.log" 2>&1 && touch "$R/emerge.ok" || { rc=1; echo "emerge failed: $R/emerge.log" >&2; }
fi
LOWER=$R/deps/u "$here/sb.sh" "$R/em" "em --color=never -1 --nodeps $* $atom" \
	>"$R/em.log" 2>&1 || { rc=$((rc | 2)); echo "em failed: $R/em.log" >&2; }
unshare --user --map-root-user --map-auto -- \
	python3 -I "$here/treediff.py" "$R/emerge/u" "$R/em/u" | tee "$R/diff.txt"
exit "$rc"
