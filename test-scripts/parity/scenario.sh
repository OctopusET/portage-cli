#!/usr/bin/env bash
# scenario.sh NAME ATOM < steps  run the same steps with emerge and with em on
# ATOM's dependency layer, then diff the two final states.
#
# Each step line is either `T <args>` (run as `emerge <args>` on one side and
# `em <args>` on the other) or `! <shell>` (run unchanged on both); `#` lines
# are comments. A failing `T` step is logged and the run goes on, so a refusal
# shows up in the diff and logs instead of hiding the later steps.
# Layout: PARITY_DIR/scenario/NAME/{emerge,em}/ uppers and *.log.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
P=${PARITY_DIR:-$HOME/.cache/em-parity}
name=$1 atom=$2
R=$P/scenario/$name
D=$P/merge/$(printf '%s' "$atom" | tr -c 'A-Za-z0-9._+-' _)
steps=$(cat)

if [[ ! -e $D/deps.ok ]]; then
	mkdir -p "$D"
	"$here/sb.sh" "$D/deps" "emerge -f --nodeps --nospinner --color=n $atom &&
		emerge -1 --onlydeps --with-bdeps=y --nospinner --color=n $atom" \
		>"$D/deps.log" 2>&1 || { echo "deps failed: $D/deps.log" >&2; exit 1; }
	touch "$D/deps.ok"
fi

script() {
	local tool=$1 line
	while IFS= read -r line; do
		case $line in
		'T '*)
			printf 'echo ">>> step: %s"\n' "${line//\"/\\\"}"
			if [[ $tool == emerge ]]; then
				printf 'emerge --nospinner --color=n %s || echo ">>> rc=$?"\n' "${line#T }"
			else
				printf 'em --color=never %s || echo ">>> rc=$?"\n' "${line#T }"
			fi
			;;
		'! '*) printf '%s\n' "${line#! }" ;;
		esac
	done <<<"$steps"
}

for side in emerge em; do
	if [[ -d $R/$side ]]; then
		unshare --user --map-root-user --map-auto -- find "$R/$side" -delete
	fi
	mkdir -p "$R"
	LOWER=$D/deps/u "$here/sb.sh" "$R/$side" "$(script $side)" >"$R/$side.log" 2>&1 &
done
wait
unshare --user --map-root-user --map-auto -- \
	python3 -I "$here/treediff.py" "$R/emerge/u" "$R/em/u" | tee "$R/diff.txt"
