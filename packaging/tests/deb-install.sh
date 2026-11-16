#!/bin/bash
# Installs, upgrades and removes the LSW .deb beside suckless-tools, which
# also ships /usr/bin/lsw (see debian/lsw.preinst). Run as root on a
# disposable Ubuntu system (CI runner or container) with the LSW package not
# yet installed:
#
#   sudo packaging/tests/deb-install.sh ./lsw_<version>_amd64.deb
set -euo pipefail
NEW=$(realpath "$1")
export DEBIAN_FRONTEND=noninteractive
work=$(mktemp -d)
failures=0

show_logs() {
    local log
    for log in "$work/apt.log" "$work/dpkg.log"; do
        [ -s "$log" ] && { echo "--- ${log##*/}:"; tail -n 40 "$log"; }
    done
    return 0
}
# A failing setup command stops the script (set -e): show what it printed.
on_exit() {
    local status=$?
    [ "$status" -eq 0 ] || show_logs
    rm -rf "$work"
}
trap on_exit EXIT
check() {
    local what=$1; shift
    if "$@"; then
        echo "PASS  $what"
    else
        echo "FAIL  $what"; failures=$((failures + 1)); show_logs
    fi
}
# The broken package failed where intended: at unpack, after preinst ran.
failed_at_unpack() { grep -q "trying to overwrite '/usr/bin/dpkg-split'" "$work/dpkg.log"; }
apt_get() { apt-get -y -q -o Dpkg::Use-Pty=0 "$@" >"$work/apt.log" 2>&1; }
is_lsw() { /usr/bin/lsw --version 2>/dev/null | grep -q '^lsw 0'; }
is_suckless() { [ "$(sha256sum <"$1")" = "$SUCKLESS_SUM" ]; }
no_diversion() { [ -z "$(dpkg-divert --list /usr/bin/lsw)" ]; }
# Installed, and dpkg --verify finds nothing outside the documentation
# (container images leave it out).
verified() {
    local problems
    [ "$(dpkg-query -W -f '${db:Status-Abbrev}' "$1" 2>/dev/null)" = "ii " ] || return 1
    problems=$(dpkg --verify "$1" 2>&1 | grep -vE ' /usr/share/(doc|man)/' || true)
    [ -z "$problems" ]
}
not_installed() { [ "$(dpkg-query -W -f '${db:Status-Abbrev}' lsw 2>/dev/null | cut -c2)" != i ]; }

# The released 0.0.1~alpha: same files, no diversion.
dpkg-deb -R "$NEW" "$work/old"
rm "$work/old/DEBIAN/preinst" "$work/old/DEBIAN/prerm" "$work/old/DEBIAN/postrm"
sed -i 's/^Version: .*/Version: 0.0.1~alpha/' "$work/old/DEBIAN/control"
dpkg-deb -b "$work/old" "$work/old.deb" >/dev/null
# This version, failing to unpack: it also contains dpkg's /usr/bin/dpkg-split.
dpkg-deb -R "$NEW" "$work/bad"
cp /usr/bin/dpkg-split "$work/bad/usr/bin/dpkg-split"
dpkg-deb -b "$work/bad" "$work/bad.deb" >/dev/null

apt-get update -q >/dev/null
apt_get install suckless-tools
SUCKLESS_SUM=$(sha256sum </usr/bin/lsw)

echo "== suckless-tools first, then LSW"
check "LSW installs" apt_get install "$NEW"
check "/usr/bin/lsw is LSW" is_lsw
check "suckless-tools' lsw is /usr/bin/lsw.suckless-tools" is_suckless /usr/bin/lsw.suckless-tools
check "dpkg --verify lsw" verified lsw

echo "== a reinstall keeps the diversion"
check "LSW reinstalls" apt_get install --reinstall "$NEW"
check "/usr/bin/lsw is LSW" is_lsw
check "suckless-tools' lsw is still /usr/bin/lsw.suckless-tools" is_suckless /usr/bin/lsw.suckless-tools

echo "== a failed reinstall keeps the diversion"
check "the broken package fails to unpack" bash -c "! dpkg -i '$work/bad.deb' >'$work/dpkg.log' 2>&1"
check "it failed at unpack" failed_at_unpack
check "/usr/bin/lsw is LSW" is_lsw
check "suckless-tools' lsw is still /usr/bin/lsw.suckless-tools" is_suckless /usr/bin/lsw.suckless-tools
check "dpkg --verify lsw" verified lsw

echo "== removing LSW gives the name back"
check "LSW is removed" apt_get remove lsw
check "no diversion left" no_diversion
check "/usr/bin/lsw is suckless-tools'" is_suckless /usr/bin/lsw
check "no /usr/bin/lsw.suckless-tools left" test ! -e /usr/bin/lsw.suckless-tools
check "dpkg --verify suckless-tools" verified suckless-tools

echo "== a failed install leaves suckless-tools' lsw in place"
for state in "removed, config left" "never installed"; do
    check "install fails ($state)" bash -c "! dpkg -i '$work/bad.deb' >'$work/dpkg.log' 2>&1"
    check "it failed at unpack" failed_at_unpack
    check "LSW is not installed" not_installed
    check "no diversion left" no_diversion
    check "/usr/bin/lsw is suckless-tools'" is_suckless /usr/bin/lsw
    check "no /usr/bin/lsw.suckless-tools left" test ! -e /usr/bin/lsw.suckless-tools
    check "dpkg --verify suckless-tools" verified suckless-tools
    apt_get purge lsw
