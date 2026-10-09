#!/bin/sh
# Run only inside the disposable installed macOS Guest as its ordinary user.
set -eu
test "$(uname -s)" = Darwin
case "$(sysctl -n hw.model)" in VirtualMac*) ;; *) echo 'Guest required' >&2; exit 1;; esac
test "$(id -u)" -ne 0
umask 077
fixture_base="$HOME/veynode-files-fixture-stage1"
test ! -e "$fixture_base" && test ! -L "$fixture_base"
mkdir "$fixture_base"
mkdir "$fixture_base/files"
printf 'outside sentinel\n' > "$fixture_base/outside"
ln -s ../outside "$fixture_base/files/outside-link"
ln "$fixture_base/outside" "$fixture_base/files/outside-hardlink"
mkfifo "$fixture_base/files/fifo"
mkdir "$fixture_base/files/sub"
printf '%s\n' '{"fixture":"macos-files-v1","symlink":true,"hardlink":true,"fifo":true,"directory":true}' > "$fixture_base/files/fixture-manifest.json"
printf 'Prepared Guest files fixture uid=%s root=%s/files\n' "$(id -u)" "$fixture_base"
