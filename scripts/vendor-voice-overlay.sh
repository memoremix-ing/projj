#!/bin/bash
# Copies voice-overlay, which reads the page aloud, into web/voice-overlay/: from
# its track beside this one, or from the workspace named.
#
#   vendor-voice-overlay.sh [VOICE_OVERLAY_WORKSPACE]
#
# projj keeps a copy so that it builds with nothing beside it. Change
# voice-overlay in its own track, then run this to bring the change in.
set -euo pipefail
here="$(cd "$(dirname "$0")/.." && pwd)"
from="$(cd "${1:-$here/../../voice-overlay.git/sts-0}" && pwd)"
[ -f "$from/src/voice-overlay.js" ] || { echo "vendor-voice-overlay.sh: no voice-overlay in $from" >&2; exit 1; }

dest="$here/web/voice-overlay"
rm -rf "$dest"
mkdir -p "$dest"
cp "$from"/src/*.js "$from/LICENSE" "$dest/"

# Which change the copy is of: the latest with anything in it, which is the
# working copy itself when it has edits not yet committed.
change="$(jj -R "$from" log --no-graph -r 'heads(::@ & ~empty())' \
  -T 'change_id.short(12) ++ " " ++ commit_id.short(12) ++ " " ++ if(current_working_copy, "(not yet committed) ") ++ description.first_line()' \
  2>/dev/null || echo "unknown")"
cat > "$dest/SOURCE" <<EOF
A copy of voice-overlay's src/, made by scripts/vendor-voice-overlay.sh from
$change
Change it in voice-overlay (track/github.com/memoremix-ing/voice-overlay.git), not here.
EOF
echo "copied voice-overlay ($change) into web/voice-overlay/"
