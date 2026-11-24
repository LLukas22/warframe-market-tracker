/*! @microsoft/focusgroup-polyfill 1.6.0, shadowless build, bundled by esbuild.
MIT License

    Copyright (c) Microsoft Corporation.

    Permission is hereby granted, free of charge, to any person obtaining a copy
    of this software and associated documentation files (the "Software"), to deal
    in the Software without restriction, including without limitation the rights
    to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
    copies of the Software, and to permit persons to whom the Software is
    furnished to do so, subject to the following conditions:

    The above copyright notice and this permission notice shall be included in all
    copies or substantial portions of the Software.

    THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
    IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
    FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
    AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
    LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
    OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
    SOFTWARE
*/

// @microsoft/focusgroup-polyfill/build/constants.mjs
var DatasetName = {
  INFERRED_ROLE: "data-fg-ir",
  ITEM: "data-fg-item",
  AUTHOR_TABINDEX: "data-fg-ati",
  SEGMENT: "data-fg-seg",
  SEGMENT_START: "data-fg-segs"
};
var BehaviorToken = {
  TOOLBAR: "toolbar",
  TABLIST: "tablist",
  RADIOGROUP: "radiogroup",
  LISTBOX: "listbox",
  MENU: "menu",
  MENUBAR: "menubar",
  NONE: "none"
};
var BEHAVIOR_TOKENS = [
  BehaviorToken.TOOLBAR,
  BehaviorToken.TABLIST,
  BehaviorToken.RADIOGROUP,
  BehaviorToken.LISTBOX,
  BehaviorToken.MENU,
  BehaviorToken.MENUBAR,
  BehaviorToken.NONE
];
var BehaviorMap = {
  toolbar: {
    ownerRole: "toolbar",
    childRole: null,
    wrap: false,
    axis: "inline"
  },
  tablist: {
    ownerRole: "tablist",
    childRole: "tab",
    wrap: true,
    axis: "inline"
  },
  radiogroup: {
    ownerRole: "radiogroup",
    childRole: "radio",
    wrap: true,
    axis: void 0
  },
  listbox: {
    ownerRole: "listbox",
    childRole: "option",
    wrap: false,
    axis: "block"
  },
  menu: {
    ownerRole: "menu",
    childRole: "menuitem",
    wrap: true,
    axis: "block"
  },
  menubar: {
    ownerRole: "menubar",
    childRole: "menuitem",
    wrap: true,
    axis: "inline"
  }
};

// @microsoft/focusgroup-polyfill/build/shadow-utils/index-shadowless.mjs
function getClosestElement(element, selector) {
  return element.closest(selector);
}
function nodeContains(node, otherNode) {
  return node.contains(otherNode);
}
function createMutationObserver(callback) {
  return new MutationObserver(callback);
}
function createTreeWalker(doc, root, whatToShow, filter) {
  return doc.createTreeWalker(root, whatToShow, filter);
}
function getParentElement(node) {
  return node.parentElement;
}

