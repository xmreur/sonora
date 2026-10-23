#!/usr/bin/env bash
# tools/mem.sh — report resident memory (RSS) of a running Sonora app.
#
# Usage:
#   tools/mem.sh            # both the old Tauri app and the egui app,
#                            # including the WebKitGTK helper processes
#   tools/mem.sh 'sonora'   # only processes whose name matches the pattern
#
# Prints one line per process plus a total, so old (multi-process Tauri) and
# new (single-process egui) builds can be compared directly.

set -euo pipefail

pattern="${1:-sonora|apple-music-lin|Sonora|WebKit}"

ps -eo pid=,rss=,comm=,args= | awk -v pat="$pattern" '
    BEGIN { IGNORECASE = 1 }
    $3 ~ pat {
        cmd = $4
        for (i = 5; i <= NF; i++) cmd = cmd " " $i
        printf "%-8d %8.1f  %s\n", $1, $2 / 1024, cmd
        total += $2
        n++
    }
    END {
        if (n == 0) {
            print "no matching processes"
        } else {
            printf "TOTAL    %8.1f MB across %d process(es)\n", total / 1024, n
        }
    }
'
