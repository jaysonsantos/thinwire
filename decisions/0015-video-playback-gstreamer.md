# 0015: Play videos in the app with GStreamer

**Status:** accepted (2026-11-05)

**Date:** 2026-11-05

**Refs:** ADR [0003](0003-main-only-ci-artifacts.md), ADR [0013](0013-ship-all-protocols-agpl-helpers.md)

## Context

The owner asked on 2026-11-05 for video playback "on all platforms that support it". Telegram, WhatsApp, Signal, and Discord send short clips. Most clips are H.264 or H.265 in MP4. Discord also sends VP9 in WebM.

egui has no video player. Two libraries can decode the clips:

- FFmpeg, through `ffmpeg-next` or a similar binding. The app decodes each frame, and it keeps the audio in time with the picture itself.
- GStreamer, through `gstreamer-rs`. A `playbin` pipeline plays the audio and keeps it in time. An `appsink` gives each frame to the app.

The owner preferred GStreamer and accepted FFmpeg.

## Decision

1. The app plays videos with GStreamer, behind the cargo feature `video` of the app.
2. `playbin` plays the file. It plays the audio through its own audio sink. An `appsink` gives RGBA frames, and the app shows each frame as an egui texture in the message row.
3. The app links GStreamer dynamically. A release ships only LGPL parts: core, base, good, and the decoders of the platform (VideoToolbox on macOS, Media Foundation or D3D11 on Windows, VA-API or libav on Linux). A release does not ship `gst-plugins-ugly`, x264, or any GPL plugin.
4. A video downloads when the user presses Play, as a voice note does. The file goes in the folder of the adapter (`thinwire_protocol::voice_files`). A helper sends a handle, not the bytes.
5. Default and public CI jobs stay feature-off. The `release-features` job turns `video` on.
6. A build with no `video` feature shows the video row with a label, as today.

## Consequences

- The macOS and Windows zips must carry the GStreamer runtime, its plugins, and a plugin registry. This adds tens of MB. Each dylib on macOS needs the Developer ID signature before notarization.
- CI on macOS and Windows needs the GStreamer development files for the release build.
- If the macOS packaging spike fails, a new ADR replaces this one with FFmpeg before the video UI work starts.
