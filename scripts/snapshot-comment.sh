#!/usr/bin/env bash
# Build one sticky-comment body when UI snapshots differ.
# Lists each changed screen and points at its old, new, and diff images.
# in the ui-snapshots artifact.
set -euo pipefail

root="${1:?snapshot directory}"
run_id="${2:?actions run id}"
out="${3:?output markdown path}"

{
  echo "## UI snapshots differ"
  echo
  echo "The \`ui-snapshots\` artifact for run ${run_id} holds the images (14 days)."
  echo
} > "$out"

found=0
while IFS= read -r diff; do
  found=1
  base="${diff%.diff.png}"
  name="$(basename "$base")"
  {
    echo "### ${name}"
    echo
    echo "- old: \`${name}.png\`"
    echo "- new: \`${name}.new.png\`"
    echo "- diff: \`${name}.diff.png\`"
    echo
  } >> "$out"
done < <(find "$root" -name '*.diff.png' | sort)

if [[ "$found" -eq 0 ]]; then
  echo "No diff image was written. See the job log." >> "$out"
fi