// @microsoft/focusgroup-polyfill/build/utils-shadowless.mjs
function hasDocument() {
  return typeof document !== "undefined";
}
function supportsFocusGroup() {
  return "focusgroup" in (globalThis?.HTMLElement?.prototype ?? {}) || "focusGroup" in (globalThis?.HTMLElement?.prototype ?? {});
}
function parseDefinition(owner) {
  const tokens = (owner.getAttribute("focusgroup") ?? "").split(" ");
  const behavior = tokens.find((token) => BEHAVIOR_TOKENS.includes(token)) ?? null;
  const base = BehaviorMap[behavior];
  let wrap = base?.wrap ?? false;
  if (tokens.includes("wrap")) wrap = true;
  else if (tokens.includes("nowrap")) wrap = false;
  const hasInline = tokens.includes("inline");
  const axis = hasInline === tokens.includes("block") ? hasInline ? void 0 : base?.axis : hasInline ? "inline" : "block";
  return {
    behavior,
    wrap,
    axis,
    memory: !tokens.includes("nomemory")
  };
}
var focusgroupCount = 0;
function generateUniqueId() {
  return String(focusgroupCount++);
}
function isKeyboardFocusable(element, owner) {
  return (element.isContentEditable || element.matches(":is(audio, video)[controls]") || element.tabIndex > -1) && !(element.disabled || element.hasAttribute("disabled") || element.matches(":is(a, area):not([href])") || element.inert || !checkVisibility(element, owner) || element.matches(":is(audio, video):not([controls])") || element.hasAttribute(DatasetName.AUTHOR_TABINDEX));
}
function getNavigationDirection(event, owner, axis) {
  const FORWARD = "forward";
  const BACKWARD = "backward";
  const BLOCK = "block";
  const INLINE = "inline";
  if (isKeyConflictElement(event.composedPath()[0])) return event.key === "Tab" ? event.shiftKey ? BACKWARD : FORWARD : null;
  if (event.shiftKey || event.ctrlKey || event.metaKey) return null;
  const { writingMode, direction } = window.getComputedStyle(owner);
  const isVertical = !writingMode.startsWith("horizontal-");
  const isRtl = direction === "rtl";
  const horizontal = isVertical ? BLOCK : INLINE;
  const vertical = isVertical ? INLINE : BLOCK;
  const isHorizontalReversed = isVertical ? writingMode.endsWith("-rl") !== isRtl : isRtl;
  const isVerticalReversed = isVertical && isRtl;
  const action = {
    ArrowUp: {
      axis: vertical,
      dir: isVerticalReversed ? FORWARD : BACKWARD
    },
    ArrowDown: {
      axis: vertical,
      dir: isVerticalReversed ? BACKWARD : FORWARD
    },
    ArrowLeft: {
      axis: horizontal,
      dir: isHorizontalReversed ? FORWARD : BACKWARD
    },
    ArrowRight: {
      axis: horizontal,
      dir: isHorizontalReversed ? BACKWARD : FORWARD
    },
    Home: { dir: "start" },
    End: { dir: "end" }
  }[event.key];
  if (!action || axis && action.axis && action.axis !== axis) return null;
  return action.dir;
}
function isKeyConflictElement(el) {
  return el?.nodeType === Node.ELEMENT_NODE && ([
    "INPUT",
    "TEXTAREA",
    "SELECT"
  ].includes(el.nodeName) && !["checkbox", "radio"].includes(el.getAttribute("type")) || el.isContentEditable || ["AUDIO", "VIDEO"].includes(el.nodeName) && el.hasAttribute("controls") || ["IFRAME", "OBJECT"].includes(el.nodeName));
}
function isSegmentor(element, owner) {
  if (!checkVisibility(element)) return false;
  if (isKeyboardFocusable(element, owner)) return element.getAttribute("focusgroup").includes(BehaviorToken.NONE);
  const walker = createTreeWalker(document, element, NodeFilter.SHOW_ELEMENT);
  while (walker.nextNode()) if (walker.currentNode !== element && isKeyboardFocusable(walker.currentNode, owner)) return true;
  return false;
}
function checkVisibility(element, ancestor) {
  if ("checkVisibility" in Element.prototype) return element.checkVisibility({
    visibilityProperty: true,
    contentVisibilityAuto: true
  });
  if (element.getClientRects().length === 0) return false;
  let current = element;
  while (current) {
    const { visibility, contentVisibility } = window.getComputedStyle(current);
    if (["hidden", "collapse"].includes(visibility)) return false;
    if (current !== element && contentVisibility === "hidden") return false;
    if (!ancestor || current === ancestor) break;
    current = getParentElement(current);
  }
  return true;
}
function inferRole(element, behavior, kind) {
  const allowRoleInferring = hasGenericRole(element) || kind === "child" && element.nodeName === "BUTTON";
  const cfg = BehaviorMap[behavior];
  const role = allowRoleInferring ? kind === "owner" ? cfg?.ownerRole : cfg?.childRole : void 0;
  if (role) {
    if (!element.hasAttribute("role") || element.hasAttribute(DatasetName.INFERRED_ROLE)) {
      element.setAttribute("role", role);
      element.setAttribute(DatasetName.INFERRED_ROLE, "");
    }
  } else if (element.hasAttribute(DatasetName.INFERRED_ROLE)) {
    element.removeAttribute("role");
    element.removeAttribute(DatasetName.INFERRED_ROLE);
  }
}
function hasGenericRole(element) {
  if ("computedRole" in HTMLElement.prototype) return element.computedRole === "generic";
  return ["DIV", "SPAN"].includes(element.nodeName) || element.nodeName.includes("-");
}

