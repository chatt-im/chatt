import {
  initCache,
  getItemSize as _getItemSize,
  getItemOffset as _getItemOffset,
  UNCACHED,
  setItemSize,
  estimateDefaultItemSize,
  updateCacheLength,
  computeRange,
  takeCacheSnapshot,
  findIndex,
} from "./cache.js";
import { isIOSWebKit } from "./environment.js";
import type {
  CacheSnapshot,
  InternalCacheSnapshot,
  ItemResize,
  ItemsRange,
} from "./types.js";
import { abs, max, min, NULL } from "./utils.js";
import { appendDebugLog, debugFlagEnabled } from "../../../debug-log";

const MAX_INT_32 = 0x7fffffff;

const SCROLL_IDLE = 0;
const SCROLL_DOWN = 1;
const SCROLL_UP = 2;
type ScrollDirection =
  | typeof SCROLL_IDLE
  | typeof SCROLL_DOWN
  | typeof SCROLL_UP;

const SCROLL_BY_NATIVE = 0;
const SCROLL_BY_MANUAL_SCROLL = 1;
const SCROLL_BY_SHIFT = 2;
type ScrollMode =
  | typeof SCROLL_BY_NATIVE
  | typeof SCROLL_BY_MANUAL_SCROLL
  | typeof SCROLL_BY_SHIFT;

const VIRTUA_DEBUG_TOP_THRESHOLD = 600;

const scrollDirectionName = (direction: ScrollDirection): string => {
  switch (direction) {
    case SCROLL_DOWN:
      return "down";
    case SCROLL_UP:
      return "up";
    default:
      return "idle";
  }
};

const scrollModeName = (mode: ScrollMode): string => {
  switch (mode) {
    case SCROLL_BY_MANUAL_SCROLL:
      return "manual";
    case SCROLL_BY_SHIFT:
      return "shift";
    default:
      return "native";
  }
};

const virtuaDebugEnabled = (): boolean => {
  return (
    debugFlagEnabled("debugScroll", "chatt.debugScroll") ||
    debugFlagEnabled("debugVirtua", "chatt.debugVirtua")
  );
};

const debugVirtua = (
  stage: string,
  fields: Record<string, unknown> = {},
): void => {
  if (!virtuaDebugEnabled()) return;
  appendDebugLog("virtua", stage, fields);
};

/** @internal */
export const ACTION_SCROLL = 1;
/** @internal */
export const ACTION_SCROLL_END = 2;
/** @internal */
export const ACTION_ITEM_RESIZE = 3;
/** @internal */
export const ACTION_VIEWPORT_RESIZE = 4;
/** @internal */
export const ACTION_ITEMS_LENGTH_CHANGE = 5;
/** @internal */
export const ACTION_START_OFFSET_CHANGE = 6;
/** @internal */
export const ACTION_END_OFFSET_CHANGE = 7;
/** @internal */
export const ACTION_MANUAL_SCROLL = 8;
/** @internal */
export const ACTION_BEFORE_MANUAL_SMOOTH_SCROLL = 9;

type Actions =
  | [type: typeof ACTION_SCROLL, offset: number]
  | [type: typeof ACTION_SCROLL_END, dummy?: void]
  | [type: typeof ACTION_ITEM_RESIZE, entries: ItemResize[]]
  | [type: typeof ACTION_VIEWPORT_RESIZE, size: number]
  | [
      type: typeof ACTION_ITEMS_LENGTH_CHANGE,
      arg: [
        length: number,
        isShift?: boolean | undefined,
        itemSizes?: readonly number[] | undefined,
      ],
    ]
  | [type: typeof ACTION_START_OFFSET_CHANGE, offset: number]
  | [type: typeof ACTION_END_OFFSET_CHANGE, offset: number]
  | [type: typeof ACTION_MANUAL_SCROLL, dummy?: void]
  | [type: typeof ACTION_BEFORE_MANUAL_SMOOTH_SCROLL, offset: number];

/** @internal */
export const UPDATE_VIRTUAL_STATE = 0b0001;
/** @internal */
export const UPDATE_SIZE_EVENT = 0b0010;
/** @internal */
export const UPDATE_SCROLL_EVENT = 0b0100;
/** @internal */
export const UPDATE_SCROLL_END_EVENT = 0b1000;

/**
 * @internal
 */
export const getScrollSize = (store: VirtualStore): number => {
  return max(store.$getTotalSize(), store.$getViewportSize());
};

type Subscriber = (sync?: boolean) => void;

