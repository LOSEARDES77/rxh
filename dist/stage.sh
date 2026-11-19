#!/bin/sh
# Lays out a release archive in DIR from a built rxh binary: the binary, man
# pages, shell completions, systemd units pointing at /usr/bin/rxh, the
# default package configuration and the license. The archives, the .deb and
# .rpm (dist/nfpm.yaml), the AUR package and the Homebrew formula all install
# from this layout.
#
# Usage: stage.sh BINARY DIR
#
#   DIR/rxh
#   DIR/rxh.toml                  default /etc/rxh/rxh.toml of the packages
#   DIR/LICENSE  DIR/README.md
#   DIR/systemd/rxh.service  DIR/systemd/rxh.socket
#   DIR/man/*.1
#   DIR/completions/rxh.bash  DIR/completions/_rxh  DIR/completions/rxh.fish
set -eu

if [ "$#" -ne 2 ]; then
    echo "usage: $0 BINARY DIR" >&2
    exit 2
fi
binary=$1
dir=$2
dist=$(cd "$(dirname "$0")" && pwd)
root=$(dirname "$dist")

if [ ! -x "$binary" ]; then
    echo "not an executable: $binary" >&2
    exit 2
fi
if [ -e "$dir" ]; then
    echo "already exists: $dir" >&2
    exit 2
fi

mkdir -p "$dir/systemd" "$dir/completions"
cp "$binary" "$dir/rxh"
chmod 755 "$dir/rxh"
cp "$dist/rxh.toml" "$root/LICENSE" "$root/README.md" "$dir/"
for unit in rxh.service rxh.socket; do
    sed 's|/usr/local/bin/rxh|/usr/bin/rxh|g' "$dist/$unit" > "$dir/systemd/$unit"
done
"$dir/rxh" man --dir "$dir/man"
"$dir/rxh" completions bash > "$dir/completions/rxh.bash"
"$dir/rxh" completions zsh > "$dir/completions/_rxh"
"$dir/rxh" completions fish > "$dir/completions/rxh.fish"
