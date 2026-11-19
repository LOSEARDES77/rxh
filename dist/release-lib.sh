# Shared by dist/pkgbuild.sh and dist/homebrew.sh. Sourced, not run.

repository=https://github.com/LOSEARDES77/rxh

# Exits with 2 unless $1 is a release version (`0.3.0`, `1.2.0-rc.1`).
check_version() {
    if ! echo "$1" | grep -Eq '^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z.]+)?$'; then
        echo "not a release version: $1" >&2
        exit 2
    fi
}

# Prints the SHA-256 of archive $2 from the `sha256sum` file $1, or exits with 2.
checksum() {
    sum=$(awk -v file="$2" '$2 == file || $2 == "*" file { print $1 }' "$1")
    if ! echo "$sum" | grep -Eq '^[0-9a-f]{64}$'; then
        echo "no checksum for $2 in $1" >&2
        exit 2
    fi
    echo "$sum"
}
