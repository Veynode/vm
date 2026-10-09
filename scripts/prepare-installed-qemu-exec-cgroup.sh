#!/bin/sh
# Installed service prerequisite. The Go service independently verifies clone3,
# the root0700 job subtree and cgroup.kill before emitting its serial challenge.
set -eu
[ "$(id -u)" = 0 ]
[ ! -L /sys/fs/cgroup ] && [ -d /sys/fs/cgroup ]
kind=$(awk '$2 == "/sys/fs/cgroup" {print $3}' /proc/mounts)
case "$kind" in
    '') mount -t cgroup2 -o nosuid,nodev,noexec cgroup2 /sys/fs/cgroup ;;
    cgroup2) ;;
    *) echo VEYNODE_EXEC_CGROUP_REJECTED >&2; exit 1 ;;
esac
[ "$(awk '$2 == "/sys/fs/cgroup" {print $3}' /proc/mounts)" = cgroup2 ]
[ "$(stat -c %u /sys/fs/cgroup)" = 0 ]
[ -f /sys/fs/cgroup/cgroup.controllers ]
# Do not change ownership or grant subtree delegation to the execution account.
echo VEYNODE_EXEC_CGROUP2_MOUNT_VERIFIED
