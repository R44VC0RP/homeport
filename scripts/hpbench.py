#!/usr/bin/env python3
"""Times first visits of Homeport folders and first opens of small files, the
way an app sees them, then saves a unified log excerpt beside the results.

    python3 hpbench.py ls 10        # list 10 unvisited (dataless) repo folders
    python3 hpbench.py open 10      # open 10 small dataless files in visited folders
    python3 hpbench.py both 10
    python3 hpbench.py walk DIR     # list DIR, then each child folder (Finder-like)
    python3 hpbench.py clicks 30    # random "clicks": dev -> repo -> subfolder ... (any state),
                                    # then opens a random small file in each folder reached
    python3 hpbench.py siblings 8   # in 8 deeper folders: open one dataless small file, wait 1 s,
                                    # then open 3 more beside it
    python3 hpbench.py first 20     # one first open of a small dataless file in each of 20 folders
    python3 hpbench.py deepls 20    # first visits of dataless folders below the crawl's depth
    python3 hpbench.py finder 8     # show 8 deeper folders in Finder (`open`), wait 3 s, then
                                    # open small files in them (needs Finder automation allowed)

Prints one line per access and p50/p90/max. Writes the log to
$TMPDIR/hpbench-<time>.log.
"""
import ctypes, os, random, statistics, subprocess, sys, time

libc = ctypes.CDLL(None)
# Materialize dataless files like a normal app (some agent shells turn it off).
# IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES=3, IOPOL_SCOPE_PROCESS=0, ON=2
assert libc.setiopolicy_np(3, 0, 2) == 0

ROOT = os.path.expanduser("~/Library/CloudStorage/Homeport-basebook")
DATALESS = 0x40000000  # SF_DATALESS
SKIP = {"node_modules", ".git", "target", "build", "dist", ".next", "DerivedData", ".venv", "__pycache__", "Pods"}


def dataless(p):
    try:
        return bool(os.lstat(p).st_flags & DATALESS)
    except OSError:
        return False


def timed(fn, p):
    t = time.perf_counter()
    try:
        n = fn(p)
    except OSError as e:
        n = f"ERR {e.errno}"
    ms = (time.perf_counter() - t) * 1000
    print(f"{time.strftime('%H:%M:%S')}.{int(time.time() * 1000) % 1000:03d} {ms:8.1f} ms  {n!s:>8}  {os.path.relpath(p, ROOT)}", flush=True)
    return ms


def ls(p):
    return len(os.listdir(p))


def read(p):
    with open(p, "rb") as f:
        return len(f.read())


def summary(name, ts):
    if not ts:
        return
    s = sorted(ts)
    print(f"{name}: n={len(s)} p50={statistics.median(s):.1f} p90={s[max(0, int(len(s) * 0.9) - 1)]:.1f} max={s[-1]:.1f} ms")


def run_ls(n):
    dev = os.path.join(ROOT, "dev")
    cands = [os.path.join(dev, d) for d in os.listdir(dev) if dataless(os.path.join(dev, d)) and os.path.isdir(os.path.join(dev, d))]
    random.shuffle(cands)
    ts = []
    for p in cands[:n]:
        ts.append(timed(ls, p))
        time.sleep(0.3)
    summary("first list", ts)


def run_open(n):
    dev = os.path.join(ROOT, "dev")
    files = []
    for d in os.listdir(dev):
        p = os.path.join(dev, d)
        if os.path.isdir(p) and not dataless(p):
            for f in os.listdir(p):
                fp = os.path.join(p, f)
                st = os.lstat(fp)
                if st.st_flags & DATALESS and os.path.isfile(fp) and st.st_size < 256 * 1024:
                    files.append(fp)
    random.shuffle(files)
    ts = []
    for p in files[:n]:
        ts.append(timed(read, p))
        time.sleep(0.3)
    summary("first open", ts)


def run_walk(d):
    d = os.path.join(ROOT, d)
    ts = [timed(ls, d)]
    for c in sorted(os.listdir(d)):
        p = os.path.join(d, c)
        if c not in SKIP and os.path.isdir(p) and not os.path.islink(p):
            ts.append(timed(ls, p))
            time.sleep(0.2)
    summary("walk", ts)


def run_clicks(n):
    """Random walks like a person clicking in Finder: a repo, then subfolders
    down to depth 4, listing each (whatever its state) and opening one small
    file in each folder reached."""
    dev = os.path.join(ROOT, "dev")
    repos = [d for d in os.listdir(dev) if not d.startswith(".") and os.path.isdir(os.path.join(dev, d))]
    lists, opens = [], []
    while len(lists) < n:
        p = os.path.join(dev, random.choice(repos))
        for _ in range(3):
            lists.append(timed(ls, p))
            names = [c for c in os.listdir(p) if not c.startswith(".")]
            files = [os.path.join(p, c) for c in names
                     if os.path.isfile(os.path.join(p, c)) and 0 < os.lstat(os.path.join(p, c)).st_size < 256 * 1024]
            if files:
                opens.append(timed(read, random.choice(files)))
            subs = [os.path.join(p, c) for c in names
                    if c not in SKIP and os.path.isdir(os.path.join(p, c)) and not os.path.islink(os.path.join(p, c))]
            if not subs:
                break
            p = random.choice(subs)
            time.sleep(0.3)
    summary("click list", lists)
    summary("click open", opens)