// @microsoft/focusgroup-polyfill/build/global-state.mjs
globalThis.__FOCUSGROUP_POLYFILL__ ??= {
  o: /* @__PURE__ */ new Set(),
  b: false
};
var state = globalThis.__FOCUSGROUP_POLYFILL__;

// @microsoft/focusgroup-polyfill/build/observer-registry.mjs
var observers = state.o;
function flushAllObservers() {
  for (const observer of observers) observer.takeRecords();
}

// @microsoft/focusgroup-polyfill/build/focusgroup-shadowless.mjs
var FocusGroup = class {
  /**
  * The focus group owner element.
  * @type {HTMLElement!}
  */
  #owner;
  /**
  * The items collection — exposes the focus group's items and answers
  * queries about them. Reconciliation is triggered externally via
  * `FocusGroup#update()`.
  * @type {FocusGroupItemCollection}
  */
  #items;
  /**
  * The focus group behavior.
  * @type {BehaviorToken | null}
  */
  #behavior = null;
  /**
  * The focus group navigation axis limitation.
  * @type {("inline" | "block" | undefined)}
  */
  #axis = void 0;
  /**
  * Whether the focus group wraps. Defaults to `false`.
  * @type {boolean}
  */
  #wrap = false;
  /**
  * Whether the focus group remembers the previously focused element.
  * Defaults to `true`.
  * @type {boolean}
  */
  #memory = true;
  /**
  * The focus group start element (initial tab stop after decoration).
  * @type {HTMLElement}
  */
  #start;
  /**
  * The current item — the most recently focused item within the group.
  * Serves as the keyboard-navigation cursor while focus is inside, and (in
  * memory mode) as the tab stop to restore on re-entry. Updated by
  * `#handleFocusin`, plus directly by `#handleKeydown` for shadow-internal
  * navigation (where focus events don't cross the shadow boundary).
  * Cleared in nomemory mode on focusout and when validation fails after
  * re-decoration.
  * @type {HTMLElement|null}
  */
  #current = null;
  /**
  * Whether the owner currently has `tabindex=0` set as a Tab-entry proxy so
  * sequential focus navigation can reach a tab stop inside a shadow root.
  * @type {boolean}
  */
  #ownerIsProxy = false;
  /**
  * The owner's original `tabindex` attribute value (or `null` if it had no
  * `tabindex`), saved before the polyfill sets `tabindex=0` for proxy duty.
  * @type {string|null}
  */
  #ownerTabindexBeforeProxy = null;
  /**
  * The abort controller for when `disconnect()` is called.
  * @type {AbortController}
  */
  #abort = new AbortController();
  /**
  * Optional owner-decoration hook injected via `options.decorateOwner`.
  * Called with `(owner, behavior)` on decoration and `(owner, null)` on
  * undecoration. When absent, no owner decoration happens.
  * @type {((element: HTMLElement, behavior: BehaviorToken|null) => void) | undefined}
  */
  #decorateOwner;
  /**
  * Optional item-decoration hook injected via `options.decorateItem`.
  * Called with `(item, behavior)` on decoration and `(item, null)` on
  * undecoration. When absent, no item decoration happens.
  * @type {((element: HTMLElement, behavior: BehaviorToken|null) => void) | undefined}
  */
  #decorateItem;
  /**
  * @param {HTMLElement!} owner - The focus group owner element.
  * @param {FocusGroupItemCollection} items - The items collection providing
  *     item discovery and queries.
  * @param {FocusGroupOptions} [options]
  */
  constructor(owner, items, options = {}) {
    if (supportsFocusGroup() || !owner) return;
    this.#owner = owner;
    this.#items = items;
    this.#decorateOwner = options.decorateOwner;
    this.#decorateItem = options.decorateItem;
    this.#updateDefinition(options.definition);
    this.#decorateOwner?.(this.#owner, this.#behavior);
    this.#decorateItems();
    const opts = { signal: this.#abort.signal };
    this.#owner.addEventListener("keydown", this.#handleKeydown.bind(this), opts);
    this.#owner.addEventListener("focusin", this.#handleFocusin.bind(this), opts);
    this.#owner.addEventListener("focusout", this.#handleFocusout.bind(this), opts);
  }
  /**
  * Tears down the focus group: disables the owner proxy, removes all event
  * listeners (via the abort signal), then disconnects the items collection
  * if it supports it.
  *
  * Ordering matters: owner-proxy teardown can trigger `flushAllObservers()`,
  * which expects the items' observer to still be in the global registry.
  * The items' own `disconnect()` is therefore called last.
  *
  * NOTE: This method does not undecorate the elements. Call it only after
  * the focusgroup owner has been removed from the DOM.
  */
  disconnect() {
    this.#disableFocusabilityProxy();
    this.#abort.abort();
    this.#items?.disconnect?.();
    this.#owner = null;
  }
  /**
  * Reconciles decoration state in response to relevant changes. Call this
  * whenever the focus group should refresh — e.g. items were added or
  * removed, the owner's `focusgroup` attribute changed, or an author set
  * `tabindex` on a decorated item.
  *
  * The polyfill's default `TreeWalkerItemCollection` calls this from a
  * `MutationObserver`. App-supplied collections (or app code that knows
  * when its model changed) can call it directly.
  *
  * @param {FocusGroupUpdateInfo} [info]
  */
  update(info = {}) {
    if (!this.#owner) return;
    if (info.definition !== void 0) {
      this.#updateDefinition(info.definition);
      this.#decorateOwner?.(this.#owner, this.#behavior);
    }
    if (info.authorTabindexChanges) for (const el of info.authorTabindexChanges) el.setAttribute(DatasetName.AUTHOR_TABINDEX, el.getAttribute("tabindex") ?? "none");
    this.#undecorateItems();
    this.#decorateItems();
  }
  /** @param {FocusGroupDefinition} [def] */
  #updateDefinition(def) {
    this.#behavior = def?.behavior ?? null;
    this.#wrap = def?.wrap ?? false;
    this.#axis = def?.axis;
    this.#memory = def?.memory ?? true;
    if (!this.#memory) this.#current = null;
  }
  #decorateItems() {
    if (!this.#behavior || this.#behavior === BehaviorToken.NONE) {
      this.#undecorateItems();
      return;
    }
    this.#items.decorate?.();
    for (const { element, segmentBoundary } of this.#items.items()) {
      this.#decorateItem?.(element, this.#behavior);
      element.setAttribute(DatasetName.AUTHOR_TABINDEX, element.getAttribute("tabindex") ?? "none");
      element.tabIndex = segmentBoundary ? 0 : -1;
    }
    if (!this.#current?.isConnected || !(this.#items.isItem?.(this.#current) ?? this.#items.contains(this.#current))) this.#current = null;
    const startItem = this.#current ?? this.#items.start ?? this.#items.first?.() ?? null;
    if (startItem) {
      startItem.tabIndex = 0;
      this.#start = startItem;
      this.#disableFocusabilityProxy();
      this.#enableFocusabilityProxy(startItem);
    }
    this.#items.flush?.();
  }
  #undecorateItems() {
    this.#disableFocusabilityProxy();
    let undecorated = false;
    for (const { element } of this.#items.items()) {
      undecorated = true;
      this.#decorateItem?.(element, null);
      const authorTabIndex = element.getAttribute(DatasetName.AUTHOR_TABINDEX);
      if (authorTabIndex) {
        if (authorTabIndex === "none") element.removeAttribute("tabindex");
        else element.setAttribute("tabindex", authorTabIndex);
        element.removeAttribute(DatasetName.AUTHOR_TABINDEX);
      }
    }
    this.#items.undecorate?.();
    if (undecorated) this.#items.flush?.();
  }
  /** @param {KeyboardEvent} evt */
  #handleKeydown(evt) {
    const current = evt.composedPath()[0];
    if (evt.defaultPrevented || current === this.#owner) return;
    if (!this.#items.contains(current)) return;
    let target;
    switch (getNavigationDirection(evt, current, this.#axis)) {
      case "start":
        target = this.#items.first();
        break;
      case "end":
        target = this.#items.last();
        break;
      case "forward":
        target = this.#items.next(current);
        if (!target && this.#wrap) target = this.#items.first();
        break;
      case "backward":
        target = this.#items.previous(current);
        if (!target && this.#wrap) target = this.#items.last();
        break;
    }
    if (target && target !== current) {
      this.#advanceFocus(current, target, true);
      this.#current = target;
      evt.preventDefault();
    }
  }
  /** @param {FocusEvent} evt */
  #handleFocusin(evt) {
    const target = evt.composedPath()[0];
    if (target === this.#owner && this.#ownerIsProxy && (!evt.relatedTarget || !nodeContains(this.#owner, evt.relatedTarget))) {
      const tabStop = this.#current || this.#start;
      this.#disableFocusabilityProxy();
      if (tabStop) tabStop.focus();
      evt.stopPropagation();
      return;
    }
    if (!this.#items.contains(target)) return;
    if (this.#ownerIsProxy) this.#disableFocusabilityProxy();
    const prev = this.#current;
    this.#current = target;
    if (prev === target) return;
    if (target.tabIndex < 0) {
      const transferFrom = prev ?? this.#start;
      if (transferFrom) this.#advanceFocus(transferFrom, target);
    }
  }
  /** @param {FocusEvent} evt */
  #handleFocusout(evt) {
    if (!evt.relatedTarget || !nodeContains(this.#owner, evt.relatedTarget)) {
      const tabStop = this.#memory ? this.#current || this.#start : this.#start;
      if (tabStop) this.#enableFocusabilityProxy(tabStop);
    }
    if (evt.relatedTarget && nodeContains(this.#owner, evt.relatedTarget) || this.#memory || !this.#start) return;
    const prev = this.#current;
    this.#current = null;
    const nextStart = this.#items.start ?? this.#items.first?.() ?? null;
    if (prev !== this.#start || nextStart !== this.#start) {
      for (const { element, segmentBoundary } of this.#items.items()) element.tabIndex = segmentBoundary ? 0 : -1;
      if (nextStart) {
        nextStart.tabIndex = 0;
        this.#start = nextStart;
      }
      this.#items.flush?.();
    }
  }
  /**
  * If the tab stop is inside a shadow DOM, sets `tabindex=0` on the
  * focusgroup owner so the browser's Tab navigation can land on it, at
  * which point `#handleFocusin` will redirect focus to the real tab stop.
  * @param {HTMLElement} tabStop - The actual focusable tab stop element.
  */
  #enableFocusabilityProxy(tabStop) {
    const rootNode = (tabStop.assignedSlot ?? tabStop).getRootNode();
    const hasFocusableHost = rootNode instanceof ShadowRoot && rootNode.host.hasAttribute(DatasetName.AUTHOR_TABINDEX);
    if (this.#ownerIsProxy || !hasFocusableHost) return;
    this.#ownerTabindexBeforeProxy = this.#owner.getAttribute("tabindex");
    this.#owner.tabIndex = 0;
    this.#ownerIsProxy = true;
    flushAllObservers();
  }
  /** Undoes `#enableFocusabilityProxy`. */
  #disableFocusabilityProxy() {
    if (!this.#ownerIsProxy) return;
    if (this.#ownerTabindexBeforeProxy !== null) this.#owner.setAttribute("tabindex", this.#ownerTabindexBeforeProxy);
    else this.#owner.removeAttribute("tabindex");
    this.#ownerIsProxy = false;
    this.#ownerTabindexBeforeProxy = null;
    this.#items.flush?.();
    flushAllObservers();
  }
  /**
  * Advances the focusgroup's active tab stop from one item to another. Sets
  * the target's `tabindex` to `0` and optionally calls `focus()` on it. The
  * previous item's `tabindex` is set to `-1` unless it belongs to a different
  * segment (in which case it remains `0` as a segment tab stop). Also disables
  * the owner proxy.
  *
  * @param {HTMLElement} prev - The currently focused item.
  * @param {HTMLElement} next - The item to receive focus.
  * @param {boolean} [shouldCallFocus=false] - Whether to programmatically
  *     call `focus()` on the target element.
  */
  #advanceFocus(prev, next, shouldCallFocus = false) {
    next.tabIndex = 0;
    if (shouldCallFocus) next.focus();
    prev.tabIndex = this.#items.sameSegment?.(prev, next) ?? true ? -1 : 0;
    this.#disableFocusabilityProxy();
    flushAllObservers();
  }
};

// @microsoft/focusgroup-polyfill/build/tree-walker-item-collection-shadowless.mjs
var TreeWalkerItemCollection = class {
  /**
  * Unique id used by `FocusGroup` to tag decorated items via the
  * `data-fg-item` attribute. Used here to disambiguate items of this group
  * from items of overlapping groups (possible via shadow-DOM slotting).
  * @type {string}
  */
  id = generateUniqueId();
  /**
  * First descendant with the `focusgroupstart` attribute (shadow-aware),
  * or the first item as fallback, or `null` if none exist. `FocusGroup`
  * reads this to choose the initial tab stop after decoration.
  * @returns {HTMLElement | null}
  */
  get start() {
    if (!this.#owner) return null;
    let first = null;
    for (const { element } of this.items()) {
      if (element.hasAttribute("focusgroupstart")) return element;
      first ??= element;
    }
    return first;
  }
  /** @type {HTMLElement} */
  #owner;
  /** @type {ShadowTreeWalker} */
  #walker;
  /** @type {MutationObserver | null} */
  #observer = null;
  /**
  * @param {HTMLElement!} owner - The focus group owner element.
  */
  constructor(owner) {
    this.#owner = owner;
    this.#walker = createTreeWalker(document, this.#owner, NodeFilter.SHOW_ELEMENT, (node) => this.#filter(node));
  }
  /**
  * Starts observing the owner subtree for mutations. Each relevant batch
  * is delivered to `focusGroup.update(info)`. Call this once, after the
  * paired `FocusGroup` has been constructed.
  *
  * @param {FocusGroup} focusGroup
  */
  observe(focusGroup) {
    this.#observer = createMutationObserver((records) => {
      const info = this.#classify(records);
      if (info) focusGroup.update(info);
    });
    this.#observer.observe(this.#owner, {
      attributes: true,
      attributeFilter: [
        "focusgroup",
        "focusgroupstart",
        "controls",
        "contenteditable",
        "disabled",
        "href",
        "hidden",
        "tabindex",
        "type"
      ],
      childList: true,
      subtree: true
    });
    observers.add(this.#observer);
  }
  /**
  * Releases the mutation observer and removes it from the global flush
  * registry. Called from `FocusGroup#disconnect()`; safe to call directly.
  */
  disconnect() {
    observers.delete(this.#observer);
    this.#observer?.disconnect();
    this.#owner = null;
    this.#walker = null;
    this.#observer = null;
  }
  /**
  * Flushes this collection's mutation observer by calling `takeRecords()`,
  * dropping any pending records (typically caused by polyfill-managed
  * attribute writes during decoration). Called by `FocusGroup` after writing
  * `tabindex`/`data-fg-*` to avoid re-entering `#handleItemsMutate`.
  */
  flush() {
    this.#observer?.takeRecords();
  }
  /**
  * Discovers items in the owner subtree and writes the marker attributes
  * (`data-fg-item`, `data-fg-seg`, `data-fg-segs`). Heavy walk — uses the
  * full candidacy filter (`isKeyboardFocusable` + ownership) and respects
  * nested-focusgroup opt-out subtrees and segmentor boundaries.
  *
  * After this call, `items()` will yield the marked items, and
  * `isItem()` / the persistent walker (used by `first/last/next/previous`)
  * will recognize them.
  */
  decorate() {
    const walker = createTreeWalker(document, this.#owner, NodeFilter.SHOW_ELEMENT, (node) => {
      if (this.#isItemCandidate(node) || this.#isNestedGroupOwner(node)) return NodeFilter.FILTER_ACCEPT;
      return NodeFilter.FILTER_SKIP;
    });
    let pendingSegmentBoundary = false;
    let segment = 0;
    let skipSubtreeOf = null;
    while (walker.nextNode()) {
      const node = walker.currentNode;
      if (skipSubtreeOf && nodeContains(skipSubtreeOf, node)) continue;
      skipSubtreeOf = null;
      if (this.#isNestedGroupOwner(node)) {
        if (isSegmentor(node, this.#owner)) pendingSegmentBoundary = true;
        if (node.getAttribute("focusgroup").includes(BehaviorToken.NONE)) {
          skipSubtreeOf = node;
          continue;
        }
        if (!isKeyboardFocusable(node, this.#owner)) continue;
      }
      node.setAttribute(DatasetName.ITEM, this.id);
      if (pendingSegmentBoundary) {
        segment++;
        node.setAttribute(DatasetName.SEGMENT, String(segment));
        node.setAttribute(DatasetName.SEGMENT_START, "");
        pendingSegmentBoundary = false;
      } else if (segment > 0) node.setAttribute(DatasetName.SEGMENT, String(segment));
    }
  }
  /**
  * Yields all items previously marked by `decorate()` in document order.
  * Light walk — iterates only `data-fg-item="${id}"` nodes via the
  * persistent walker (which rejects foreign-focusgroup subtrees). Reads
  * `segmentBoundary` from the DOM marker.
  *
  * @returns {Generator<FocusGroupItem>}
  */
  *items() {
    this.#walker.currentNode = this.#owner;
    while (this.#walker.nextNode()) {
      const node = this.#walker.currentNode;
      yield {
        element: node,
        segmentBoundary: node.hasAttribute(DatasetName.SEGMENT_START)
      };
    }
  }
  /**
  * Clears all marker attributes (`data-fg-item`, `data-fg-seg`,
  * `data-fg-segs`) written by `decorate()`. Light walk over marked nodes.
  */
  undecorate() {
    const marked = [];
    this.#walker.currentNode = this.#owner;
    while (this.#walker.nextNode()) marked.push(this.#walker.currentNode);
    for (const node of marked) {
      node.removeAttribute(DatasetName.ITEM);
      node.removeAttribute(DatasetName.SEGMENT);
      node.removeAttribute(DatasetName.SEGMENT_START);
    }
  }
  /**
  * Whether `el` is the first item of a non-initial segment (i.e. the item
  * that immediately follows a segmentor in document order).
  * @param {HTMLElement} el
  * @returns {boolean}
  */
  isSegmentStart(el) {
    return el.hasAttribute(DatasetName.SEGMENT_START);
  }
  /**
  * Whether `a` and `b` belong to the same segment.
  * @param {HTMLElement} a
  * @param {HTMLElement} b
  * @returns {boolean}
  */
  sameSegment(a, b) {
    return a.getAttribute(DatasetName.SEGMENT) === b.getAttribute(DatasetName.SEGMENT);
  }
  /** @returns {HTMLElement | null} The first item, or null. */
  first() {
    this.#walker.currentNode = this.#owner;
    return this.#walker.nextNode() ?? null;
  }
  /** @returns {HTMLElement | null} The last item, or null. */
  last() {
    this.#walker.currentNode = this.#owner;
    let last = null;
    while (this.#walker.nextNode()) last = this.#walker.currentNode;
    return last;
  }
  /**
  * @param {HTMLElement} current
  * @returns {HTMLElement | null}
  */
  next(current) {
    this.#walker.currentNode = current;
    return this.#walker.nextNode() ?? null;
  }
  /**
  * @param {HTMLElement} current
  * @returns {HTMLElement | null}
  */
  previous(current) {
    this.#walker.currentNode = current;
    return this.#walker.previousNode() ?? null;
  }
  /**
  * @param {Element} element
  * @returns {boolean} Whether `element` is currently an item of this group.
  */
  contains(element) {
    return this.#filter(element) !== NodeFilter.FILTER_REJECT;
  }
  /**
  * Strict membership: is `element` currently a decorated item of *this*
  * collection?
  * @param {Element} element
  * @returns {boolean}
  */
  isItem(element) {
    return element.getAttribute(DatasetName.ITEM) === this.id;
  }
  /**
  * The persistent walker's filter — accepts elements decorated as items of
  * this items collection (matching `id`) and rejects nested groups whose
  * decoration belongs to another collection.
  *
  * @param {Element} node
  * @returns {number}
  */
  #filter(node) {
    if (node.hasAttribute("focusgroup") && node.getAttribute(DatasetName.ITEM) !== this.id) return NodeFilter.FILTER_REJECT;
    const closestForeignGroup = getClosestElement(getParentElement(node), `[focusgroup]:not([${DatasetName.ITEM}="${this.id}"])`);
    if (closestForeignGroup && closestForeignGroup !== this.#owner) return NodeFilter.FILTER_REJECT;
    return this.isItem(node) ? NodeFilter.FILTER_ACCEPT : NodeFilter.FILTER_SKIP;
  }
  /**
  * @param {Element} node
  * @returns {boolean}
  */
  #isItemCandidate(node) {
    return isKeyboardFocusable(node, this.#owner) && getClosestElement(getParentElement(node), "[focusgroup]") === this.#owner;
  }
  /**
  * @param {Element} node
  * @returns {boolean}
  */
  #isNestedGroupOwner(node) {
    return node.hasAttribute("focusgroup") && node !== this.#owner;
  }
  /**
  * Translates a `MutationRecord` batch into a `FocusGroupUpdateInfo`,
  * or `null` if the batch contains nothing relevant. Filters out:
  *
  * - `tabindex` writes on items decorated by *other* focusgroups
  *   (identified by the presence of `AUTHOR_TABINDEX` and a non-matching
  *   `data-fg-item`), and
  * - `tabindex` writes on the owner element itself (caused by the paired
  *   `FocusGroup` toggling its owner-proxy tabindex).
  *
  * @param {MutationRecord[]} records
  * @returns {FocusGroupUpdateInfo | null}
  */
  #classify(records) {
    const relevant = records.filter((e) => !(e.type === "attributes" && e.attributeName === "tabindex" && (e.target.hasAttribute(DatasetName.AUTHOR_TABINDEX) && e.target.getAttribute(DatasetName.ITEM) !== this.id || e.target === this.#owner)));
    if (relevant.length === 0) return null;
    const definitionChanged = relevant.some((e) => e.target === this.#owner && e.attributeName === "focusgroup");
    const removedNodes = [];
    for (const e of relevant) if (e.type === "childList" && e.removedNodes.length > 0) removedNodes.push(...e.removedNodes);
    const authorTabindexChanges = [];
    for (const e of relevant) if (e.type === "attributes" && e.attributeName === "tabindex" && e.target.hasAttribute(DatasetName.AUTHOR_TABINDEX) && e.target.getAttribute(DatasetName.ITEM) === this.id) authorTabindexChanges.push(e.target);
    return {
      definition: definitionChanged ? parseDefinition(this.#owner) : void 0,
      removedNodes,
      authorTabindexChanges
    };
  }
};

// @microsoft/focusgroup-polyfill/build/polyfill-shadowless.mjs
var elementPolyfillMap;
if (hasDocument() && typeof MutationObserver !== "undefined" && !supportsFocusGroup()) {
  elementPolyfillMap = state.m ??= /* @__PURE__ */ new Map();
  if (!state.g) {
    const observer = createMutationObserver((entries) => {
      for (const entry of entries) {
        if (entry.type !== "childList") continue;
        for (const node of entry.removedNodes) if (elementPolyfillMap.has(node)) {
          elementPolyfillMap.get(node)?.disconnect();
          elementPolyfillMap.delete(node);
        }
        if (!state.b) continue;
        for (const node of entry.addedNodes) if (node.nodeType === Node.ELEMENT_NODE) polyfill(node);
      }
    });
    observer.observe(document.body, {
      childList: true,
      subtree: true
    });
    state.g = observer;
  }
}
function polyfill(root) {
  if (supportsFocusGroup() || !hasDocument()) return;
  root ??= document.body;
  const walker = createTreeWalker(document, root, NodeFilter.SHOW_ELEMENT, (node) => node.hasAttribute("focusgroup") ? NodeFilter.FILTER_ACCEPT : NodeFilter.FILTER_SKIP);
  do {
    const element = walker.currentNode;
    if (!element.hasAttribute?.("focusgroup") || elementPolyfillMap.has(element)) continue;
    elementPolyfillMap.set(element, null);
    requestAnimationFrame(() => {
      if (!elementPolyfillMap.has(element)) return;
      const items = new TreeWalkerItemCollection(element);
      const fg = new FocusGroup(element, items, {
        definition: parseDefinition(element),
        decorateOwner: (el, behavior) => inferRole(el, behavior, "owner"),
        decorateItem: (el, behavior) => inferRole(el, behavior, "child")
      });
      items.observe(fg);
      elementPolyfillMap.set(element, fg);
    });
  } while (walker.nextNode());
}
function polyfillBodyAndObserve() {
  if (!hasDocument()) return;
  state.b = true;
  polyfill();
}

// <stdin>
polyfillBodyAndObserve();
