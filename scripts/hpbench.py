#!/usr/bin/env python3
"""Times first visits of Homeport folders and first opens of small files, the
way an app sees them, then saves a unified log excerpt beside the results.

    python3 hpbench.py ls 10        # list 10 unvisited (dataless) repo folders
    python3 hpbench.py open 10      # open 10 small dataless files in visited folders
    python3 hpbench.py both 10
    python3 hpbench.py walk DIR     # list DIR, then each child folder (Finder-like)

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


if __name__ == "__main__":
    mode = sys.argv[1] if len(sys.argv) > 1 else "both"
    start = time.time()
    if mode == "walk":
        run_walk(sys.argv[2])
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
