#!/bin/bash
# Copies read-aloud, which reads the page aloud, into web/read-aloud/: from
# its track beside this one, or from the workspace named.
#
#   vendor-read-aloud.sh [READ_ALOUD_WORKSPACE]
#
# projj keeps a copy so that it builds with nothing beside it. Change
# read-aloud in its own track, then run this to bring the change in.
set -euo pipefail
here="$(cd "$(dirname "$0")/.." && pwd)"
from="$(cd "${1:-$here/../../read-aloud.git/sts-0}" && pwd)"
[ -f "$from/src/read-aloud.js" ] || { echo "vendor-read-aloud.sh: no read-aloud in $from" >&2; exit 1; }

dest="$here/web/read-aloud"
rm -rf "$dest"
mkdir -p "$dest"
cp "$from"/src/*.js "$from/LICENSE" "$dest/"

# Which change the copy is of: the latest with anything in it, which is the
# working copy itself when it has edits not yet committed.
change="$(jj -R "$from" log --no-graph -r 'heads(::@ & ~empty())' \
  -T 'change_id.short(12) ++ " " ++ commit_id.short(12) ++ " " ++ if(current_working_copy, "(not yet committed) ") ++ description.first_line()' \
  2>/dev/null || echo "unknown")"
cat > "$dest/SOURCE" <<EOF
A copy of read-aloud's src/, made by scripts/vendor-read-aloud.sh from
$change
Change it in read-aloud (track/github.com/memoremix-ing/read-aloud.git), not here.
EOF
echo "copied read-aloud ($change) into web/read-aloud/"
