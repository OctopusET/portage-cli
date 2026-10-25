#!/usr/bin/env python3
"""classify.py OUT: compare OUT/*.emerge against OUT/*.em merge lists.

Classes: same, order (same lines, other order), display (same packages,
different line text), set (different packages), rc (one side failed), fail
(both failed: their partial plans are not compared).
Writes OUT.json; prints counts and the packages most often on one side only.
"""
import collections
import json
import os
import re
import sys

LINE = re.compile(r"^\[(ebuild|binary|nomerge|blocks|uninstall)[^\]]*\]\s+(\S+)")


def parse(path):
    text = open(path, errors="replace").read()
    rc = re.findall(r"^rc=(\d+)$", text, re.M)
    lines = [re.sub(r"\s+", " ", l.strip()) for l in text.splitlines() if LINE.match(l.strip())]
    pkgs = {LINE.match(l).group(2) for l in lines}
    return (rc[-1] if rc else "?"), lines, pkgs


def cpn(pkg):
    return re.sub(r"-[0-9][^/]*$", "", pkg.split("::")[0].split(":")[0])


def main(out):
    classes = collections.defaultdict(list)
    only = {"emerge": collections.Counter(), "em": collections.Counter()}
    for name in sorted(os.listdir(out)):
        if not name.endswith(".emerge"):
            continue
        slug = name[: -len(".emerge")]
        erc, el, ep = parse(os.path.join(out, name))
        mrc, ml, mp = parse(os.path.join(out, slug + ".em"))
        if (erc == "0") != (mrc == "0"):
            k = "rc"
        elif erc != "0":
            k = "fail"
        elif ep != mp:
            k = "set"
            for p in ep - mp:
                only["emerge"][cpn(p)] += 1
            for p in mp - ep:
                only["em"][cpn(p)] += 1
        elif el == ml:
            k = "same"
        elif sorted(el) == sorted(ml):
            k = "order"
        else:
            k = "display"
        classes[k].append(slug)
    json.dump(classes, open(out.rstrip("/") + ".json", "w"), indent=1)
    print({k: len(v) for k, v in sorted(classes.items())})
    for side, c in only.items():
        print(f"only {side}:", ", ".join(f"{p} {n}" for p, n in c.most_common(15)))


if __name__ == "__main__":
    main(sys.argv[1])
