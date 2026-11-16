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
trap 'rm -rf "$work"' EXIT
failures=0

check() {
    local what=$1; shift
    if "$@"; then echo "PASS  $what"; else echo "FAIL  $what"; failures=$((failures + 1)); fi
}
apt_get() { apt-get -y -q -o Dpkg::Use-Pty=0 "$@" >"$work/apt.log" 2>&1; }
is_lsw() { /usr/bin/lsw --version 2>/dev/null | grep -q '^lsw 0'; }
is_suckless() { [ "$(sha256sum <"$1")" = "$SUCKLESS_SUM" ]; }
no_diversion() { [ -z "$(dpkg-divert --list /usr/bin/lsw)" ]; }
# dpkg --verify, ignoring documentation (container images leave it out).
verified() {
    local problems
    problems=$(dpkg --verify "$1" | grep -vE ' /usr/share/(doc|man)/' || true)
    [ -z "$problems" ]
}

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
check "/usr/bin/lsw is LSW" is_lsw
check "suckless-tools' lsw is still /usr/bin/lsw.suckless-tools" is_suckless /usr/bin/lsw.suckless-tools
check "dpkg --verify lsw" verified lsw

echo "== removing LSW gives the name back"
check "LSW is removed" apt_get remove lsw
check "no diversion left" no_diversion
check "/usr/bin/lsw is suckless-tools'" is_suckless /usr/bin/lsw
check "no /usr/bin/lsw.suckless-tools left" test ! -e /usr/bin/lsw.suckless-tools
check "dpkg --verify suckless-tools" verified suckless-tools

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
apt_get purge suckless-tools
check "without suckless-tools, the downgrade works" bash -c "dpkg -i '$work/old.deb' >'$work/dpkg.log' 2>&1"
check "the downgrade leaves no diversion" no_diversion
check "/usr/bin/lsw is LSW" is_lsw
check "dpkg --verify lsw" verified lsw
check "LSW is purged" apt_get purge lsw
check "no /usr/bin/lsw left" test ! -e /usr/bin/lsw
apt_get install suckless-tools

echo "== 0.0.1~alpha beside suckless-tools fails (the original bug)"
check "0.0.1~alpha does not install" bash -c "! apt-get -y install '$work/old.deb' >'$work/apt.log' 2>&1"
check "with the overwrite error" grep -q "trying to overwrite '/usr/bin/lsw'" "$work/apt.log"

if [ "$failures" -ne 0 ]; then echo "$failures check(s) failed"; exit 1; fi
echo "All checks passed"
