#!/usr/bin/env python3
"""Splits the samples of `perf script --no-inline` (stdin) into cost buckets.

Usage: perf script --no-inline -i FILE | buckets.py [MICROSECONDS_PER_REQUEST]
"""
import collections
import re
import sys

KERNEL = [
    ("kernel: TCP send (sendto)", ("__x64_sys_sendto",)),
    ("kernel: TCP receive (recvfrom)", ("__x64_sys_recvfrom",)),
    ("kernel: futex (park, unpark, locks)", ("__se_sys_futex", "__x64_sys_futex")),
    ("kernel: epoll_wait", ("do_epoll_wait", "__x64_sys_epoll_wait")),
    ("kernel: write (access log)", ("__x64_sys_write", "ksys_write")),
]
USER = [
    ("user: formatting (core::fmt)", r"<?(core|alloc)::fmt|<[^>]* as core::fmt::|<alloc::string::String as core::fmt::Write>"),
    ("user: tokio runtime", r"<?tokio::|<[^ ]*tokio::|<?parking_lot|<?mio::"),
    ("user: hyper, http, httparse, bytes", r"<?(hyper|hyper_util|http|httparse|bytes|http_body|http_body_util|h2)::|<<?(hyper|http)"),
    ("user: rxh", r"<?<?(rxh_server|rxh_core|rxh)::"),
]

buckets = collections.Counter()
total = 0
weight = 0
stack = []


def classify():
    names = [frame[0] for frame in stack]
    leaf, dso = stack[0]
    if "kernel" in dso or dso.startswith("["):
        if "vdso" in dso:
            return "user: clock_gettime"
        for label, symbols in KERNEL:
            if any(name in symbols for name in names):
                return label
        return "kernel: other (syscall entry, scheduler, interrupts)"
    if "libc" in dso:
        if "clock_gettime" in leaf:
            return "user: clock_gettime"
        if "lll_lock" in leaf or leaf == "syscall":
            return "user: libc lock and syscall stubs"
        if re.search(r"mem(cpy|move|set|cmp|chr)|str", leaf):
            return "user: memcpy and friends"
        return "user: allocator (malloc, realloc, free)"
    for label, pattern in USER:
        if re.match(pattern, leaf):
            return label
    return "user: std and other"


def flush():
    global total
    if stack:
        total += weight
        buckets[classify()] += weight


for line in sys.stdin:
    if line.startswith(("\t", " ")) and line.strip():
        rest = line.split(None, 1)[1].rstrip()
        symbol, _, dso = rest.rpartition(" (")
        stack.append((re.sub(r"\+0x[0-9a-f]+$", "", symbol), dso.rstrip(")")))
    elif line.strip():
        flush()
        stack = []
        header = re.search(r":\s+(\d+) \S+:", line)
        weight = int(header.group(1)) if header else 1
flush()

per_request = float(sys.argv[1]) if len(sys.argv) > 1 else None
for label, value in sorted(buckets.items(), key=lambda item: (item[0].split(":")[0], -item[1])):
    share = value / total
    extra = f"  {share * per_request:6.2f} us" if per_request else ""
    print(f"{100 * share:6.2f}%{extra}  {label}")
