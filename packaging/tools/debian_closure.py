#!/usr/bin/python3
"""Resolve the Debian librust-*-dev closure for a workspace's direct dependencies.

Usage: closure.py meta.json Packages [Packages ...] [--swap crate=newcrate:req]

Starts from each workspace member's normal (and build) dependencies, expressed
the way dh-cargo/debcargo express them (librust-<crate>-dev plus one
librust-<crate>+<feature>-dev per enabled feature, with >= / << bounds from
the cargo requirement), then walks Depends: in the binary Packages index until
everything is satisfied or a relation can't be.
"""
import json, re, sys
from collections import defaultdict
import apt_pkg

apt_pkg.init_system()


def load(paths):
    real = defaultdict(list)    # name -> [(version, depends)]
    virt = defaultdict(list)    # provided name -> [(version or None, provider)]
    for p in paths:
        for st in open(p, encoding="utf-8", errors="replace").read().split("\n\n"):
            m = re.search(r"^Package: (\S+)", st, re.M)
            if not m:
                continue
            name = m.group(1)
            ver = re.search(r"^Version: (\S+)", st, re.M).group(1)
            dep = re.search(r"^Depends: (.*(?:\n .*)*)", st, re.M)
            prov = re.search(r"^Provides: (.*(?:\n .*)*)", st, re.M)
            deps = dep.group(1).replace("\n", " ") if dep else ""
            real[name].append((ver, deps))
            if prov:
                for item in prov.group(1).replace("\n", " ").split(","):
                    item = item.strip()
                    mm = re.match(r"(\S+)(?:\s*\(=\s*(\S+)\))?", item)
                    if mm:
                        virt[mm.group(1)].append((mm.group(2), name, ver))
    return real, virt


def candidates(name, real, virt):
    """[(version, providing real package, its version)]"""
    out = [(v, name, v) for v, _ in real.get(name, [])]
    out += [(pv, prov, rv) for pv, prov, rv in virt.get(name, [])]
    return out


def ok(ver, op, want):
    if ver is None:
        return False
    c = apt_pkg.version_compare(ver, want)
    return {">=": c >= 0, "<<": c < 0, "<=": c <= 0, ">>": c > 0, "=": c == 0}[op]


def parse_rel(s):
    m = re.match(r"\s*(\S+)(?:\s*\((>=|<=|<<|>>|=)\s*([^)]+)\))?", s)
    if not m:
        return None, None, ""
    return m.group(1), m.group(2), (m.group(3) or "").strip()


def cargo_to_deb(req):
    """Translate a cargo requirement to [(op, version)] the way debcargo does."""
    out = []
    for clause in req.split(","):
        clause = clause.strip()
        m = re.match(r"^(\^|~|>=|<=|>|<|=)?\s*([\d.]+)", clause)
        if not m:
            continue
        op, num = m.group(1) or "^", m.group(2)
        parts = [int(x) for x in num.split(".")]
        n = len(parts)
        p3 = (parts + [0, 0, 0])[:3]
        lo = ".".join(map(str, p3)) + "-~~"
        if op == "^":
            if p3[0] > 0 or n == 1:
                hi = f"{p3[0]+1}-~~"
            elif p3[1] > 0 or n == 2:
                hi = f"0.{p3[1]+1}-~~"
            else:
                hi = f"0.0.{p3[2]+1}-~~"
            out += [(">=", lo), ("<<", hi)]
        elif op == "~":
            hi = f"{p3[0]+1}-~~" if n == 1 else f"{p3[0]}.{p3[1]+1}-~~"
            out += [(">=", lo), ("<<", hi)]
        elif op == "=":
            out += [(">=", lo), ("<<", ".".join(map(str, p3[:2] + [p3[2] + 1])) + "-~~")]
        elif op == ">=":
            out.append((">=", lo))
        elif op == "<":
            out.append(("<<", lo))
    return out


def debname(c):
    return c.lower().replace("_", "-")


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--swap")]
    swaps = {}
    for a in sys.argv[1:]:
        if a.startswith("--swap="):
            old, new = a[len("--swap="):].split("=", 1)
            nc, nr = new.split(":", 1)
            swaps[old] = (nc, nr)
    meta = json.load(open(args[0]))
    real, virt = load(args[1:])
    pkgs = {p["id"]: p for p in meta["packages"]}
    ws = [pkgs[i] for i in meta["workspace_members"]]
    wsnames = {p["name"] for p in ws}

    roots = []  # (from, name, [(op, ver)])
    for p in ws:
        for d in p["dependencies"]:
            if d["kind"] not in (None, "build") or d["name"] in wsnames:
                continue
            name, req = d["name"], d["req"]
            if name in swaps:
                name, req = swaps[name]
            bounds = cargo_to_deb(req)
            base = f"librust-{debname(name)}-dev"
            roots.append((p["name"], base, bounds, req))
            feats = list(d["features"])
            if d.get("uses_default_features", True):
                feats.append("default")
            for f in feats:
                roots.append((p["name"], f"librust-{debname(name)}+{debname(f)}-dev", bounds, req))

    problems = []
    chosen = {}
    seen = set()
    queue = [(frm, name, bounds) for frm, name, bounds, _ in roots]
    direct = []
    for frm, name, bounds, req in roots:
        cs = [c for c in candidates(name, real, virt) if all(ok(c[0], op, v) for op, v in bounds)]
        allc = sorted({c[0] for c in candidates(name, real, virt) if c[0]}, key=__import__("functools").cmp_to_key(apt_pkg.version_compare))
        direct.append({"from": frm, "relation": name, "cargo_req": req,
                       "satisfied": bool(cs), "archive": allc[-1] if allc else None})
    while queue:
        frm, name, bounds = queue.pop()
        key = (name, tuple(bounds))
        if key in seen:
            continue
        seen.add(key)
        cs = [c for c in candidates(name, real, virt) if all(ok(c[0], op, v) for op, v in bounds)]
        if not cs:
            problems.append({"needed_by": frm, "relation": name,
                             "bounds": " ".join(f"{o} {v}" for o, v in bounds),
                             "archive_has": sorted({c[0] or "(unversioned)" for c in candidates(name, real, virt)})})
            continue
        cs.sort(key=__import__("functools").cmp_to_key(lambda a, b: apt_pkg.version_compare(a[2], b[2])))
        prov, pver = cs[-1][1], cs[-1][2]
        if prov in chosen:
            continue
        chosen[prov] = pver
        deps = dict(real[prov])[pver]
        # group "a (>= x), a (<< y)" into one bound list per name
        grouped = defaultdict(list)
        for item in deps.split(","):
            alts = item.split("|")
            n, op, v = parse_rel(alts[0])
            if not n or not n.startswith("librust-"):
                continue
            if op:
                grouped[n].append((op, v))
            else:
                grouped.setdefault(n, [])
        for n, b in grouped.items():
            queue.append((prov, n, b))
    json.dump({"direct": direct, "problems": problems,
               "closure_size": len(chosen),
               "closure": dict(sorted(chosen.items()))}, sys.stdout, indent=1)


if __name__ == "__main__":
    main()
