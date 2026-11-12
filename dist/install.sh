#!/bin/sh
# Installs overrune's daemon, units and polkit action. Run as root, after
# `cargo build --release`.
set -eu
cd "$(dirname "$0")/.."

install -Dm755 target/release/overrunerd /usr/bin/overrunerd
# The resume unit's ExecStart: a one-shot re-apply of the active profile.
ln -sf overrunerd /usr/bin/overrune-reapply
[ -x target/release/overrune ] && install -Dm755 target/release/overrune /usr/bin/overrune

install -Dm644 dist/overrunerd.socket /etc/systemd/system/overrunerd.socket
install -Dm644 dist/overrunerd.service /etc/systemd/system/overrunerd.service
install -Dm644 dist/overrune-resume.service /etc/systemd/system/overrune-resume.service
install -Dm644 dist/com.github.overrune.manage.policy /usr/share/polkit-1/actions/com.github.overrune.manage.policy
