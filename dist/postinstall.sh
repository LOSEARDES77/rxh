#!/bin/sh
# Postinstall script of the .deb and .rpm packages (dist/nfpm.yaml): creates
# the group rxh of the admin socket, which rxh.service runs with. pacman runs
# systemd-sysusers by itself.
set -e
if command -v systemd-sysusers >/dev/null 2>&1; then
    systemd-sysusers /usr/lib/sysusers.d/rxh.conf
elif ! getent group rxh >/dev/null 2>&1; then
    groupadd --system rxh
fi