done

echo "== someone else's diversion of /usr/bin/lsw"
dpkg-divert --local --divert /usr/bin/lsw.local --add /usr/bin/lsw >/dev/null
check "LSW does not install" bash -c "! dpkg -i '$NEW' >'$work/dpkg.log' 2>&1"
check "and is not left half-installed" bash -c "[ \"\$(dpkg-query -W -f '\${db:Status-Abbrev}' lsw | cut -c2-3)\" = 'n ' ]"
check "the local diversion is untouched" bash -c "[ \"\$(dpkg-divert --listpackage /usr/bin/lsw)\" = LOCAL ]"
dpkg-divert --local --remove /usr/bin/lsw >/dev/null
apt_get purge lsw

echo "== LSW first, then suckless-tools; then suckless-tools removed"
apt_get purge suckless-tools lsw
check "LSW installs" apt_get install "$NEW"
check "suckless-tools installs" apt_get install suckless-tools
check "/usr/bin/lsw is LSW" is_lsw
check "suckless-tools' lsw is /usr/bin/lsw.suckless-tools" is_suckless /usr/bin/lsw.suckless-tools
check "suckless-tools is removed" apt_get purge suckless-tools
check "/usr/bin/lsw is LSW" is_lsw
check "no /usr/bin/lsw.suckless-tools left" test ! -e /usr/bin/lsw.suckless-tools
check "LSW is purged" apt_get purge lsw
check "no diversion left" no_diversion
check "no /usr/bin/lsw left" test ! -e /usr/bin/lsw

echo "== upgrade from 0.0.1~alpha (no diversion)"
check "0.0.1~alpha installs" apt_get install "$work/old.deb"
check "a failed upgrade fails" bash -c "! dpkg -i '$work/bad.deb' >'$work/dpkg.log' 2>&1"
check "it failed at unpack" failed_at_unpack
check "the failed upgrade leaves no diversion" no_diversion
check "/usr/bin/lsw is LSW" is_lsw
check "dpkg --verify lsw" verified lsw
check "the upgrade succeeds" apt_get install "$NEW"
check "/usr/bin/lsw is LSW" is_lsw
check "no /usr/bin/lsw.suckless-tools" test ! -e /usr/bin/lsw.suckless-tools
check "dpkg --verify lsw" verified lsw
check "suckless-tools installs" apt_get install suckless-tools
check "suckless-tools' lsw is /usr/bin/lsw.suckless-tools" is_suckless /usr/bin/lsw.suckless-tools
check "LSW is removed" apt_get remove lsw
check "/usr/bin/lsw is suckless-tools'" is_suckless /usr/bin/lsw
check "no diversion left" no_diversion

echo "== downgrade to 0.0.1~alpha"
check "LSW installs" apt_get install "$NEW"
check "beside suckless-tools, the downgrade is refused" bash -c "! dpkg -i '$work/old.deb' >'$work/dpkg.log' 2>&1"
check "with a message" grep -q "cannot be installed beside suckless-tools" "$work/dpkg.log"
check "LSW is still this version" bash -c "[ \"\$(dpkg-query -W -f '\${Version} \${db:Status-Abbrev}' lsw)\" = '$(dpkg-deb -f "$NEW" Version) ii ' ]"
check "/usr/bin/lsw is LSW" is_lsw
check "suckless-tools' lsw is still /usr/bin/lsw.suckless-tools" is_suckless /usr/bin/lsw.suckless-tools
check "dpkg --verify lsw" verified lsw
rm /usr/bin/lsw.suckless-tools
check "with suckless-tools' lsw deleted by hand, the downgrade is still refused" bash -c "! dpkg -i '$work/old.deb' >'$work/dpkg.log' 2>&1"
apt_get purge suckless-tools
check "without suckless-tools, the downgrade works" bash -c "dpkg -i '$work/old.deb' >'$work/dpkg.log' 2>&1"
check "the downgrade leaves no diversion" no_diversion
check "/usr/bin/lsw is LSW" is_lsw
check "dpkg --verify lsw" verified lsw
check "LSW is purged" apt_get purge lsw
check "no /usr/bin/lsw left" test ! -e /usr/bin/lsw
apt_get install suckless-tools

echo "== upgrade from 0.0.1~alpha forced in with --force-overwrite"
check "0.0.1~alpha forced in" bash -c "dpkg -i --force-overwrite '$work/old.deb' >'$work/dpkg.log' 2>&1"
check "the upgrade succeeds" apt_get install "$NEW"
check "and says how to get suckless-tools' lsw back" grep -q "apt install --reinstall suckless-tools" "$work/apt.log"
check "which works" apt_get install --reinstall suckless-tools
check "suckless-tools' lsw is /usr/bin/lsw.suckless-tools" is_suckless /usr/bin/lsw.suckless-tools
check "/usr/bin/lsw is LSW" is_lsw
check "LSW is purged" apt_get purge lsw
check "/usr/bin/lsw is suckless-tools'" is_suckless /usr/bin/lsw
check "no diversion left" no_diversion

echo "== 0.0.1~alpha beside suckless-tools fails (the original bug)"
check "0.0.1~alpha does not install" bash -c "! apt-get -y install '$work/old.deb' >'$work/apt.log' 2>&1"
check "with the overwrite error" grep -q "trying to overwrite '/usr/bin/lsw'" "$work/apt.log"

if [ "$failures" -ne 0 ]; then echo "$failures check(s) failed"; exit 1; fi
echo "All checks passed"
