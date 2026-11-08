# GStreamer runtime in Thinwire.app

The macOS app plays videos with GStreamer (ADR `0015`). `scripts/bundle-gstreamer-macos.sh` copies a small part of the official GStreamer 1.28.8 framework into `Thinwire.app/Contents/Frameworks/GStreamer`. The app links these libraries dynamically. You can replace each library with a compatible build of the same version.

## Parts and licenses

| Part | Files | License | Text |
| --- | --- | --- | --- |
| GStreamer core, base, good, and bad (libraries and plugins) | `libgst*.dylib`, `gstreamer-1.0/libgst*.dylib` | LGPL-2.1-or-later | `LGPL-2.1.txt` |
| GLib, GObject, GModule, GIO | `libglib-2.0`, `libgobject-2.0`, `libgmodule-2.0`, `libgio-2.0` | LGPL-2.1-or-later | `LGPL-2.1.txt` |
| gettext runtime | `libintl` | LGPL-2.1-or-later | `LGPL-2.1.txt` |
| ORC | `liborc-0.4` | BSD-2-Clause | `orc-COPYING.txt` |
| PCRE2 | `libpcre2-8` | BSD-3-Clause with the PCRE2 exception | `pcre2-LICENCE.md` |
| libffi | `libffi` | MIT | `libffi-LICENSE.txt` |
| zlib | `libz` | Zlib | `zlib-LICENSE.txt` |
| bzip2 | `libbz2` | bzip2-1.0.6 | `bzip2-COPYING.txt` |
| Opus | `libopus` | BSD-3-Clause | `opus-COPYING.txt` |
| libvpx | `libvpx` | BSD-3-Clause, with a patent grant | `libvpx-LICENSE.txt`, `libvpx-PATENTS.txt` |
| MoltenVK | `libMoltenVK` | Apache-2.0 | `MoltenVK-LICENSE.txt` |

The app does not ship a plugin of `gst-plugins-ugly`, a GPL plugin, x264, or FFmpeg. The decoders of H.264, H.265, and AAC are the VideoToolbox and AudioToolbox decoders of macOS (`applemedia`).

## Source

The binaries come from the official package `gstreamer-1.0-1.28.8-universal.pkg` at <https://gstreamer.freedesktop.org/data/pkg/osx/1.28.8/>. `scripts/install-gstreamer-macos.sh` pins its SHA-256. The source of each part is at <https://gstreamer.freedesktop.org/src/> and in the Cerbero build recipes of tag `1.28.8` at <https://gitlab.freedesktop.org/gstreamer/cerbero>. On request, the maintainer of Thinwire sends a copy of the source of each LGPL part for three years after the release.
