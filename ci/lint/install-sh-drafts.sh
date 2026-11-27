#!/usr/bin/env bash
# Checks that install.sh skips a draft release: extracts select_release from
# the script (the script itself is not run) and feeds it the same releases
# list as install-drafts.sh, split the way install.sh's api_get splits it.
# Also checks the script picks its release through it.
# Usage: install-sh-drafts.sh  (run from the repo root)
set -euo pipefail
script=scripts/install.sh
fn=$(sed -n '/^select_release() {$/,/^}$/p' "$script")
[ -n "$fn" ] || { echo "install.sh drafts check: select_release not found in $script"; exit 1; }
grep -q '| select_release ' "$script" || { echo "install.sh drafts check: $script never calls select_release"; exit 1; }
eval "$fn"
failed=0
for case in '0 v0.3.0' '1 v0.2.1'; do
    set -- $case
    picked=$(tr ',{}[]' '\n\n\n\n\n' < ci/fixtures/releases-draft.json | select_release "$1")
    [ "$picked" = "$2" ] || { echo "install.sh drafts check: stable=$1 picked '$picked', expected $2"; failed=1; }
done
[ "$failed" -eq 0 ] || exit 1
echo "install.sh drafts check passed: draft v0.4.0 skipped; picked v0.3.0, and v0.2.1 with --stable"
