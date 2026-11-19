#!/bin/sh
# Prints the tags of an official image, one per line.
#
# Usage: image-tags.sh VERSION VARIANT IMAGE...
#
# VERSION is the release (`0.3.0`, `1.2.0-rc.1`), VARIANT `debian`, `alpine`
# or `distroless`. A release gets `IMAGE:VERSION`, plus `IMAGE:MAJOR.MINOR`,
# `IMAGE:MAJOR` (from 1.0) and `IMAGE:latest` unless it is a pre-release.
# Variants other than debian add their name as a suffix (`0.3-alpine`), and
# their floating tag is the bare name (`alpine` instead of `latest`).
set -eu

if [ "$#" -lt 3 ]; then
    echo "usage: $0 VERSION VARIANT IMAGE..." >&2
    exit 2
fi
version=$1
variant=$2
shift 2

case $variant in
    debian) suffix= floating=latest ;;
    alpine | distroless) suffix=-$variant floating=$variant ;;
    *)
        echo "unknown variant: $variant" >&2
        exit 2
        ;;
esac

if ! echo "$version" | grep -Eq '^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z.-]+)?$'; then
    echo "not a release version: $version" >&2
    exit 2
fi

major=${version%%.*}
minor=${version#*.}
minor=${minor%%.*}

for image in "$@"; do
    echo "$image:$version$suffix"
    case $version in
        *-*) ;;
        *)
            echo "$image:$major.$minor$suffix"
            if [ "$major" -gt 0 ]; then
                echo "$image:$major$suffix"
            fi
            echo "$image:$floating"
            ;;
    esac
done
