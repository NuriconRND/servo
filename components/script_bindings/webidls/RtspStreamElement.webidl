/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

// Non-standard, experimental element for playing back web-non-standard live
// video sources (e.g. RTSP) without touching the standard <video>/
// HTMLMediaElement code path. Gated behind the `dom_rtsp_stream_enabled` pref;
// when the pref is off the interface is not exposed and `<rtsp-stream>` parses
// as a plain HTMLElement.
[Exposed=Window, Pref="dom_rtsp_stream_enabled"]
interface RtspStreamElement : HTMLElement {
  [HTMLConstructor] constructor();

  // The rtsp:// URL to play.
  [CEReactions] attribute USVString src;

  // Presentation box size (reflected content attributes), like <video>.
  [CEReactions] attribute unsigned long width;
  [CEReactions] attribute unsigned long height;

  // Natural dimensions reported by the media backend once known.
  readonly attribute unsigned long videoWidth;
  readonly attribute unsigned long videoHeight;

  // True once playback has been started and not stopped/errored.
  readonly attribute boolean playing;

  undefined play();
  undefined stop();
};
