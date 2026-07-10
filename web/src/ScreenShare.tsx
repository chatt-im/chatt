import { For, Show, createEffect, createMemo, createSignal, onCleanup } from "solid-js";
import Icon from "./Icon";
import type { ShareInfo } from "./types";
import { ScreenShareDecoder } from "./video-decode";

const MIN_ZOOM = 1;
const MAX_ZOOM = 8;

interface Size {
  width: number;
  height: number;
}

interface ViewState {
  zoom: number;
  panX: number;
  panY: number;
}

function clamp(value: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, value));
}

function initialSize(share: ShareInfo): Size {
  if (share.width > 0 && share.height > 0) {
    return { width: share.width, height: share.height };
  }
  return { width: 16, height: 9 };
}

function ScreenShareItem(props: {
  share: ShareInfo;
  playing: boolean;
  state: string;
  error?: string;
  fullscreen: boolean;
  onPlay: (streamId: number) => void;
  onStop: (streamId: number) => void;
  onToggleFullscreen: (streamId: number) => void;
  canvasRef: (streamId: number, el: HTMLCanvasElement) => void;
}) {
  const [videoSize, setVideoSize] = createSignal<Size>(initialSize(props.share));
  const [viewportSize, setViewportSize] = createSignal<Size>({ width: 0, height: 0 });
  const [view, setView] = createSignal<ViewState>({ zoom: 1, panX: 0, panY: 0 });
  const [dragging, setDragging] = createSignal(false);
  let viewState: ViewState = { zoom: 1, panX: 0, panY: 0 };

  let canvasEl: HTMLCanvasElement | undefined;
  let viewportEl: HTMLDivElement | undefined;
  let canvasObserver: MutationObserver | undefined;
  let viewportObserver: ResizeObserver | undefined;
  let styleFrame: number | undefined;
  let drag:
    | {
        pointerId: number;
        startX: number;
        startY: number;
        panX: number;
        panY: number;
      }
    | undefined;

  function fitScale(size = videoSize(), viewport = viewportSize()): number {
    if (size.width <= 0 || size.height <= 0 || viewport.width <= 0 || viewport.height <= 0) {
      return 1;
    }
    return Math.min(viewport.width / size.width, viewport.height / size.height);
  }

  function clampViewState(
    candidate: ViewState,
    size = videoSize(),
    viewport = viewportSize(),
  ): ViewState {
    const zoom = clamp(candidate.zoom, MIN_ZOOM, MAX_ZOOM);
    const scale = fitScale(size, viewport) * zoom;
    const maxX = Math.max(0, (size.width * scale - viewport.width) / 2);
    const maxY = Math.max(0, (size.height * scale - viewport.height) / 2);
    return {
      zoom,
      panX: clamp(candidate.panX, -maxX, maxX),
      panY: clamp(candidate.panY, -maxY, maxY),
    };
  }

  function applyCanvasStyle() {
    if (!canvasEl) return;
    const size = videoSize();
    const viewport = viewportSize();
    const scale = fitScale(size, viewport) * viewState.zoom;
    const offsetX = (viewport.width - size.width * scale) / 2 + viewState.panX;
    const offsetY = (viewport.height - size.height * scale) / 2 + viewState.panY;
    canvasEl.style.width = `${size.width}px`;
    canvasEl.style.height = `${size.height}px`;
    canvasEl.style.transform = `matrix(${scale}, 0, 0, ${scale}, ${offsetX}, ${offsetY})`;
  }

  function scheduleCanvasStyle() {
    if (styleFrame !== undefined) return;
    styleFrame = requestAnimationFrame(() => {
      styleFrame = undefined;
      applyCanvasStyle();
    });
  }

  function applyView(
    candidate: ViewState,
    publish = true,
    size = videoSize(),
    viewport = viewportSize(),
  ): ViewState {
    const next = clampViewState(candidate, size, viewport);
    viewState = next;
    if (publish) setView(next);
    scheduleCanvasStyle();
    return next;
  }

  function updateCanvasSize() {
    if (!canvasEl) return;
    if (canvasEl.width > 0 && canvasEl.height > 0) {
      const next = { width: canvasEl.width, height: canvasEl.height };
      setVideoSize(next);
      applyView(viewState, true, next);
    }
  }

  function updateViewportSize() {
    if (!viewportEl) return;
    const next = {
      width: viewportEl.clientWidth,
      height: viewportEl.clientHeight,
    };
    setViewportSize(next);
    applyView(viewState, true, videoSize(), next);
  }

  function setCanvasRef(el: HTMLCanvasElement) {
    canvasEl = el;
    props.canvasRef(props.share.stream_id, el);
    updateCanvasSize();
    canvasObserver?.disconnect();
    canvasObserver = new MutationObserver(updateCanvasSize);
    canvasObserver.observe(el, { attributes: true, attributeFilter: ["width", "height"] });
  }

  function setViewportRef(el: HTMLDivElement) {
    viewportEl = el;
    updateViewportSize();
    viewportObserver?.disconnect();
    viewportObserver = new ResizeObserver(updateViewportSize);
    viewportObserver.observe(el);
  }

  function zoomAt(clientX: number, clientY: number, factor: number) {
    if (!viewportEl) return;
    const rect = viewportEl.getBoundingClientRect();
    const cursorX = clientX - rect.left;
    const cursorY = clientY - rect.top;
    const size = videoSize();
    const viewport = viewportSize();
    const current = viewState;
    const oldScale = fitScale(size, viewport) * current.zoom;
    const oldOffsetX = (viewport.width - size.width * oldScale) / 2 + current.panX;
    const oldOffsetY = (viewport.height - size.height * oldScale) / 2 + current.panY;
    const contentX = (cursorX - oldOffsetX) / oldScale;
    const contentY = (cursorY - oldOffsetY) / oldScale;
    const zoom = clamp(current.zoom * factor, MIN_ZOOM, MAX_ZOOM);
    const newScale = fitScale(size, viewport) * zoom;
    const baseX = (viewport.width - size.width * newScale) / 2;
    const baseY = (viewport.height - size.height * newScale) / 2;
    applyView(
      {
        zoom,
        panX: cursorX - baseX - contentX * newScale,
        panY: cursorY - baseY - contentY * newScale,
      },
      true,
      size,
      viewport,
    );
  }

  function onWheel(event: WheelEvent) {
    if (!props.playing) return;
    event.preventDefault();
    const unit = event.deltaMode === WheelEvent.DOM_DELTA_LINE ? 16 : viewportSize().height;
    const delta = event.deltaMode === WheelEvent.DOM_DELTA_PIXEL ? event.deltaY : event.deltaY * unit;
    zoomAt(event.clientX, event.clientY, Math.exp(-delta * 0.001));
  }

  function onPointerDown(event: PointerEvent) {
    if (!props.playing || event.button !== 0) return;
    event.preventDefault();
    drag = {
      pointerId: event.pointerId,
      startX: event.clientX,
      startY: event.clientY,
      panX: viewState.panX,
      panY: viewState.panY,
    };
    setDragging(true);
    viewportEl?.setPointerCapture(event.pointerId);
  }

  function onPointerMove(event: PointerEvent) {
    if (!drag || drag.pointerId !== event.pointerId) return;
    event.preventDefault();
    applyView(
      {
        ...viewState,
        panX: drag.panX + event.clientX - drag.startX,
        panY: drag.panY + event.clientY - drag.startY,
      },
      false,
    );
  }

  function endDrag(event: PointerEvent) {
    if (!drag || drag.pointerId !== event.pointerId) return;
    viewportEl?.releasePointerCapture(event.pointerId);
    drag = undefined;
    setDragging(false);
    setView(viewState);
  }

  function onDoubleClick(event: MouseEvent) {
    if (!props.playing) return;
    event.preventDefault();
    zoomAt(event.clientX, event.clientY, event.shiftKey ? 0.5 : 2);
  }

  function resetView() {
    applyView({ zoom: 1, panX: 0, panY: 0 });
  }

  function onViewportKeyDown(event: KeyboardEvent) {
    if (!props.playing || !viewportEl) return;
    const step = event.shiftKey ? 120 : 40;
    if (event.key === "+" || event.key === "=") {
      const rect = viewportEl.getBoundingClientRect();
      zoomAt(rect.left + rect.width / 2, rect.top + rect.height / 2, 1.25);
    } else if (event.key === "-") {
      const rect = viewportEl.getBoundingClientRect();
      zoomAt(rect.left + rect.width / 2, rect.top + rect.height / 2, 0.8);
    } else if (event.key === "0" || event.key === "Home") {
      resetView();
    } else if (["ArrowLeft", "ArrowRight", "ArrowUp", "ArrowDown"].includes(event.key)) {
      applyView({
        ...viewState,
        panX: viewState.panX + (event.key === "ArrowLeft" ? step : event.key === "ArrowRight" ? -step : 0),
        panY: viewState.panY + (event.key === "ArrowUp" ? step : event.key === "ArrowDown" ? -step : 0),
      });
    } else {
      return;
    }
    event.preventDefault();
  }

  createEffect(() => {
    const next = initialSize(props.share);
    if (!canvasEl || canvasEl.width === 0 || canvasEl.height === 0) {
      setVideoSize(next);
      applyView(viewState, true, next);
    }
  });

  createEffect(() => {
    if (props.playing) {
      requestAnimationFrame(updateViewportSize);
    } else {
      resetView();
    }
  });

  onCleanup(() => {
    canvasObserver?.disconnect();
    viewportObserver?.disconnect();
    if (styleFrame !== undefined) cancelAnimationFrame(styleFrame);
  });

  return (
    <div class="screenshare-item" classList={{ "is-playing": props.playing }}>
      <div class="screenshare-row">
        <span class="screenshare-sender">{props.share.sender} is sharing a screen</span>
        <div class="screenshare-controls">
          <span class="screenshare-status" role="status" aria-live="polite">
            {props.state.replace(/-/g, " ")}
          </span>
          <Show
            when={props.playing}
            fallback={
              <button
                class="screenshare-button"
                type="button"
                aria-label="Play screen share"
                title="Play"
                disabled={!ScreenShareDecoder.supported()}
                onClick={() => props.onPlay(props.share.stream_id)}
              >
                <Icon name="play" />
              </button>
            }
          >
            <button
              class="screenshare-button"
              type="button"
              aria-label="Reset screen share view"
              title="Reset view"
              onClick={resetView}
            >
              <Icon name="rotate-ccw" />
            </button>
            <span class="screenshare-zoom">{view().zoom.toFixed(1)}x</span>
            <button
              class="screenshare-button"
              type="button"
              aria-label={props.fullscreen ? "Exit fullscreen" : "Enter fullscreen"}
              title={props.fullscreen ? "Exit fullscreen" : "Fullscreen"}
              onClick={() => props.onToggleFullscreen(props.share.stream_id)}
            >
              <Icon name={props.fullscreen ? "minimize-2" : "maximize-2"} />
            </button>
            <button
              class="screenshare-button"
              type="button"
              aria-label="Stop screen share"
              title="Stop"
              onClick={() => props.onStop(props.share.stream_id)}
            >
              <Icon name="square" />
            </button>
          </Show>
        </div>
      </div>
      <Show when={props.error}>
        <div class="screenshare-error">{props.error}</div>
      </Show>
      <div
        class="screenshare-viewport"
        classList={{ "is-dragging": dragging() }}
        ref={setViewportRef}
        onWheel={onWheel}
        onPointerDown={onPointerDown}
        onPointerMove={onPointerMove}
        onPointerUp={endDrag}
        onPointerCancel={endDrag}
        onDblClick={onDoubleClick}
        onKeyDown={onViewportKeyDown}
        role="region"
        aria-label={`Screen share from ${props.share.sender}. Use arrow keys to pan, plus and minus to zoom, and Home to reset.`}
        tabIndex={0}
      >
        <canvas class="screenshare-canvas" ref={setCanvasRef} />
      </div>
    </div>
  );
}

