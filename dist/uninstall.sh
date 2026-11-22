#!/bin/sh
# Removes what install.sh installed. Run as root.
set -eu

systemctl disable --now overrunerd.socket overrunerd.service overrune-resume.service 2>/dev/null || true

rm -f /usr/bin/overrunerd /usr/bin/overrune-reapply /usr/bin/overrune \
    /etc/systemd/system/overrunerd.socket \
    /etc/systemd/system/overrunerd.service \
    /etc/systemd/system/overrune-resume.service \
    /usr/share/polkit-1/actions/com.github.overrune.manage.policy

systemctl daemon-reload
