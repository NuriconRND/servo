/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */
/*
 * The origin of this IDL file is
 * https://dom.spec.whatwg.org/#element and
 * https://w3c.github.io/DOM-Parsing/ and
 * http://dev.w3.org/csswg/cssom-view/ and
 * http://www.w3.org/TR/selectors-api/
 *
 * Copyright © 2012 W3C® (MIT, ERCIM, Keio), All Rights Reserved. W3C
 * liability, trademark and document use rules apply.
 */

[Exposed=Window]
interface Element : Node {
  [Constant]
  readonly attribute DOMString? namespaceURI;
  [Constant]
  readonly attribute DOMString? prefix;
  [Constant]
  readonly attribute DOMString localName;
  // Not [Constant] because it depends on which document we're in
  [Pure]
  readonly attribute DOMString tagName;

  [CEReactions, Pure]
           attribute DOMString id;
  [CEReactions, Pure]
           attribute DOMString className;
  [SameObject, PutForwards=value]
  readonly attribute DOMTokenList classList;
  [CEReactions, Unscopable] attribute DOMString slot;

  [Pure]
  boolean hasAttributes();
  [SameObject]
  readonly attribute NamedNodeMap attributes;
  [Pure]
  sequence<DOMString> getAttributeNames();
  [Pure]
  DOMString? getAttribute(DOMString name);
  [Pure]
  DOMString? getAttributeNS(DOMString? namespace, DOMString localName);
  [CEReactions, Throws]
  boolean toggleAttribute(DOMString name, optional boolean force);
  [CEReactions, Throws]
  undefined setAttribute(DOMString name, (TrustedType or DOMString) value);
  [CEReactions, Throws]
  undefined setAttributeNS(DOMString? namespace, DOMString name, (TrustedType or DOMString) value);
  [CEReactions]
  undefined removeAttribute(DOMString name);
  [CEReactions]
  undefined removeAttributeNS(DOMString? namespace, DOMString localName);
  boolean hasAttribute(DOMString name);
  boolean hasAttributeNS(DOMString? namespace, DOMString localName);

  [Pure]
  Attr? getAttributeNode(DOMString name);
  [Pure]
  Attr? getAttributeNodeNS(DOMString? namespace, DOMString localName);
  [CEReactions, Throws]
  Attr? setAttributeNode(Attr attr);
  [CEReactions, Throws]
  Attr? setAttributeNodeNS(Attr attr);
  [CEReactions, Throws]
  Attr removeAttributeNode(Attr oldAttr);

  [Pure, Throws]
  Element? closest(DOMString selectors);
  [Pure, Throws]
  boolean matches(DOMString selectors);
  [Pure, Throws]
  boolean webkitMatchesSelector(DOMString selectors); // historical alias of .matches

  HTMLCollection getElementsByTagName(DOMString localName);
  HTMLCollection getElementsByTagNameNS(DOMString? namespace, DOMString localName);
  HTMLCollection getElementsByClassName(DOMString classNames);

  [CEReactions, Throws]
  Element? insertAdjacentElement(DOMString where_, Element element); // historical
  [Throws]
  undefined insertAdjacentText(DOMString where_, DOMString data);
  [CEReactions, Throws]
  undefined insertAdjacentHTML(DOMString position, (TrustedHTML or DOMString) string);

  [Throws] ShadowRoot attachShadow(ShadowRootInit init);
  readonly attribute ShadowRoot? shadowRoot;

  readonly attribute CustomElementRegistry? customElementRegistry;
};

dictionary ShadowRootInit {
  required ShadowRootMode mode;
  boolean delegatesFocus = false;
  SlotAssignmentMode slotAssignment = "named";
  boolean clonable = false;
  boolean serializable = false;
};

// http://dev.w3.org/csswg/cssom-view/#extensions-to-the-element-interface
partial interface Element {
  DOMRectList getClientRects();
  [NewObject]
  DOMRect getBoundingClientRect();

  undefined scrollIntoView(optional (boolean or ScrollIntoViewOptions) arg = {});
  undefined scroll(optional ScrollToOptions options = {});
  undefined scroll(unrestricted double x, unrestricted double y);

  undefined scrollTo(optional ScrollToOptions options = {});
  undefined scrollTo(unrestricted double x, unrestricted double y);
  undefined scrollBy(optional ScrollToOptions options = {});
  undefined scrollBy(unrestricted double x, unrestricted double y);
  attribute unrestricted double scrollTop;
  attribute unrestricted double scrollLeft;
  readonly attribute long scrollWidth;
  readonly attribute long scrollHeight;