/** @internal */
export type StateVersion =
  number & {} /* hack for typescript to pretend as not falsy */;

/**
 * @internal
 */
export type VirtualStore = {
  $dispose(): void;
  $getStateVersion(): StateVersion;
  $getCacheSnapshot(): CacheSnapshot;
  $getRange(bufferSize?: number): ItemsRange;
  $findItemIndex(offset: number): number;
  $isUnmeasuredItem(index: number): boolean;
  $getItemOffset(index: number, fromEnd?: boolean): number;
  $getItemSize(index: number): number;
  $getItemsLength(): number;
  $getScrollOffset(): number;
  $isScrolling(): boolean;
  $getViewportSize(): number;
  $getStartSpacerSize(): number;
  $getEndSpacerSize(): number;
  $getTotalSize(): number;
  _flushJump(): [number, boolean];
  $subscribe(target: number, cb: Subscriber): () => void;
  $update(...action: Actions): void;
};

/**
 * @internal
 */
export const createVirtualStore = (
  elementsCount: number,
  itemSize: number = 40,
  ssrCount: number = 0,
  cacheSnapshot?: CacheSnapshot | undefined,
  shouldAutoEstimateItemSize: boolean = false,
  itemSizes?: readonly number[] | undefined,
): VirtualStore => {
  let isSSR = !!ssrCount;
  let stateVersion: StateVersion = 1;
  let viewportSize = 0;
  let startSpacerSize = 0;
  let endSpacerSize = 0;
  let scrollOffset = 0;
  let jump = 0;
  let pendingJump = 0;
  let _flushedJump = 0;
  let _scrollDirection: ScrollDirection = SCROLL_IDLE;
  let _scrollMode: ScrollMode = SCROLL_BY_NATIVE;
  let resetShiftOnScroll = false;
  let _frozenRange: ItemsRange | null = NULL;
  let _prevRange: ItemsRange = [0, isSSR ? max(ssrCount - 1, 0) : -1];
  let _totalMeasuredSize = 0;
  let _isViewportMeasured = false;

  const cache = initCache(
    elementsCount,
    cacheSnapshot
      ? (cacheSnapshot as unknown as InternalCacheSnapshot)[1]
      : itemSize,
    cacheSnapshot
      ? (cacheSnapshot as unknown as InternalCacheSnapshot)[0]
      : itemSizes,
  );
  const subscribers = new Set<[number, Subscriber]>();
  const getRelativeScrollOffset = () => scrollOffset - startSpacerSize;
  const getVisibleOffset = () => getRelativeScrollOffset() + pendingJump + jump;
  const getRange = (startOffset: number, endOffset: number) => {
    return computeRange(cache, startOffset, endOffset, _prevRange[0]);
  };
  const getItemsSize = (): number => _getItemOffset(cache, cache._length);
  const getTotalSize = (): number => getItemsSize() + endSpacerSize;
  const getItemOffset = (index: number, fromEnd?: boolean): number => {
    const offset = _getItemOffset(cache, index) - pendingJump;
    if (fromEnd) {
      return getItemsSize() - offset - getItemSize(index);
    }
    return offset;
  };
  const getItemSize = (index: number): number => {
    return _getItemSize(cache, index);
  };
  const isSizeEqual = (index: number, value: number = UNCACHED): boolean => {
    return cache._sizes[index] === value;
  };

  const applyJump = (j: number) => {
    if (j) {
      const deferred =
        (isIOSWebKit() && _scrollDirection !== SCROLL_IDLE) ||
        (_frozenRange && _scrollMode === SCROLL_BY_MANUAL_SCROLL);
      if (deferred) {
        pendingJump += j;
      } else {
        jump += j;
      }
      debugVirtua("apply-jump", {
        amount: j,
        deferred,
        pendingJump,
        jump,
        scrollOffset,
        viewportSize,
        totalSize: getTotalSize(),
        direction: scrollDirectionName(_scrollDirection),
        mode: scrollModeName(_scrollMode),
      });
    }
  };

  return {
    $dispose: () => {
      subscribers.clear();
    },
    $getStateVersion: () => stateVersion,
    $getCacheSnapshot: () => {
      return takeCacheSnapshot(cache) as unknown as CacheSnapshot;
    },
    $getRange: (bufferSize = 200) => {
      if (!_isViewportMeasured || isSSR) {
        // Return range for SSR, or return [0, -1] to render nothing, until the scroll offset and viewport size are determined.
        // https://github.com/inokawa/virtua/issues/415
        // https://github.com/inokawa/virtua/pull/818
        return _prevRange;
      }
      let startIndex: number;
      let endIndex: number;
      if (_flushedJump) {
        // Return previous range for consistent render until next scroll event comes in.
        // And it must be clamped. https://github.com/inokawa/virtua/issues/597
        [startIndex, endIndex] = _prevRange;
      } else {
        let startOffset = max(0, getVisibleOffset());
        let endOffset = startOffset + viewportSize;

        // For faster initial render pass, returns without buffer if measurement seems to be in progress.
        if (!shouldAutoEstimateItemSize) {
          bufferSize = max(0, bufferSize);

          if (_scrollDirection !== SCROLL_DOWN) {
            startOffset -= bufferSize;
          }
          if (_scrollDirection !== SCROLL_UP) {
            endOffset += bufferSize;
          }
        }

        [startIndex, endIndex] = _prevRange = getRange(
          max(0, startOffset),
          max(0, endOffset),
        );
        if (_frozenRange) {
          startIndex = min(startIndex, _frozenRange[0]);
          endIndex = max(endIndex, _frozenRange[1]);
        }
      }

      return [max(startIndex, 0), min(endIndex, cache._length - 1)];
    },
    $findItemIndex: (offset) => findIndex(cache, offset - startSpacerSize),
    $isUnmeasuredItem: isSizeEqual,
    $getItemOffset: getItemOffset,
    $getItemSize: getItemSize,
    $getItemsLength: () => cache._length,
    $getScrollOffset: () => scrollOffset,
    $isScrolling: () => _scrollDirection !== SCROLL_IDLE,
    $getViewportSize: () => viewportSize,
    $getStartSpacerSize: () => startSpacerSize,
    $getEndSpacerSize: () => endSpacerSize,
    $getTotalSize: getTotalSize,
    _flushJump: () => {
      _flushedJump = jump;
      jump = 0;
      return [_flushedJump, _scrollMode === SCROLL_BY_SHIFT];
    },
    $subscribe: (target, cb) => {
      const sub: [number, Subscriber] = [target, cb];
      subscribers.add(sub);
      return () => {
        subscribers.delete(sub);
      };
    },
    $update: (type, payload): void => {
      let shouldFlushPendingJump: boolean | undefined;
      let shouldSync: boolean | undefined;
      let mutated = 0;

      switch (type) {
        case ACTION_SCROLL: {
          if (payload === scrollOffset && _scrollMode === SCROLL_BY_NATIVE) {
            // Ignore scroll events from different direction
            break;
          }

          const flushedJump = _flushedJump;
          _flushedJump = 0;
          const shouldResetShift =
            resetShiftOnScroll && _scrollMode === SCROLL_BY_SHIFT;

          const delta = payload - scrollOffset;
          const distance = abs(delta);

          // Scroll event after jump compensation is not reliable because it may result in the opposite direction.
          // The delta of artificial scroll may not be equal with the jump because it may be batched with other scrolls.
          // And at least in latest Chrome/Firefox/Safari in 2023, setting value to scrollTop/scrollLeft can lose subpixel because its integer (sometimes float probably depending on dpr).
          const isJustJumped = flushedJump && distance < abs(flushedJump) + 1;

          // Scroll events are dispatched enough so it's ok to skip some of them.
          if (
            !isJustJumped &&
            // Ignore until manual scrolling
            _scrollMode === SCROLL_BY_NATIVE
          ) {
            _scrollDirection = delta < 0 ? SCROLL_UP : SCROLL_DOWN;
          }

          // TODO This will cause glitch in reverse infinite scrolling. Disable this until better solution is found.
          // if (
          //   pendingJump &&
          //   ((_scrollDirection === SCROLL_UP &&
          //     payload - max(pendingJump, 0) <= 0) ||
          //     (_scrollDirection === SCROLL_DOWN &&
          //       payload - min(pendingJump, 0) >= getScrollOffsetMax()))
          // ) {
          //   // Flush if almost reached to start or end
          //   shouldFlushPendingJump = true;
          // }

          if (isSSR) {
            isSSR = false;
          }

          scrollOffset = payload;
          mutated = UPDATE_SCROLL_EVENT;

          // Skip if offset is not changed
          // Scroll offset may exceed min or max especially in Safari's elastic scrolling.
          const relativeOffset = getRelativeScrollOffset();
          if (
            relativeOffset >= -viewportSize &&
            relativeOffset <= getTotalSize()
          ) {
            mutated += UPDATE_VIRTUAL_STATE;

            // Update synchronously if scrolled a lot
            shouldSync = distance > viewportSize;
          }
          if (
            virtuaDebugEnabled() &&
            (payload < VIRTUA_DEBUG_TOP_THRESHOLD ||
              distance > viewportSize / 2 ||
              !!flushedJump)
          ) {
            debugVirtua("scroll", {
              payload,
              previous: payload - delta,
              delta,
              distance,
              flushedJump,
              isJustJumped: !!isJustJumped,
              relativeOffset,
              viewportSize,
              totalSize: getTotalSize(),
              direction: scrollDirectionName(_scrollDirection),
              mode: scrollModeName(_scrollMode),
              resetShiftOnScroll: shouldResetShift,
              pendingJump,
              jump,
              shouldSync: !!shouldSync,
            });
          }
          if (shouldResetShift) {
            // Local fix for reverse infinite chat history: shift mode is needed
            // for the prepend jump itself, but keeping it for the whole smooth
            // wheel gesture makes each newly measured row fight the user's
            // scroll with another distance-from-end correction.
            resetShiftOnScroll = false;
            _scrollMode = SCROLL_BY_NATIVE;
            debugVirtua("shift-scroll-applied", {
              payload,
              scrollOffset,
              viewportSize,
              totalSize: getTotalSize(),
            });
          }
          break;
        }
        case ACTION_SCROLL_END: {
          mutated = UPDATE_SCROLL_END_EVENT;
          if (_scrollDirection !== SCROLL_IDLE) {
            shouldFlushPendingJump = true;
            mutated += UPDATE_VIRTUAL_STATE;
          }
          debugVirtua("scroll-end", {
            direction: scrollDirectionName(_scrollDirection),
            mode: scrollModeName(_scrollMode),
            pendingJump,
            jump,
            shouldFlushPendingJump: !!shouldFlushPendingJump,
            scrollOffset,
            viewportSize,
            totalSize: getTotalSize(),
          });
          _scrollDirection = SCROLL_IDLE;
          _scrollMode = SCROLL_BY_NATIVE;
          resetShiftOnScroll = false;
          _frozenRange = NULL;
          break;
        }
        case ACTION_ITEM_RESIZE: {
          const updated = payload.filter(
            ([index, size]) => !isSizeEqual(index, size),
          );

          // Skip if all items are cached and not updated
          if (!updated.length) {
            break;
          }

          let minIndex = updated[0]![0];
          let maxIndex = minIndex;
          const resizeJump = updated.reduce((acc, [index, size]) => {
            minIndex = min(minIndex, index);
            maxIndex = max(maxIndex, index);
            let shouldKeep: boolean;
            if (
              // Keep distance from end during shifting
              _scrollMode === SCROLL_BY_SHIFT
            ) {
              shouldKeep = true;
            } else if (
              _frozenRange &&
              _scrollMode === SCROLL_BY_MANUAL_SCROLL
            ) {
              // https://github.com/inokawa/virtua/issues/380
              // https://github.com/inokawa/virtua/issues/758
              shouldKeep = index < _frozenRange[0];
            } else {
              // Otherwise we should maintain visible position
              const start = getRelativeScrollOffset();
              const itemOffset = getItemOffset(index);
              const itemSize = getItemSize(index);
              shouldKeep =
                _scrollDirection !== SCROLL_DOWN &&
                _scrollMode === SCROLL_BY_NATIVE
                  ? // https://github.com/inokawa/virtua/issues/385
                    // https://github.com/inokawa/virtua/discussions/865
                    itemOffset + itemSize < start
                  : // https://github.com/inokawa/virtua/pull/868
                    itemOffset < start &&
                    itemOffset + itemSize < start + viewportSize;
            }

            if (shouldKeep) {
              acc += size - getItemSize(index);
            }
            return acc;
          }, 0);
          debugVirtua("item-resize", {
            count: updated.length,
            minIndex,
            maxIndex,
            resizeJump,
            scrollOffset,
            viewportSize,
            totalSize: getTotalSize(),
            direction: scrollDirectionName(_scrollDirection),
            mode: scrollModeName(_scrollMode),
          });
          // Calculate jump by resize to minimize junks in appearance
          applyJump(resizeJump);

          // Update item sizes
          for (const [index, size] of updated) {
            const prevSize = getItemSize(index);
            const isInitialMeasurement = setItemSize(cache, index, size);

            if (shouldAutoEstimateItemSize) {
              _totalMeasuredSize += isInitialMeasurement
                ? size
                : size - prevSize;
            }
          }

          // Estimate initial item size from measured sizes
          if (
            shouldAutoEstimateItemSize &&
            viewportSize &&
            // If the total size is lower than the viewport, the item may be a empty state
            _totalMeasuredSize > viewportSize
          ) {
            const estimateJump = estimateDefaultItemSize(
              cache,
              findIndex(cache, getVisibleOffset()),
            );
            debugVirtua("estimate-item-size", {
              estimateJump,
              defaultItemSize: cache._defaultItemSize,
              scrollOffset,
              viewportSize,
              totalSize: getTotalSize(),
            });
            applyJump(estimateJump);
            shouldAutoEstimateItemSize = false;
          }

          mutated = UPDATE_VIRTUAL_STATE + UPDATE_SIZE_EVENT;

          // Synchronous update is necessary in current design to minimize visible glitch in concurrent rendering.
          // However this seems to be the main cause of the errors from ResizeObserver.
          // https://github.com/inokawa/virtua/issues/470
          //
          // And in React, synchronous update with flushSync after asynchronous update will overtake the asynchronous one.
          // If items resize happens just after scroll, race condition can occur depending on implementation.
          shouldSync = true;
          break;
        }
        case ACTION_VIEWPORT_RESIZE: {
          if (viewportSize !== payload) {
            if (!viewportSize) {
              _isViewportMeasured = shouldSync = true;
            }
            viewportSize = payload;
            mutated = UPDATE_VIRTUAL_STATE + UPDATE_SIZE_EVENT;
          }
          break;
        }
        case ACTION_ITEMS_LENGTH_CHANGE: {
          const previousLength = cache._length;
          if (payload[1]) {
            const lengthJump = updateCacheLength(
              cache,
              payload[0],
              true,
              payload[2],
            );
            debugVirtua("items-length-change", {
              previousLength,
              nextLength: payload[0],
              shift: true,
              lengthJump,
              hasItemSizes: !!payload[2],
              scrollOffset,
              viewportSize,
              totalSize: getTotalSize(),
              defaultItemSize: cache._defaultItemSize,
            });
            applyJump(lengthJump);
            _scrollMode = lengthJump ? SCROLL_BY_SHIFT : SCROLL_BY_NATIVE;
            resetShiftOnScroll = !!lengthJump;
            mutated = UPDATE_VIRTUAL_STATE;
          } else {
            updateCacheLength(cache, payload[0], false, payload[2]);
            debugVirtua("items-length-change", {
              previousLength,
              nextLength: payload[0],
              shift: false,
              hasItemSizes: !!payload[2],
              scrollOffset,
              viewportSize,
              totalSize: getTotalSize(),
              defaultItemSize: cache._defaultItemSize,
            });
            // https://github.com/inokawa/virtua/issues/552
            // https://github.com/inokawa/virtua/issues/557
            mutated = UPDATE_VIRTUAL_STATE;
          }
          break;
        }
        case ACTION_START_OFFSET_CHANGE: {
          if (startSpacerSize !== payload) {
            startSpacerSize = payload;
            mutated = UPDATE_VIRTUAL_STATE + UPDATE_SIZE_EVENT;
          }
          break;
        }
        case ACTION_END_OFFSET_CHANGE: {
          if (endSpacerSize !== payload) {
            endSpacerSize = payload;
            mutated = UPDATE_VIRTUAL_STATE + UPDATE_SIZE_EVENT;
          }
          break;
        }
        case ACTION_MANUAL_SCROLL: {
          _scrollMode = SCROLL_BY_MANUAL_SCROLL;
          break;
        }
        case ACTION_BEFORE_MANUAL_SMOOTH_SCROLL: {
          _frozenRange = getRange(payload, payload + viewportSize);
          mutated = UPDATE_VIRTUAL_STATE;
          break;
        }
      }

      if (mutated) {
        stateVersion = (stateVersion & MAX_INT_32) + 1;

        if (shouldFlushPendingJump && pendingJump) {
          jump += pendingJump;
          pendingJump = 0;
        }

        subscribers.forEach(([target, cb]) => {
          // Early return to skip React's computation
          if (!(mutated & target)) {
            return;
          }
          // https://github.com/facebook/react/issues/25191
          // https://github.com/facebook/react/blob/a5fc797db14c6e05d4d5c4dbb22a0dd70d41f5d5/packages/react-reconciler/src/ReactFiberWorkLoop.js#L1443-L1447
          cb(shouldSync);
        });
      }
    },
  };
};