// Presents available screen shares with play/stop controls and a per-share
// canvas the decoder draws to. Decode and frame feeding live in App.
export default function ScreenShare(props: {
  shares: ShareInfo[];
  playing: number[];
  states: Record<number, string>;
  errors: Record<number, string>;
  fullscreenStream: number | null;
  onPlay: (streamId: number) => void;
  onStop: (streamId: number) => void;
  onToggleFullscreen: (streamId: number) => void;
  canvasRef: (streamId: number, el: HTMLCanvasElement) => void;
}) {
  const visibleShares = createMemo(() => {
    if (props.fullscreenStream === null) return props.shares;
    return props.shares.filter((share) => share.stream_id === props.fullscreenStream);
  });
  const isPlaying = (streamId: number) => props.playing.includes(streamId);
  return (
    <Show when={visibleShares().length > 0}>
      <div class="screenshare">
        <For each={visibleShares()}>
          {(share) => (
            <ScreenShareItem
              share={share}
              playing={isPlaying(share.stream_id)}
              state={props.states[share.stream_id] ?? "available"}
              error={props.errors[share.stream_id]}
              fullscreen={props.fullscreenStream === share.stream_id}
              onPlay={props.onPlay}
              onStop={props.onStop}
              onToggleFullscreen={props.onToggleFullscreen}
              canvasRef={props.canvasRef}
            />
          )}
        </For>
      </div>
    </Show>
  );
}
