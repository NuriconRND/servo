/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

// https://drafts.csswg.org/web-animations-1/#the-animation-interface
//
// Servo 최소 구현. 재생 제어(play/pause/reverse/finish), 프라미스
// (ready/finished), 이벤트(finish/cancel/remove), playState 는 없다.
// 범위: docs/superpowers/specs/2026-09-17-web-animations-minimal-design.md
[Pref="dom_web_animations_enabled", Exposed=Window]
interface Animation {
  undefined cancel();
};

// https://drafts.csswg.org/web-animations-1/#the-effecttiming-dictionaries
enum FillMode { "none", "forwards", "backwards", "both", "auto" };
