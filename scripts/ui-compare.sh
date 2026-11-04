#!/usr/bin/env bash
# Build a before / after comparison of the UI snapshots.
#
# usage: scripts/ui-compare.sh [base-ref] [out-dir]
#
# "before" is the snapshot set at `base-ref` (default HEAD). "after" is the
# snapshot set in the working tree (`scripts/update-snapshots.sh` writes it).
# The output holds `before/`, `after/`, `compare/` (before on the left, after on
# the right), and `index.html`, a gallery to open in a browser.
#
# ImageMagick builds the side-by-side images. Without it on PATH, the script
# runs it through `nix shell`.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$(dirname "$script_dir")"

base="${1:-HEAD}"
out="${2:-docs/ui-revamp}"
snapshots="crates/thinwire/tests/snapshots"

magick_run() {
    if command -v magick >/dev/null 2>&1; then
        magick "$@"
    else
        nix shell nixpkgs#imagemagick --command magick "$@"
    fi
}

rm -rf "${out:?}/before" "${out:?}/after" "${out:?}/compare"
mkdir -p "$out/before" "$out/after" "$out/compare"

# The snapshot set at the base ref. Names that did not exist there are new.
while IFS= read -r path; do
    case "$path" in
    *.png) git show "$base:$path" >"$out/before/$(basename "$path")" ;;
    *) ;;
    esac
done < <(git ls-tree --name-only -r "$base" -- "$snapshots")

# Only finished snapshots. A failed run leaves `.new.png` and `.diff.png`
# files, and an update run leaves `.old.png` backups.
for file in "$snapshots"/*.png; do
    case "$file" in
    *.new.png | *.diff.png | *.old.png) continue ;;
    *) cp "$file" "$out/after/" ;;
    esac
done

changed=0
unchanged=0
added=0
for after in "$out"/after/*.png; do
    name="$(basename "$after")"
    before="$out/before/$name"
    if [ ! -f "$before" ]; then
        added=$((added + 1))
        cp "$after" "$out/compare/$name"
    elif cmp -s "$before" "$after"; then
        unchanged=$((unchanged + 1))
    else
        changed=$((changed + 1))
        magick_run "$before" "$after" -background '#8a8f98' -gravity center \
            -splice 0x0 +append -bordercolor '#8a8f98' -border 6 "$out/compare/$name"
    fi
done

gallery="$out/index.html"
{
    echo '<!doctype html><meta charset="utf-8"><title>thinwire UI: before and after</title>'
    echo '<style>'
    echo 'body{font:15px/1.5 system-ui,sans-serif;margin:24px;background:#f4f5f7;color:#14161a}'
    echo 'h1{margin:0 0 4px}h2{margin:32px 0 8px;font-size:18px}'
    echo '.pair{display:flex;gap:12px;flex-wrap:wrap;margin:8px 0}'
    echo 'figure{margin:0;flex:1 1 420px}figcaption{font-size:13px;color:#5c626c;margin:0 0 4px}'
    echo 'img{width:100%;height:auto;border:1px solid #c4c8cf;border-radius:6px;background:#fff}'
    echo 'nav a{margin-right:12px}'
    echo '</style>'
    echo '<h1>thinwire UI: before and after</h1>'
    echo "<p>Base: <code>$base</code>. Left is before, right is after. $changed changed, $added new, $unchanged unchanged.</p>"
    echo '<nav>'
    for after in "$out"/after/*.png; do
        name="$(basename "$after" .png)"
        scene="${name%%-*}"
        echo "$scene"
    done | sort -u | while IFS= read -r scene; do
        echo "<a href=\"#$scene\">$scene</a>"
    done
    echo '</nav>'
    for after in "$out"/after/*.png; do
        name="$(basename "$after" .png)"
        echo "${name%%-*}"
    done | sort -u | while IFS= read -r scene; do
        echo "<h2 id=\"$scene\">$scene</h2>"
        for after in "$out"/after/"$scene"-*.png; do
            name="$(basename "$after" .png)"
            echo '<div class="pair">'
            if [ -f "$out/before/$name.png" ]; then
                echo "<figure><figcaption>before · $name</figcaption><img loading=\"lazy\" src=\"before/$name.png\" alt=\"before $name\"></figure>"
            else
                echo '<figure><figcaption>before · (new screen)</figcaption></figure>'
            fi
            echo "<figure><figcaption>after · $name</figcaption><img loading=\"lazy\" src=\"after/$name.png\" alt=\"after $name\"></figure>"
            echo '</div>'
        done
    done
} >"$gallery"

echo "changed: $changed, new: $added, unchanged: $unchanged"
echo "gallery: $gallery"
echo "side by side: $out/compare/"