  readonly attribute long clientTop;
  readonly attribute long clientLeft;
  readonly attribute long clientWidth;
  readonly attribute long clientHeight;

  readonly attribute double currentCSSZoom;
};

// https://html.spec.whatwg.org/multipage/#dom-parsing-and-serialization
partial interface Element {
  [CEReactions, Throws] undefined setHTMLUnsafe((TrustedHTML or DOMString) html, optional SetHTMLUnsafeOptions options = {});
  DOMString getHTML(optional GetHTMLOptions options = {});

  [CEReactions, Throws] attribute (TrustedHTML or [LegacyNullToEmptyString] DOMString) innerHTML;
  [CEReactions, Throws] attribute (TrustedHTML or [LegacyNullToEmptyString] DOMString) outerHTML;
};

dictionary GetHTMLOptions {
  boolean serializableShadowRoots = false;
  sequence<ShadowRoot> shadowRoots = [];
};

// https://drafts.csswg.org/cssom-view/#dictdef-scrollintoviewoptions
dictionary ScrollIntoViewOptions : ScrollOptions {
  ScrollLogicalPosition block = "start";
  ScrollLogicalPosition inline = "nearest";
  ScrollIntoViewContainer container = "all";
};

enum ScrollLogicalPosition { "start", "center", "end", "nearest" };
enum ScrollIntoViewContainer { "all", "nearest" };

// https://fullscreen.spec.whatwg.org/#api
partial interface Element {
  Promise<undefined> requestFullscreen();
};

// https://w3c.github.io/pointerevents/#extensions-to-the-element-interface
partial interface Element {
  [Throws] undefined setPointerCapture(long pointerId);
  [Throws] undefined releasePointerCapture(long pointerId);
  boolean hasPointerCapture(long pointerId);
};

Element includes ChildNode;
Element includes NonDocumentTypeChildNode;
Element includes ParentNode;
Element includes ActivatableElement;
Element includes ARIAMixin;

// https://drafts.csswg.org/css-shadow-parts/#idl
partial interface Element {
  [SameObject, PutForwards=value] readonly attribute DOMTokenList part;
};

// https://wicg.github.io/sanitizer-api/#sanitizer-api
partial interface Element {
  [CEReactions, Throws] undefined setHTML(DOMString html, optional SetHTMLOptions options = {});
};

// https://drafts.csswg.org/web-animations-1/#the-animatable-interface-mixin
//
// Servo 최소 구현. 키프레임은 배열 형태만 받고, 값은 전부 문자열로 강제된다
// (`offset: 0.5` 는 "0.5" 로 들어와 우리가 파싱한다). 옵션은 아래 넷뿐이며,
// 선언되지 않은 멤버는 WebIDL 이 무시하므로 이 딕셔너리가 곧 지원 범위다.
// 범위: docs/superpowers/specs/2026-09-17-web-animations-minimal-design.md
// ★선언한 것만 구현되어 있다.★ WebIDL 은 사전(dictionary)의 모르는 멤버를 조용히
// 버리므로, 여기에 없는 옵션(`iterationStart`, `endDelay`, `composite` ...)을 넘기면
// 아무 일도 일어나지 않는다. 그것이 표준 동작이지만 조용하므로, 쓰는 쪽이 여기를 보고
// 무엇이 실제로 듣는지 알 수 있어야 한다.
//
// `iterationStart`/`endDelay` 는 stylo 가 애초에 모델하지 않는다(CSS 에 대응 속성이
// 없다). 넣으려면 `Animation` 에 새 상태를 더해야 하므로 범위가 다르다.
dictionary ServoKeyframeAnimationOptions {
  unrestricted double duration = 0;
  // 음수면 그만큼 이미 진행한 상태로 시작한다. CSS `animation-delay` 와 같은 뜻이다.
  // `unrestricted` 가 아니다 -- NaN/Infinity 는 의미가 없고 바인딩이 거부해야 한다.
  double delay = 0;
  DOMString easing = "linear";
  unrestricted double iterations = 1;
  FillMode fill = "auto";
  PlaybackDirection direction = "normal";
};

partial interface Element {
  [Pref="dom_web_animations_enabled", Throws]
  Animation animate(sequence<record<DOMString, DOMString>> keyframes,
                    optional ServoKeyframeAnimationOptions options = {});
};
