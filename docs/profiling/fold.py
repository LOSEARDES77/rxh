#!/usr/bin/env python3
"""Summarises `perf script --no-inline` output read from stdin.

Usage: perf script --no-inline -i FILE | fold.py [--per EVENT] [--sites] [--folded OUT] [FRAME...]

Prints the inclusive and self share of every frame. For each FRAME given
(a substring), prints the callers that reach it, skipping std and allocator
frames. With `--sites`, every sample is charged to the first frame that is not
part of std or the allocator: for `probe_libc:malloc` samples that is the
allocation site. With `--per EVENT`, samples of EVENT (a substring) are only
counted, and the tables show samples per EVENT instead of percentages. With
`--folded OUT`, the other samples are also written as folded stacks, the input
of `inferno-flamegraph` and `flamegraph.pl`.
"""
import collections
import re
import sys

PLUMBING = re.compile(
    r"(<?_?_?(rust|rustc|alloc|core|std|hashbrown|libc)\b|\[unknown\]|_int_|malloc|realloc|cfree|calloc"
    r"|posix_memalign|<?&?(mut )?(alloc|core|std|hashbrown)::|<\[|<\(|<bytes::|bytes::|<u8 |<str>|<&)"
)

args = sys.argv[1:]
per = None
if "--per" in args:
    index = args.index("--per")
    per = args[index + 1]
    del args[index : index + 2]
folded_path = None
if "--folded" in args:
    index = args.index("--folded")
    folded_path = args[index + 1]
    del args[index : index + 2]
sites_wanted = "--sites" in args
watch = [arg for arg in args if arg != "--sites"]

inclusive = collections.Counter()
self_weight = collections.Counter()
sites = collections.Counter()
folded = collections.Counter()
site_bytes = collections.Counter()
callers = collections.defaultdict(collections.Counter)
events = collections.Counter()
total = 0
divisor = 0
weight = 0
size = 0
event = ""
stack = []


def first_caller(frames):
    for frame in frames:
        if not PLUMBING.match(frame):
            return frame
    return "(none)"


def flush():
    global total, divisor
    if not stack:
        return
    events[event] += 1
    if per and per in event:
        divisor += 1
        return
    total += weight
    self_weight[stack[0]] += weight
    if folded_path:
        folded[";".join(reversed(stack))] += weight
    for frame in set(stack):
        inclusive[frame] += weight
    if sites_wanted:
        site = first_caller(stack)
        sites[site] += weight
        site_bytes[site] += size
    for name in watch:
        for index, frame in enumerate(stack):
            if name in frame:
                callers[name][first_caller(stack[index + 1 :])] += weight
                break


for line in sys.stdin:
    if line.startswith(("\t", " ")) and line.strip():
        frame = line.split(None, 1)[1].rsplit(" (", 1)[0]
        stack.append(re.sub(r"\+0x[0-9a-f]+$", "", frame))
    elif line.strip():
        flush()
        stack = []
        header = re.search(r":\s+(?:(\d+) )?(\S+):", line)
        event = header.group(2) if header else "?"
        # Probe samples carry no period: one sample is one call.
        weight = int(header.group(1)) if header and header.group(1) else 1
        found = re.search(r"size=(\d+)", line)
        size = int(found.group(1)) if found else 0
flush()

if folded_path:
    with open(folded_path, "w") as out:
        for frames, value in folded.items():
            out.write(f"{frames} {value}\n")

if per:
    unit, scale = f"per {per}", 1 / max(divisor, 1)
    show = lambda value: f"{value * scale:8.2f}"
else:
    unit, scale = "%", 100 / max(total, 1)
    show = lambda value: f"{value * scale:6.2f}%"

print("events:", dict(events), "unit:", unit)
if sites_wanted:
    print("== sites (calls, bytes)")
    for frame, value in sites.most_common(70):
        print(f"{show(value)} {site_bytes[frame] * (scale if per else 1):10.0f}  {frame[:170]}")
print("== inclusive")
for frame, value in inclusive.most_common(300):
    print(f"{show(value)}  {frame[:170]}")
print("== self")
for frame, value in self_weight.most_common(60):
    print(f"{show(value)}  {frame[:170]}")
for name in watch:
    print(f"== callers of {name}: {show(sum(callers[name].values()))}")
    for frame, value in callers[name].most_common(30):
        print(f"{show(value)}  {frame[:170]}")
