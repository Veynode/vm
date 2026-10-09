#!/bin/sh
# Explicit fault injection inside the disposable macOS Guest only.
# Usage: sh macos-files-worker-fault.sh ABS_STAGE6_BINARY SHA256 ABS_FILES_ROOT
set -eu
test "$(/usr/bin/uname -s)" = Darwin
case "$(/usr/sbin/sysctl -n hw.model)" in VirtualMac*) ;; *) echo 'Refused: VirtualMac Guest required' >&2; exit 1;; esac
fault_uid=$(/usr/bin/id -u)
test "$fault_uid" -ne 0
test "$#" -eq 3
fault_binary=$1
fault_hash=$2
fault_root=$3
case "$fault_binary:$fault_root" in *[[:space:]]*) echo 'Refused: diagnostic paths must not contain whitespace' >&2; exit 1;; esac
case "$fault_binary" in /*) ;; *) exit 1;; esac
case "$fault_root" in /*) ;; *) exit 1;; esac
test -f "$fault_binary" && test -x "$fault_binary" && test ! -L "$fault_binary"
test -d "$fault_root" && test ! -L "$fault_root"
test "$(/usr/bin/stat -f %u "$fault_binary")" = "$fault_uid"
test "$(/usr/bin/stat -f %u "$fault_root")" = "$fault_uid"
test "$(/usr/bin/stat -f %Lp "$fault_root")" = 700
test "${#fault_hash}" -eq 64
case "$fault_hash" in *[!0-9a-f]*) exit 1;; esac
test "$(/usr/bin/shasum -a 256 "$fault_binary" | /usr/bin/awk '{print $1}')" = "$fault_hash"
# No host process inventory occurs before the VirtualMac/non-root checks above.
# Only exact executable/root arguments and the same UID are admitted. Legacy
# execution permission, wrappers and unknown arguments are deliberately refused.
fault_service=$(/bin/ps -ww -axo pid=,ppid=,uid=,command= | /usr/bin/awk -v uid="$fault_uid" -v binary="$fault_binary" -v root="$fault_root" '
  $3 == uid && $4 == binary {
    okay=1; files=0; rooted=0; observe=0; input=0;
    for (i=5;i<=NF;i++) {
      if ($i=="--allow-files") files++;
      else if ($i=="--allow-observe") observe++;
      else if ($i=="--allow-input") input++;
      else if ($i=="--files-root" && i<NF && $(i+1)==root) {rooted++;i++;}
      else okay=0;
    }
    if (okay && files==1 && rooted==1 && observe<=1 && input<=1 && (!input || observe)) {count++;pid=$1;}
  }
  END {if (count==1 && pid>1) print pid;else exit 1;}')
fault_child=$(/bin/ps -ww -axo pid=,ppid=,uid=,command= | /usr/bin/awk -v parent="$fault_service" -v uid="$fault_uid" -v binary="$fault_binary" -v root="$fault_root" '
  $2==parent && $3==uid && NF==7 && $4==binary && $5=="--file-worker" && $6=="--files-root" && $7==root {count++;pid=$1;}
  END {if (count==1 && pid>1) print pid;else exit 1;}')
case "$fault_service:$fault_child" in *[!0-9:]*) exit 1;; esac
test "$fault_service" -gt 1 && test "$fault_child" -gt 1 && test "$fault_child" -ne "$fault_service"
# Re-read exact child identity immediately before the one explicit signal.
# The trusted service owns/reaps this child; this is not a general process killer.
fault_before=$(/bin/ps -ww -p "$fault_child" -o pid=,ppid=,uid=,lstart=,command=)
test -n "$fault_before"
/bin/ps -ww -p "$fault_child" -o pid=,ppid=,uid=,command= | /usr/bin/awk -v child="$fault_child" -v parent="$fault_service" -v uid="$fault_uid" -v binary="$fault_binary" -v root="$fault_root" '
  $1==child && $2==parent && $3==uid && NF==7 && $4==binary && $5=="--file-worker" && $6=="--files-root" && $7==root {okay=1;}
  END {exit !okay;}'
test "$fault_before" = "$(/bin/ps -ww -p "$fault_child" -o pid=,ppid=,uid=,lstart=,command=)"
kill -KILL "$fault_child"
printf 'Sent SIGKILL to verified files worker pid=%s; service pid=%s was not signalled.\n' "$fault_child" "$fault_service"
