#!/bin/bash
# Copies contenTTS, which reads the page aloud, into web/contentts/: from
# its track beside this one, or from the workspace named.
#
#   vendor-contentts.sh [CONTENTTS_WORKSPACE]
#
# projj keeps a copy so that it builds with nothing beside it. Change
# contenTTS in its own track, then run this to bring the change in.
set -euo pipefail
here="$(cd "$(dirname "$0")/.." && pwd)"
from="$(cd "${1:-$here/../../contenTTS.git/sts-0}" && pwd)"
[ -f "$from/src/contentts.js" ] || { echo "vendor-contentts.sh: no contenTTS in $from" >&2; exit 1; }

dest="$here/web/contentts"
rm -rf "$dest"
mkdir -p "$dest"
cp "$from"/src/*.js "$from/LICENSE" "$dest/"

# Which change the copy is of: the latest with anything in it, which is the
# working copy itself when it has edits not yet committed.
change="$(jj -R "$from" log --no-graph -r 'heads(::@ & ~empty())' \
  -T 'change_id.short(12) ++ " " ++ commit_id.short(12) ++ " " ++ if(current_working_copy, "(not yet committed) ") ++ description.first_line()' \
  2>/dev/null || echo "unknown")"
cat > "$dest/SOURCE" <<EOF
A copy of contenTTS's src/, made by scripts/vendor-contentts.sh from
$change
Change it in contenTTS (track/github.com/memoremix-ing/contenTTS.git), not here.
EOF
echo "copied contenTTS ($change) into web/contentts/"
