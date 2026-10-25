#!/usr/bin/env python3
"""treediff.py A B: diff two overlay upper dirs (A = emerge, B = em).

Files are compared by type, mode, owner, symlink target and content hash.
VDB entries are compared key by key: CONTENTS without mtimes, environment.bz2
by variable and function names. Paths that are per-run noise are skipped.
"""
import bz2
import hashlib
import os
import re
import stat
import sys

NOISE = re.compile(
    r"^(tmp|run|dev|proc|var/tmp|var/log|var/cache|root|var/lib/portage/home"
    r"|etc/ld\.so\.cache|var/db/pkg/\.\w+)(/|$)|/__pycache__/"
)
VDB = re.compile(r"^var/db/pkg/[^/]+/[^/]+/")
VOLATILE_KEYS = {"COUNTER", "BUILD_TIME"}


def walk(root):
    out = {}
    for dirpath, dirs, files in os.walk(root):
        for name in dirs + files:
            full = os.path.join(dirpath, name)
            rel = os.path.relpath(full, root)
            if NOISE.search(rel):
                continue
            st = os.lstat(full)
            if stat.S_ISLNK(st.st_mode):
                desc = ("l", os.readlink(full))
            elif stat.S_ISDIR(st.st_mode):
                desc = ("d",)
            elif stat.S_ISCHR(st.st_mode) and st.st_rdev == 0:
                desc = ("whiteout",)
            elif stat.S_ISREG(st.st_mode):
                desc = ("f", hashlib.sha256(open(full, "rb").read()).hexdigest()[:16])
            else:
                desc = ("other", st.st_mode)
            out[rel] = desc + (oct(stat.S_IMODE(st.st_mode)), st.st_uid, st.st_gid)
    return out


def vdb_key(root, rel):
    data = open(os.path.join(root, rel), "rb").read()
    if rel.endswith("/CONTENTS"):
        lines = []
        for l in data.decode(errors="replace").splitlines():
            f = l.split(" ")
            if f[0] == "obj":
                l = " ".join(f[:-1])
            elif f[0] == "sym":
                l = " ".join(f[:-1])
            lines.append(l)
        return sorted(lines)
    if rel.endswith("/environment.bz2"):
        text = bz2.decompress(data).decode(errors="replace")
        names = set(re.findall(r"^declare -\S+ ([A-Za-z_][A-Za-z0-9_]*)", text, re.M))
        funcs = set(re.findall(r"^([A-Za-z_][\w:-]*) \(\) $", text, re.M))
        return sorted(names) + sorted(f + "()" for f in funcs)
    return data.decode(errors="replace").split()


def show(label, items, limit=40):
    items = sorted(items)
    if items:
        print(f"## {label} ({len(items)})")
        for i in items[:limit]:
            print("  " + i)
        if len(items) > limit:
            print(f"  ... {len(items) - limit} more")


def main(a_root, b_root):
    a, b = walk(a_root), walk(b_root)
    show("only emerge", [p for p in a if p not in b])
    show("only em", [p for p in b if p not in a])
    diff, vdb = [], []
    for p in sorted(set(a) & set(b)):
        if VDB.match(p) and a[p][0] == b[p][0] == "f":
            if p.rsplit("/", 1)[1] in VOLATILE_KEYS:
                continue
            x, y = vdb_key(a_root, p), vdb_key(b_root, p)
            if x != y:
                sx, sy = set(x), set(y)
                vdb.append(f"{p}: -{sorted(sx - sy)[:12]} +{sorted(sy - sx)[:12]}")
        elif a[p] != b[p]:
            diff.append(f"{p}: emerge={a[p]} em={b[p]}")
    show("differ", diff)
    show("vdb differ", vdb, limit=100)


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2])
