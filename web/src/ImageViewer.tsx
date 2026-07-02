import {
  batch,
  createEffect,
  createMemo,
  createSignal,
  onCleanup,
  onMount,
  Show,
} from "solid-js";
import Icon from "./Icon";

type Point = { x: number; y: number };
type Size = { width: number; height: number };

const MANUAL_ZOOM_MIN = 0.1;
const MANUAL_ZOOM_MAX = 8;
const ZOOM_STEP = 0.25;
const KEY_PAN_STEP = 32;

function midpoint(a: Point, b: Point): Point {
  return { x: (a.x + b.x) / 2, y: (a.y + b.y) / 2 };
}

function distance(a: Point, b: Point): number {
  return Math.hypot(a.x - b.x, a.y - b.y);
}

function pointsEqual(a: Point, b: Point): boolean {
  return a.x === b.x && a.y === b.y;
}

export default function ImageViewer(props: {
  name: string;
  width: number | null;
  height: number | null;
}) {
  let viewportEl: HTMLDivElement | undefined;
  let gestureRect: DOMRect | undefined;
  let gestureFrame = 0;
  let resizeObserver: ResizeObserver | undefined;

  const initialWidth = props.width && props.width > 0 ? props.width : 0;
  const initialHeight = props.height && props.height > 0 ? props.height : 0;
  const [naturalSize, setNaturalSize] = createSignal<Size>({
    width: initialWidth,
    height: initialHeight,
  });
  const [viewportSize, setViewportSize] = createSignal<Size>({ width: 0, height: 0 });
  const [mode, setMode] = createSignal<"fit" | "manual">("fit");
  const [manualScale, setManualScale] = createSignal(1);
  const [pan, setPan] = createSignal<Point>({ x: 0, y: 0 });
  const [loaded, setLoaded] = createSignal(false);
  const [loadError, setLoadError] = createSignal(false);
  const [dragging, setDragging] = createSignal(false);

  const pointers = new Map<number, Point>();
  const renderedPointers = new Map<number, Point>();

  const fitScale = createMemo(() => {
    const image = naturalSize();
    const viewport = viewportSize();
    if (
      image.width <= 0 ||
      image.height <= 0 ||
      viewport.width <= 0 ||
      viewport.height <= 0
    ) {
      return 1;
    }
    return Math.min(viewport.width / image.width, viewport.height / image.height);
  });

  const scale = createMemo(() => (mode() === "fit" ? fitScale() : manualScale()));
  const minScale = () => Math.min(MANUAL_ZOOM_MIN, fitScale());
  const maxScale = () => Math.max(MANUAL_ZOOM_MAX, fitScale());
  const zoomPercent = () => Math.round(scale() * 100);

  function clampPan(next: Point, atScale = scale()): Point {
    const image = naturalSize();
    const viewport = viewportSize();
    const maxX = Math.max(0, (image.width * atScale - viewport.width) / 2);
    const maxY = Math.max(0, (image.height * atScale - viewport.height) / 2);
    return {
      x: Math.min(maxX, Math.max(-maxX, next.x)),
      y: Math.min(maxY, Math.max(-maxY, next.y)),
    };
  }

  const canPan = createMemo(() => {
    const image = naturalSize();
    const viewport = viewportSize();
    const currentScale = scale();
    return (
      image.width * currentScale > viewport.width + 0.5 ||
      image.height * currentScale > viewport.height + 0.5
    );
  });

  // A panel resize changes Fit's derived scale. Manual zoom retains its native
  // pixel percentage, but its pan is clamped to the newly visible bounds.
  createEffect(() => {
    viewportSize();
    naturalSize();
    const currentScale = scale();
    setPan((current) => {
      const next = mode() === "fit" ? { x: 0, y: 0 } : clampPan(current, currentScale);
      return pointsEqual(current, next) ? current : next;
    });
  });

  onMount(() => {
    if (!viewportEl) return;
    resizeObserver = new ResizeObserver(([entry]) => {
      if (!entry) return;
      setViewportSize({
        width: entry.contentRect.width,
        height: entry.contentRect.height,
      });
    });
    resizeObserver.observe(viewportEl);
  });

  onCleanup(() => {
    resizeObserver?.disconnect();
    if (gestureFrame) cancelAnimationFrame(gestureFrame);
    pointers.clear();
    renderedPointers.clear();
  });

  function resetFit() {
    batch(() => {
      setMode("fit");
      setPan({ x: 0, y: 0 });
    });
  }

  function resetNative() {
    batch(() => {
      setMode("manual");
      setManualScale(1);
      setPan({ x: 0, y: 0 });
    });
  }

  // Keep the source pixel under oldFocal beneath newFocal while zooming. For a
  // mouse wheel they are identical; separate points also account for a pinch
  // gesture whose midpoint moves while its distance changes.
  function zoomAt(targetScale: number, oldFocal: Point, newFocal = oldFocal) {
    const oldScale = scale();
    const nextScale = Math.min(maxScale(), Math.max(minScale(), targetScale));
    if (!Number.isFinite(nextScale) || oldScale <= 0) return;

    const viewport = viewportSize();
    const center = { x: viewport.width / 2, y: viewport.height / 2 };
    const currentPan = pan();
    const sourceOffset = {
      x: (oldFocal.x - center.x - currentPan.x) / oldScale,
      y: (oldFocal.y - center.y - currentPan.y) / oldScale,
    };
    const nextPan = clampPan(
      {
        x: newFocal.x - center.x - sourceOffset.x * nextScale,
        y: newFocal.y - center.y - sourceOffset.y * nextScale,
      },
      nextScale,
    );

    batch(() => {
      setMode("manual");
      setManualScale(nextScale);
      setPan(nextPan);
    });
  }

  function zoomFromCenter(delta: number) {
    const viewport = viewportSize();
    zoomAt(scale() + delta, { x: viewport.width / 2, y: viewport.height / 2 });
  }

  function panBy(dx: number, dy: number) {
    if (!canPan()) return;
    setPan((current) => clampPan({ x: current.x + dx, y: current.y + dy }));
  }

  function copyCurrentPointers() {
    renderedPointers.clear();
    for (const [id, point] of pointers) renderedPointers.set(id, point);
  }

  function flushGestureFrame() {
    if (gestureFrame) cancelAnimationFrame(gestureFrame);
    gestureFrame = 0;
    const active = [...pointers.entries()].slice(0, 2);

    if (active.length === 1) {
      const [id, point] = active[0]!;
      const previous = renderedPointers.get(id);
      if (previous) panBy(point.x - previous.x, point.y - previous.y);
    } else if (active.length === 2 && gestureRect) {
      const [[firstId, first], [secondId, second]] = active;
      const previousFirst = renderedPointers.get(firstId);
      const previousSecond = renderedPointers.get(secondId);
      if (previousFirst && previousSecond) {
        const oldDistance = distance(previousFirst, previousSecond);
        const nextDistance = distance(first, second);
        if (oldDistance > 0 && nextDistance > 0) {
          const oldCenter = midpoint(previousFirst, previousSecond);
          const nextCenter = midpoint(first, second);
          zoomAt(
            scale() * (nextDistance / oldDistance),
            {
              x: oldCenter.x - gestureRect.left,
              y: oldCenter.y - gestureRect.top,
            },
            {
              x: nextCenter.x - gestureRect.left,
              y: nextCenter.y - gestureRect.top,
            },
          );
        }
      }
    }

    copyCurrentPointers();
  }

  function scheduleGestureFrame() {
    if (gestureFrame) return;
    gestureFrame = requestAnimationFrame(flushGestureFrame);
  }

  function onPointerDown(event: PointerEvent) {
    if (event.pointerType === "mouse" && event.button !== 0) return;
    event.preventDefault();
    viewportEl?.focus({ preventScroll: true });
    if (pointers.size === 0) gestureRect = viewportEl?.getBoundingClientRect();
    const point = { x: event.clientX, y: event.clientY };
    pointers.set(event.pointerId, point);
    renderedPointers.set(event.pointerId, point);
    setDragging(true);
    try {
      viewportEl?.setPointerCapture(event.pointerId);
    } catch {
      // Capture can fail if the pointer ended between dispatch and this handler.
    }
  }

  function onPointerMove(event: PointerEvent) {
    if (!pointers.has(event.pointerId)) return;
    event.preventDefault();
    pointers.set(event.pointerId, { x: event.clientX, y: event.clientY });
    scheduleGestureFrame();
  }

  function finishPointer(event: PointerEvent) {
    if (!pointers.has(event.pointerId)) return;
    pointers.set(event.pointerId, { x: event.clientX, y: event.clientY });
    flushGestureFrame();
    pointers.delete(event.pointerId);
    renderedPointers.delete(event.pointerId);
    copyCurrentPointers();
    if (pointers.size === 0) {
      gestureRect = undefined;
      setDragging(false);
    }
  }

  function onWheel(event: WheelEvent) {
    event.preventDefault();
    if (!viewportEl) return;
    const rect = viewportEl.getBoundingClientRect();
    const unit =
      event.deltaMode === WheelEvent.DOM_DELTA_LINE
        ? 16
        : event.deltaMode === WheelEvent.DOM_DELTA_PAGE
          ? Math.max(1, viewportSize().height)
          : 1;
    const factor = Math.exp(-event.deltaY * unit * 0.002);
    zoomAt(scale() * factor, {
      x: event.clientX - rect.left,
      y: event.clientY - rect.top,
    });
  }

  function onViewportKeyDown(event: KeyboardEvent) {
    let dx = 0;
    let dy = 0;
    switch (event.key) {
      case "ArrowLeft":
        dx = KEY_PAN_STEP;
        break;
      case "ArrowRight":
        dx = -KEY_PAN_STEP;
        break;
      case "ArrowUp":
        dy = KEY_PAN_STEP;
        break;
      case "ArrowDown":
        dy = -KEY_PAN_STEP;
        break;
      default:
        return;
    }
    if (!canPan()) return;
    event.preventDefault();
    panBy(dx, dy);
  }

  return (
    <div class="image-viewer">
      <div
        class="image-viewer-toolbar"
        role="toolbar"
        aria-label="Image zoom controls"
      >
        <button
          type="button"
          aria-label="Fit image"
          title="Fit"
          onClick={resetFit}
        >
          <Icon name="maximize-2" />
        </button>
        <button
          class="image-viewer-text-button"
          type="button"
          title="Actual size"
          onClick={resetNative}
        >
          100%
        </button>
        <button
          type="button"
          aria-label="Zoom out"
          title="Zoom out"
          disabled={scale() <= minScale()}
          onClick={() => zoomFromCenter(-ZOOM_STEP)}
        >
          <Icon name="zoom-out" />
        </button>
        <output class="image-viewer-zoom">
          {zoomPercent()}%
        </output>
        <button
          type="button"
          aria-label="Zoom in"
          title="Zoom in"
          disabled={scale() >= maxScale()}
          onClick={() => zoomFromCenter(ZOOM_STEP)}
        >
          <Icon name="zoom-in" />
        </button>
      </div>
      <div
        class="image-viewer-viewport"
        classList={{
          "is-pannable": canPan(),
          "is-panning": dragging() && canPan(),
        }}
        ref={viewportEl}
        role="group"
        aria-label={`Image preview: ${props.name}`}
        tabIndex={0}
        onPointerDown={onPointerDown}
        onPointerMove={onPointerMove}
        onPointerUp={finishPointer}
        onPointerCancel={finishPointer}
        onWheel={onWheel}
        onKeyDown={onViewportKeyDown}
      >
        <img
          class="image-viewer-image"
          classList={{ "is-loaded": loaded() && !loadError() }}
          src={`/files/${encodeURIComponent(props.name)}`}
          alt={props.name}
          draggable={false}
          style={{
            width: `${naturalSize().width}px`,
            height: `${naturalSize().height}px`,
            transform: `translate(-50%, -50%) translate3d(${pan().x}px, ${pan().y}px, 0) scale(${scale()})`,
          }}
          onLoad={(event) => {
            const image = event.currentTarget;
            batch(() => {
              setNaturalSize({
                width: image.naturalWidth,
                height: image.naturalHeight,
              });
              setLoaded(true);
              setLoadError(false);
            });
          }}
          onError={() => {
            setLoaded(true);
            setLoadError(true);
          }}
        />
        <Show when={!loaded()}>
          <div class="image-viewer-status">loading…</div>
        </Show>
        <Show when={loadError()}>
          <div class="image-viewer-status">failed to load image</div>
        </Show>
      </div>
    </div>
  );
}
