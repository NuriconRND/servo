/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

// Non-standard, experimental element for displaying image formats beyond the
// browser-standard set (TIFF, OpenEXR, HDR, TGA, DDS, QOI, PNM, ...), decoded
// via the `image` crate's extended decoders. Separate from <img>, which keeps
// its standard format set. Gated behind the `dom_x_image_enabled` pref.
[Exposed=Window, Pref="dom_x_image_enabled"]
interface XImageElement : HTMLElement {
  [HTMLConstructor] constructor();

  // The image URL to fetch and decode.
  [CEReactions] attribute USVString src;

  // Presentation box size (reflected content attributes), like <img>.
  [CEReactions] attribute unsigned long width;
  [CEReactions] attribute unsigned long height;

  // Natural dimensions of the decoded image, 0 until decoded.
  readonly attribute unsigned long naturalWidth;
  readonly attribute unsigned long naturalHeight;

  // True once a frame has been decoded and is ready to display.
  readonly attribute boolean complete;
};
