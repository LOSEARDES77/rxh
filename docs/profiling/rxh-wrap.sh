#!/bin/sh
# Stand-in for the rxh binary: tweaks the generated configuration, then execs rxh.
# RXH_REAL: rxh binary. RXH_EXTRA: TOML appended to the configuration.
# RXH_CPUS: CPU list for taskset.
for last in "$@"; do :; done
[ -n "$RXH_EXTRA" ] && printf '\n%b\n' "$RXH_EXTRA" >> "$last"
if [ -n "$RXH_CPUS" ]; then exec taskset -c "$RXH_CPUS" "$RXH_REAL" "$@"; fi
exec "$RXH_REAL" "$@"