def deep_folders(n, min_files=4):
    """Materialized folders below a repo's top level with >= min_files small dataless files."""
    dev = os.path.join(ROOT, "dev")
    repos = [d for d in os.listdir(dev) if not d.startswith(".")]
    random.shuffle(repos)
    out = []
    for r in repos:
        rp = os.path.join(dev, r)
        if not os.path.isdir(rp) or dataless(rp):
            continue
        for c in os.listdir(rp):
            p = os.path.join(rp, c)
            if c.startswith(".") or c in SKIP or os.path.islink(p) or not os.path.isdir(p) or dataless(p):
                continue
            fs = [os.path.join(p, f) for f in os.listdir(p) if not f.startswith(".")]
            fs = [f for f in fs if os.path.isfile(f) and dataless(f) and 0 < os.lstat(f).st_size < 256 * 1024]
            if len(fs) >= min_files:
                out.append((p, fs))
                break
        if len(out) >= n:
            break
    return out


def run_siblings(n):
    first, rest = [], []
    for _, fs in deep_folders(n):
        random.shuffle(fs)
        first.append(timed(read, fs[0]))
        time.sleep(1)
        rest += [timed(read, f) for f in fs[1:4]]
    summary("first open in folder", first)
    summary("next opens beside it", rest)


def run_first(n):
    ts = []
    for _, fs in deep_folders(n, 1):
        ts.append(timed(read, random.choice(fs)))
        time.sleep(2)
    summary("first open in folder", ts)


def run_deepls(n):
    """First visits of dataless folders below the crawl's depth (or anywhere
    the crawl skipped), found by walking materialized folders."""
    dev = os.path.join(ROOT, "dev")
    repos = [os.path.join(dev, d) for d in os.listdir(dev) if not d.startswith(".")]
    random.shuffle(repos)
    found = []
    for repo in repos:
        if len(found) >= n:
            break
        # At most two per repo, so the sample spans repos.
        stack, mine, seen = [repo], [], 0
        while stack and len(mine) < 2 and seen < 400:
            p = stack.pop()
            seen += 1
            if os.path.islink(p) or not os.path.isdir(p):
                continue
            if dataless(p):
                if p != repo:
                    mine.append(p)
                continue
            kids = [os.path.join(p, c) for c in os.listdir(p) if not c.startswith(".") and c not in SKIP]
            random.shuffle(kids)
            stack.extend(kids)
        found += mine
    random.shuffle(found)
    ts = []
    for p in found[:n]:
        ts.append(timed(ls, p))
        time.sleep(2)
    summary("first list (deep)", ts)


def run_finder(n):
    ts = []
    for p, fs in deep_folders(n):
        subprocess.run(["open", p])
        time.sleep(3)
        random.shuffle(fs)
        ts += [timed(read, f) for f in fs[:3]]
    summary("open after Finder showed the folder", ts)


if __name__ == "__main__":
    mode = sys.argv[1] if len(sys.argv) > 1 else "both"
    start = time.time()
    if mode == "walk":
        run_walk(sys.argv[2])
    elif mode == "siblings":
        run_siblings(int(sys.argv[2]) if len(sys.argv) > 2 else 8)
    elif mode == "first":
        run_first(int(sys.argv[2]) if len(sys.argv) > 2 else 20)
    elif mode == "deepls":
        run_deepls(int(sys.argv[2]) if len(sys.argv) > 2 else 20)
    elif mode == "finder":
        run_finder(int(sys.argv[2]) if len(sys.argv) > 2 else 8)
    elif mode == "clicks":
        run_clicks(int(sys.argv[2]) if len(sys.argv) > 2 else 30)
    else:
        n = int(sys.argv[2]) if len(sys.argv) > 2 else 10
        if mode in ("ls", "both"):
            run_ls(n)
        if mode in ("open", "both"):
            run_open(n)
    secs = int(time.time() - start) + 5
    out = os.path.join(os.environ.get("TMPDIR", "/tmp"), time.strftime("hpbench-%H%M%S.log"))
    with open(out, "w") as f:
        subprocess.run(["/usr/bin/log", "show", "--last", f"{secs}s", "--style", "compact", "--info",
                        "--predicate", 'process == "fileproviderd" OR process == "HomeportFP" OR subsystem == "dev.vogel.homeport"'],
                       stdout=f, stderr=subprocess.DEVNULL)
    print("log:", out)
