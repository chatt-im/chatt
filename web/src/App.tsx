import { createSignal, createEffect, onCleanup, onMount, For, Show } from "solid-js";
import { Virtualizer, type VirtualizerHandle } from "./vendor/virtua/solid/Virtualizer";
import type { WebMessage, ServerEnvelope, ClientRequest, ShareInfo, Fragment } from "./types";
import ScreenShare from "./ScreenShare";
import { ScreenShareDecoder, parseFrame } from "./video-decode";
import { renderMarkdown } from "./markdown";
import { renderInline } from "./highlight";
import { decodeFeed } from "./feed";
import FileViewer from "./FileViewer";

// Pixel tolerance when deciding the view is "at the bottom". Scroll positions
// are fractional, so an exact comparison would intermittently read as
// not-at-bottom right after a programmatic scroll.
const BOTTOM_EPSILON = 4;

// Distance from the top, in pixels, at which scrolling up requests older history.
const TOP_THRESHOLD = 200;

// How many older messages one paging request asks for.
const PAGE = 100;

// Size of each file-upload chunk frame. Kept well under the server's WebSocket
// payload cap so a browser never has to fragment a single frame.
const UPLOAD_CHUNK_BYTES = 256 * 1024;

// Preload a bounded number of image attachments from each message batch. The
// virtualizer may defer mounting rows while it measures and pins the bottom, but
// attachment URLs are known as soon as the WebSocket message arrives.
const IMAGE_PRELOAD_BATCH_LIMIT = 32;
const IMAGE_PRELOAD_CACHE_LIMIT = 128;
const RECENT_IMAGE_KEEP_MOUNTED = 12;

// Consecutive messages from one sender within this window collapse into a group:
// only the first carries the sender/time header (Discord-style).
const GROUP_WINDOW_MS = 5 * 60 * 1000;

// Do not flash a connection error while the initial WebSocket handshake (or a
// quick reconnect) is still in progress.
const CONNECTION_ERROR_DELAY_MS = 3_000;

type ImagePreload = {
  image: HTMLImageElement;
  link: HTMLLinkElement;
};

const DEFAULT_SHARE_PANE_HEIGHT = 360;
const MIN_SHARE_PANE_HEIGHT = 160;
const MIN_CHAT_PANE_HEIGHT = 140;
const DIVIDER_SIZE = 9;
const PANE_KEY_STEP = 32;
const DEFAULT_FILE_PANEL_WIDTH = 560;
const MIN_FILE_PANEL_WIDTH = 320;
const MIN_CHAT_SPLIT_WIDTH = 320;
const FILE_PANEL_DIVIDER_SIZE = 3;
const FILE_PANEL_KEY_STEP = 32;

// Builds the asset URL for an attachment served from the client's receive dir.
function fileUrl(name: string): string {
  return `/files/${encodeURIComponent(name)}`;
}

function imageDebugEnabled(): boolean {
  if (typeof location === "undefined") return false;
  if (new URLSearchParams(location.search).has("debugImages")) return true;
  try {
    return localStorage.getItem("chatt.debugImages") === "1";
  } catch {
    return false;
  }
}

function debugImageTiming(stage: string, name: string, url: string) {
  if (!imageDebugEnabled() || typeof performance === "undefined") return;
  const href = new URL(url, location.href).href;
  const entries = performance.getEntriesByName(href);
  const timing = entries[entries.length - 1] as PerformanceResourceTiming | undefined;
  console.debug("[chatt:image]", {
    stage,
    name,
    t: Math.round(performance.now() * 10) / 10,
    startTime: timing?.startTime,
    requestStart: timing?.requestStart,
    responseEnd: timing?.responseEnd,
    url: href,
  });
}

function clamp(value: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, value));
}

function formatTime(ms: number): string {
  if (!ms) return "";
  const d = new Date(ms);
  return d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}

function formatBytes(bytes: number): string {
  const KIB = 1024;
  const MIB = 1024 * KIB;
  if (bytes >= MIB) return `${(bytes / MIB).toFixed(1)} MiB`;
  if (bytes >= KIB) return `${(bytes / KIB).toFixed(1)} KiB`;
  return `${bytes} B`;
}

// Progress bar shown on a file's placeholder message while the host client
// pulls the file off the relay. Replaced by the attachment on completion.
function TransferProgressBar(props: { progress: { transferred: number; total: number } }) {
  const ratio = () => {
    const { transferred, total } = props.progress;
    return total > 0 ? Math.min(1, transferred / total) : 0;
  };
  const pct = () => Math.round(ratio() * 100);
  return (
    <div
      class="message-progress"
      role="progressbar"
      aria-valuenow={pct()}
      aria-valuemin={0}
      aria-valuemax={100}
    >
      <div class="message-progress-track">
        <div class="message-progress-fill" style={{ width: `${pct()}%` }} />
      </div>
      <span class="message-progress-label">
        receiving {formatBytes(props.progress.transferred)} / {formatBytes(props.progress.total)} (
        {pct()}%)
      </span>
    </div>
  );
}

// The virtualizer unmounts rows that scroll out of the window, so a fragment's
// body would otherwise be re-parsed every time its row scrolls back in. A
// fragment object is created once per decoded message and never mutated
// (progress merges keep the same fragments array), so it is a stable cache
// key; replaced messages simply fall out with GC.
const fragmentHtmlCache = new WeakMap<Fragment, string>();

function fragmentHtml(fragment: Fragment): string {
  let html = fragmentHtmlCache.get(fragment);
  if (html === undefined) {
    html =
      fragment.kind === "text"
        ? renderMarkdown(fragment.text)
        : renderInline(fragment.text, fragment.spans);
    fragmentHtmlCache.set(fragment, html);
  }
  return html;
}

// Renders a message body from its fragments: prose as markdown, code blocks
// from their precomputed highlight spans. Nothing is re-highlighted here.
function MessageBody(props: { fragments: Fragment[] }) {
  return (
    <For each={props.fragments}>
      {(fragment) =>
        fragment.kind === "text" ? (
          <div class="message-body" innerHTML={fragmentHtml(fragment)} />
        ) : (
          <pre class="code-block">
            <code innerHTML={fragmentHtml(fragment)} />
          </pre>
        )
      }
    </For>
  );
}

function Attachment(props: { message: WebMessage; onOpenFile: (name: string) => void }) {
  const att = () => props.message.attachment!;
  const url = () => fileUrl(att().name);
  // Fades the image in on decode instead of snapping. The box is already
  // reserved by width/height, so this only affects the pixels, never layout.
  const [loaded, setLoaded] = createSignal(false);
  onMount(() => {
    if (att().kind === "image") debugImageTiming("img:mount", att().name, url());
  });
  return (
    <div class="message-media">
      <Show when={att().kind === "image"}>
        <img
          class="media-image"
          classList={{ "is-loaded": loaded() }}
          src={url()}
          alt={att().name}
          width={att().width ?? undefined}
          height={att().height ?? undefined}
          loading="eager"
          decoding="async"
          fetchpriority="high"
          onLoad={() => {
            debugImageTiming("img:load", att().name, url());
            setLoaded(true);
          }}
          onError={() => {
            debugImageTiming("img:error", att().name, url());
            setLoaded(true);
          }}
        />
      </Show>
      <Show when={att().kind === "video"}>
        <video class="media-video" src={url()} controls preload="metadata" />
      </Show>
      <Show when={att().kind === "audio"}>
        <audio class="media-audio" src={url()} controls preload="metadata" />
      </Show>
      <Show when={att().kind === "file"}>
        {/* Collapsed file card. Expanding opens the highlighted viewer, which
          * falls back to a download for a non-text file. */}
        <div class="media-file">
          <button
            class="media-file-open"
            type="button"
            onClick={() => props.onOpenFile(att().name)}
          >
            {att().name}
          </button>
          <a class="media-file-download" href={url()} download={att().name}>
            download
          </a>
        </div>
      </Show>
    </div>
  );
}

function MessageRow(props: {
  message: WebMessage;
  prev?: WebMessage;
  onOpenFile: (name: string) => void;
}) {
  // A continuation hides the header and shows its time only on hover, in the
  // reserved left gutter. `prev` is supplied reactively from the message list,
  // so prepended history re-evaluates the boundary row's grouping.
  const continuation = () => {
    const prev = props.prev;
    const msg = props.message;
    return (
      !!prev &&
      prev.sender === msg.sender &&
      msg.timestamp_ms - prev.timestamp_ms < GROUP_WINDOW_MS
    );
  };
  return (
    <div class="message" classList={{ "is-continuation": continuation() }}>
      {/* The time always lives in the left gutter so it sits in one consistent
        * column: shown on a group's first row, revealed on hover for the rest. */}
      <span class="message-time-gutter">{formatTime(props.message.timestamp_ms)}</span>
      <Show when={!continuation()}>
        <div class="message-meta">
          <span class="message-sender">{props.message.sender}</span>
        </div>
      </Show>
      <MessageBody fragments={props.message.fragments} />
      <Show when={props.message.attachment}>
        <Attachment message={props.message} onOpenFile={props.onOpenFile} />
      </Show>
      <Show when={!props.message.attachment && props.message.progress}>
        <TransferProgressBar progress={props.message.progress!} />
      </Show>
    </div>
  );
}

export default function App() {
  const [messages, setMessages] = createSignal<WebMessage[]>([]);
  const [connected, setConnected] = createSignal(false);
  const [connectionErrorVisible, setConnectionErrorVisible] = createSignal(false);
  // Drives virtua's `shift`: while true a data change is treated as a prepend so
  // scroll position is anchored from the end (reverse infinite scroll).
  const [prepend, setPrepend] = createSignal(false);

  // Screen shares this browser can watch, and the stream ids currently playing.
  const [shares, setShares] = createSignal<ShareInfo[]>([]);
  const [playing, setPlaying] = createSignal<number[]>([]);
  // Per-stream play-failure messages reported by the client, shown on the row.
  const [shareErrors, setShareErrors] = createSignal<Record<number, string>>({});
  const [sharePaneHeight, setSharePaneHeight] = createSignal(DEFAULT_SHARE_PANE_HEIGHT);
  const [fullscreenStream, setFullscreenStream] = createSignal<number | null>(null);
  // The file currently expanded into the viewer panel, or null when none is.
  const [openFile, setOpenFile] = createSignal<string | null>(null);
  const [filePanelWidth, setFilePanelWidth] = createSignal(DEFAULT_FILE_PANEL_WIDTH);
  const [filePanelResizing, setFilePanelResizing] = createSignal(false);

  // The compose box is hidden until the client reports a writable feed in its
  // `config` envelope, so a read-only view never shows controls it cannot use.
  const [readonly, setReadonly] = createSignal(true);
  const [draft, setDraft] = createSignal("");
  // Files dragged onto the composer, held until the message is submitted.
  const [queued, setQueued] = createSignal<File[]>([]);
  const [dragActive, setDragActive] = createSignal(false);
  // A per-connection counter naming each upload so its chunk frames route to the
  // right server-side file.
  let nextUploadId = 1;

  function setShareError(streamId: number, message: string) {
    setShareErrors((prev) => ({ ...prev, [streamId]: message }));
  }

  function clearShareError(streamId: number) {
    setShareErrors((prev) => {
      if (!(streamId in prev)) return prev;
      const next = { ...prev };
      delete next[streamId];
      return next;
    });
  }
  // One decoder and canvas per stream, so several shares can play at once.
  const decoders = new Map<number, ScreenShareDecoder>();
  const canvases = new Map<number, HTMLCanvasElement>();

  function registerCanvas(streamId: number, el: HTMLCanvasElement) {
    canvases.set(streamId, el);
  }

  function closeDecoder(streamId: number) {
    decoders.get(streamId)?.close();
    decoders.delete(streamId);
    setPlaying((prev) => prev.filter((id) => id !== streamId));
  }

  let mainEl: HTMLElement | undefined;
  let appBodyEl: HTMLDivElement | undefined;
  let logEl: HTMLDivElement | undefined;
  let contentEl: HTMLDivElement | undefined;
  let composerTextEl: HTMLTextAreaElement | undefined;
  let handle: VirtualizerHandle | undefined;
  let resizeObserver: ResizeObserver | undefined;
  let chatViewportResizeObserver: ResizeObserver | undefined;
  let splitResizeObserver: ResizeObserver | undefined;
  let fileSplitResizeObserver: ResizeObserver | undefined;
  let viewportPinFrame = 0;
  let socket: WebSocket | undefined;
  let reconnectTimer: number | undefined;
  let connectionErrorTimer: number | undefined;
  const imagePreloads = new Map<string, ImagePreload>();
  let paneResize:
    | {
        move: (event: PointerEvent) => void;
        up: (event: PointerEvent) => void;
      }
    | undefined;
  let filePanelResize:
    | {
        move: (event: PointerEvent) => void;
        up: (event: PointerEvent) => void;
        cancelFrame: () => void;
      }
    | undefined;

  // Paging cursor: the sequence number of the oldest message currently held and
  // whether the server still has older history to send.
  let oldestSeq = 0;
  let hasMore = false;
  let loadingOlder = false;

  // HARD REQUIREMENT (see docs/web.md): while following, the view MUST stay glued
  // to the newest message and MUST NOT break when media grows the layout after a
  // message arrives. Two rules:
  //   1. `following` flips ONLY on a genuine user scroll. With virtua a position-
  //      derived flag fails: item-resize jump compensation produces scroll events
  //      we never initiated, which would latch follow off. So we flip `following`
  //      only while a real input gesture (wheel/touch/pointer/key) is in control.
  //   2. EVERY content-size change re-pins (ResizeObserver on the wrapper around
  //      the Virtualizer), so media that grows later is followed, not stranded.
  let following = true;
  // True while a real user input gesture controls the scroll.
  let userDriving = false;
  // True while a pin() we initiated is in flight, so onScroll ignores it.
  let suppress = false;
  let suppressTimer: number | undefined;
  let idleTimer: number | undefined;

  // Use virtua's measured geometry, not DOM scrollHeight: when tail items are
  // still unmeasured, totalSize is an estimate and scrollHeight can disagree.
  function atBottom(): boolean {
    if (!handle) return true;
    return (
      handle.scrollOffset >= handle.scrollSize - handle.viewportSize - BOTTOM_EPSILON
    );
  }

  // The ONLY thing allowed to flip `following`. Bound to genuine input events.
  function markUser() {
    userDriving = true;
    if (idleTimer) {
      clearTimeout(idleTimer);
      idleTimer = undefined;
    }
  }

  function onKeyDown(e: KeyboardEvent) {
    switch (e.key) {
      case "PageUp":
      case "PageDown":
      case "Home":
      case "End":
      case "ArrowUp":
      case "ArrowDown":
      case " ":
        markUser();
    }
  }

  // Re-pin to the newest message. A no-op while detached. Safe to over-call.
  function pin() {
    // Changing the split width can resize every mounted chat row. Let the
    // virtualizer process those measurements without repeatedly restarting
    // scrollToIndex's measurement loop; the drag end performs one final pin.
    if (filePanelResize || !handle || !following) return;
    const last = messages().length - 1;
    if (last < 0) return;
    suppress = true;
    // scrollToIndex re-resolves the target as the last item (and a late-decoding
    // image inside it) gets measured, so it lands exactly at the newest message.
    handle.scrollToIndex(last, { align: "end" });
    // scrollToIndex is multi-frame and may emit zero scroll events when already
    // at the bottom, so clearing `suppress` from onScrollEnd would deadlock it.
    // Always clear on a timer that outlives the measurement window.
    if (suppressTimer) clearTimeout(suppressTimer);
    suppressTimer = window.setTimeout(() => {
      suppress = false;
    }, 250);
  }

  function preloadImage(message: WebMessage): boolean {
    const att = message.attachment;
    if (!att || att.kind !== "image") return false;

    const url = fileUrl(att.name);
    if (imagePreloads.has(url)) return false;

    const link = document.createElement("link");
    link.rel = "preload";
    link.as = "image";
    link.href = url;
    link.fetchPriority = "high";
    document.head.appendChild(link);
    debugImageTiming("preload:link", att.name, url);

    const img = new Image(att.width ?? undefined, att.height ?? undefined);
    img.decoding = "async";
    img.loading = "eager";
    img.fetchPriority = "high";
    img.addEventListener("load", () => debugImageTiming("preload:load", att.name, url), {
      once: true,
    });
    img.addEventListener("error", () => debugImageTiming("preload:error", att.name, url), {
      once: true,
    });
    imagePreloads.set(url, { image: img, link });
    img.src = url;

    while (imagePreloads.size > IMAGE_PRELOAD_CACHE_LIMIT) {
      const oldest = imagePreloads.keys().next().value;
      if (oldest === undefined) break;
      imagePreloads.get(oldest)?.link.remove();
      imagePreloads.delete(oldest);
    }
    return true;
  }

  function preloadRecentImages(batch: readonly WebMessage[]) {
    let started = 0;
    for (
      let i = batch.length - 1;
      i >= 0 && started < IMAGE_PRELOAD_BATCH_LIMIT;
      i--
    ) {
      if (preloadImage(batch[i]!)) started++;
    }
  }

  function recentImageIndexes(): number[] {
    const items = messages();
    const indexes: number[] = [];
    for (
      let i = items.length - 1;
      i >= 0 && indexes.length < RECENT_IMAGE_KEEP_MOUNTED;
      i--
    ) {
      if (items[i]?.attachment?.kind === "image") indexes.push(i);
    }
    indexes.reverse();
    return indexes;
  }

  function requestOlder() {
    if (!hasMore || loadingOlder) return;
    if (!socket || socket.readyState !== WebSocket.OPEN) return;
    loadingOlder = true;
    const req: ClientRequest = {
      type: "load_older",
      before_seq: oldestSeq,
      limit: PAGE,
    };
    socket.send(JSON.stringify(req));
  }

  function playShare(streamId: number) {
    if (socket && socket.readyState === WebSocket.OPEN) {
      clearShareError(streamId);
      socket.send(JSON.stringify({ type: "play_share", stream_id: streamId } as ClientRequest));
    }
  }

  function exitShareFullscreen() {
    setFullscreenStream(null);
    if (document.fullscreenElement === mainEl) document.exitFullscreen().catch(() => {});
  }

  function stopShare(streamId: number) {
    if (socket && socket.readyState === WebSocket.OPEN) {
      socket.send(JSON.stringify({ type: "stop_share", stream_id: streamId } as ClientRequest));
    }
    if (fullscreenStream() === streamId) exitShareFullscreen();
    closeDecoder(streamId);
  }

  function sendJson(req: ClientRequest) {
    if (socket && socket.readyState === WebSocket.OPEN) {
      socket.send(JSON.stringify(req));
    }
  }

  function hideConnectionError() {
    if (connectionErrorTimer !== undefined) {
      clearTimeout(connectionErrorTimer);
      connectionErrorTimer = undefined;
    }
    setConnectionErrorVisible(false);
  }

  function scheduleConnectionError() {
    if (connectionErrorVisible() || connectionErrorTimer !== undefined) return;
    connectionErrorTimer = window.setTimeout(() => {
      connectionErrorTimer = undefined;
      if (!connected()) setConnectionErrorVisible(true);
    }, CONNECTION_ERROR_DELAY_MS);
  }

  function resizeComposer() {
    const textArea = composerTextEl;
    if (!textArea) return;

    textArea.style.height = "auto";
    const maxHeight = Number.parseFloat(getComputedStyle(textArea).maxHeight);
    const nextHeight = Number.isFinite(maxHeight)
      ? Math.min(textArea.scrollHeight, maxHeight)
      : textArea.scrollHeight;
    textArea.style.height = `${nextHeight}px`;
    textArea.style.overflowY =
      Number.isFinite(maxHeight) && textArea.scrollHeight > maxHeight ? "auto" : "hidden";
  }

  // Streams one queued file to the client: an `upload_start`, then binary chunks
  // each prefixed with the little-endian upload id, then `upload_finish`. The
  // server reassembles them into a temp file and relays it as a normal upload.
  async function sendFile(file: File) {
    if (!socket || socket.readyState !== WebSocket.OPEN) return;
    const uploadId = nextUploadId++;
    sendJson({ type: "upload_start", upload_id: uploadId, name: file.name, size: file.size });
    const bytes = new Uint8Array(await file.arrayBuffer());
    for (let offset = 0; offset < bytes.length; offset += UPLOAD_CHUNK_BYTES) {
      const chunk = bytes.subarray(offset, offset + UPLOAD_CHUNK_BYTES);
      const frame = new Uint8Array(4 + chunk.length);
      new DataView(frame.buffer).setUint32(0, uploadId, true);
      frame.set(chunk, 4);
      socket.send(frame);
    }
    sendJson({ type: "upload_finish", upload_id: uploadId });
  }

  function submitCompose() {
    const body = draft().trim();
    const files = queued();
    if (!body && files.length === 0) return;
    if (body) sendJson({ type: "send_message", body });
    for (const file of files) void sendFile(file);
    setDraft("");
    setQueued([]);
    queueMicrotask(resizeComposer);
  }

  function onComposeKeyDown(event: KeyboardEvent) {
    if (event.key === "Enter" && !event.shiftKey) {
      event.preventDefault();
      submitCompose();
    }
  }

  function onComposeDragOver(event: DragEvent) {
    event.preventDefault();
    setDragActive(true);
  }

  function onComposeDragLeave(event: DragEvent) {
    event.preventDefault();
    setDragActive(false);
  }

  function onComposeDrop(event: DragEvent) {
    event.preventDefault();
    setDragActive(false);
    const files = event.dataTransfer ? Array.from(event.dataTransfer.files) : [];
    if (files.length > 0) setQueued((prev) => [...prev, ...files]);
  }

  function removeQueued(index: number) {
    setQueued((prev) => prev.filter((_, i) => i !== index));
  }

  function hasVideoPane(): boolean {
    return shares().length > 0 && playing().length > 0;
  }

  function clampSharePaneHeight(height: number): number {
    const total = mainEl?.clientHeight ?? 0;
    if (total <= 0) return Math.max(MIN_SHARE_PANE_HEIGHT, height);
    const minShare = Math.min(
      MIN_SHARE_PANE_HEIGHT,
      Math.max(96, total - MIN_CHAT_PANE_HEIGHT - DIVIDER_SIZE),
    );
    const maxShare = Math.max(minShare, total - MIN_CHAT_PANE_HEIGHT - DIVIDER_SIZE);
    return clamp(height, minShare, maxShare);
  }

  function setClampedSharePaneHeight(height: number) {
    setSharePaneHeight(clampSharePaneHeight(height));
    pin();
  }

  function clampFilePanelWidth(width: number): number {
    const total = appBodyEl?.clientWidth ?? 0;
    if (total <= 0) return Math.max(MIN_FILE_PANEL_WIDTH, width);
    const minFile = Math.min(
      MIN_FILE_PANEL_WIDTH,
      Math.max(240, total - MIN_CHAT_SPLIT_WIDTH - FILE_PANEL_DIVIDER_SIZE),
    );
    const maxFile = Math.max(
      minFile,
      total - MIN_CHAT_SPLIT_WIDTH - FILE_PANEL_DIVIDER_SIZE,
    );
    return clamp(width, minFile, maxFile);
  }

  function setClampedFilePanelWidth(width: number) {
    setFilePanelWidth(clampFilePanelWidth(width));
    pin();
  }

  function removePaneResizeListeners() {
    if (!paneResize) return;
    window.removeEventListener("pointermove", paneResize.move);
    window.removeEventListener("pointerup", paneResize.up);
    window.removeEventListener("pointercancel", paneResize.up);
    paneResize = undefined;
  }

  function removeFilePanelResizeListeners() {
    if (!filePanelResize) return;
    window.removeEventListener("pointermove", filePanelResize.move);
    window.removeEventListener("pointerup", filePanelResize.up);
    window.removeEventListener("pointercancel", filePanelResize.up);
    filePanelResize.cancelFrame();
    filePanelResize = undefined;
    setFilePanelResizing(false);
  }

  function beginPaneResize(event: PointerEvent) {
    if (fullscreenStream() !== null) return;
    event.preventDefault();
    removePaneResizeListeners();
    const startY = event.clientY;
    const startHeight = sharePaneHeight();
    const move = (moveEvent: PointerEvent) => {
      moveEvent.preventDefault();
      setClampedSharePaneHeight(startHeight + moveEvent.clientY - startY);
    };
    const up = (upEvent: PointerEvent) => {
      upEvent.preventDefault();
      removePaneResizeListeners();
    };
    paneResize = { move, up };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up);
    window.addEventListener("pointercancel", up);
  }

  function beginFilePanelResize(event: PointerEvent) {
    event.preventDefault();
    removeFilePanelResizeListeners();
    const startX = event.clientX;
    const startWidth = filePanelWidth();
    // The app body's width does not change as its children are resized. Read
    // it once so pointer moves never force layout merely to recompute bounds.
    const total = appBodyEl?.clientWidth ?? 0;
    const minWidth =
      total > 0
        ? Math.min(
            MIN_FILE_PANEL_WIDTH,
            Math.max(240, total - MIN_CHAT_SPLIT_WIDTH - FILE_PANEL_DIVIDER_SIZE),
          )
        : MIN_FILE_PANEL_WIDTH;
    const maxWidth =
      total > 0
        ? Math.max(
            minWidth,
            total - MIN_CHAT_SPLIT_WIDTH - FILE_PANEL_DIVIDER_SIZE,
          )
        : Number.POSITIVE_INFINITY;
    let nextWidth = startWidth;
    let frame = 0;
    const updateWidth = (clientX: number) => {
      nextWidth = clamp(startWidth + startX - clientX, minWidth, maxWidth);
    };
    const flush = () => {
      if (frame) cancelAnimationFrame(frame);
      frame = 0;
      setFilePanelWidth(nextWidth);
    };
    const move = (moveEvent: PointerEvent) => {
      moveEvent.preventDefault();
      updateWidth(moveEvent.clientX);
      // Chrome can deliver pointer events faster than it can lay out both
      // panes. Coalesce them so there is at most one flex relayout per paint.
      if (!frame) {
        frame = requestAnimationFrame(() => {
          frame = 0;
          setFilePanelWidth(nextWidth);
        });
      }
    };
    const up = (upEvent: PointerEvent) => {
      upEvent.preventDefault();
      if (upEvent.type === "pointerup") updateWidth(upEvent.clientX);
      flush();
      removeFilePanelResizeListeners();
      pin();
    };
    filePanelResize = {
      move,
      up,
      cancelFrame: () => {
        if (frame) cancelAnimationFrame(frame);
        frame = 0;
      },
    };
    setFilePanelResizing(true);
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up);
    window.addEventListener("pointercancel", up);
  }

  function onDividerKeyDown(event: KeyboardEvent) {
    if (fullscreenStream() !== null) return;
    if (event.key === "ArrowUp") {
      event.preventDefault();
      setClampedSharePaneHeight(sharePaneHeight() - PANE_KEY_STEP);
    } else if (event.key === "ArrowDown") {
      event.preventDefault();
      setClampedSharePaneHeight(sharePaneHeight() + PANE_KEY_STEP);
    }
  }

  function onFileDividerKeyDown(event: KeyboardEvent) {
    if (event.key === "ArrowLeft") {
      event.preventDefault();
      setClampedFilePanelWidth(filePanelWidth() + FILE_PANEL_KEY_STEP);
    } else if (event.key === "ArrowRight") {
      event.preventDefault();
      setClampedFilePanelWidth(filePanelWidth() - FILE_PANEL_KEY_STEP);
    }
  }

  async function toggleShareFullscreen(streamId: number) {
    if (fullscreenStream() === streamId) {
      exitShareFullscreen();
      return;
    }
    setFullscreenStream(streamId);
    if (mainEl?.requestFullscreen && document.fullscreenElement !== mainEl) {
      await mainEl.requestFullscreen().catch(() => {});
    }
  }

  function onDocumentFullscreenChange() {
    if (!document.fullscreenElement) setFullscreenStream(null);
  }

  function onScroll(offset: number) {
    // Page in older history when the user nears the top. Guarded so it never
    // duplicates an in-flight request; after a prepend the offset moves down
    // (content added above), so it does not immediately re-trigger.
    if (offset < TOP_THRESHOLD) requestOlder();

    // Rule 1: ignore our own pin() scroll and any scroll not under user control
    // (virtua resize-jump compensation). Only a real user scroll flips follow.
    if (suppress) return;
    if (!userDriving) return;
    following = atBottom();
  }

  // virtua's onScrollEnd fires ~150ms after scrolling stops. Release user
  // control shortly after so a later spontaneous compensation scroll is not
  // mis-attributed to the user.
  function onScrollEnd() {
    if (idleTimer) clearTimeout(idleTimer);
    idleTimer = window.setTimeout(() => {
      userDriving = false;
    }, 120);
  }

  // After a prepend renders with shift=true, reset so the next data change is a
  // normal append. Depends on messages() so it runs once per change, in the
  // effect phase after virtua's render has already read shift.
  createEffect(() => {
    messages();
    setPrepend(false);
  });

  function connect() {
    const ws = new WebSocket(`ws://${location.host}/ws`);
    // Video frames arrive as binary messages; everything else is JSON text.
    ws.binaryType = "arraybuffer";
    ws.onopen = () => {
      setConnected(true);
      hideConnectionError();
    };
    ws.onmessage = (ev) => {
      if (typeof ev.data !== "string") {
        // A binary message is either a chat feed frame (zero sentinel) or a
        // video frame. decodeFeed returns null for the latter.
        const buffer = ev.data as ArrayBuffer;
        const feed = decodeFeed(buffer);
        if (!feed) {
          const frame = parseFrame(buffer);
          if (frame) decoders.get(frame.streamId)?.decode(frame);
          return;
        }
        if (feed.kind === "sync") {
          preloadRecentImages(feed.messages);
          setMessages(feed.messages);
          oldestSeq = feed.oldest_seq;
          hasMore = feed.has_more;
          following = true;
          pin();
        } else if (feed.kind === "older") {
          loadingOlder = false;
          if (feed.messages.length > 0) {
            preloadRecentImages(feed.messages);
            setPrepend(true);
            setMessages((prev) => [...feed.messages, ...prev]);
          }
          oldestSeq = feed.oldest_seq;
          hasMore = feed.has_more;
        } else {
          // A live message. Upsert by the announcement timestamp and file id;
          // transfer ids are reused after server restarts, while the pair
          // identifies one file.
          const msg = feed.message;
          preloadImage(msg);
          setMessages((prev) => {
            if (msg.file_id !== null) {
              const i = prev.findIndex(
                (m) => m.file_id === msg.file_id && m.timestamp_ms === msg.timestamp_ms,
              );
              if (i >= 0) {
                const next = prev.slice();
                next[i] = msg;
                return next;
              }
            }
            return [...prev, msg];
          });
          pin();
        }
        return;
      }
      const env: ServerEnvelope = JSON.parse(ev.data);
      if (env.type === "share_available") {
        setShares((prev) => {
          const share = {
            stream_id: env.stream_id,
            sender: env.sender,
            codec: env.codec,
            width: env.width,
            height: env.height,
          };
          const index = prev.findIndex((s) => s.stream_id === env.stream_id);
          if (index < 0) return [...prev, share];
          const next = prev.slice();
          next[index] = share;
          return next;
        });
      } else if (env.type === "share_config") {
        // Configure this stream's decoder from the codec and descriptor the
        // client supplies, then mark the share as playing. The canvas was
        // mounted with the share's row, so it is already registered. The client
        // broadcasts share_config on every play request so a tab that joined
        // after the share started can bootstrap its decoder; a tab already
        // playing this stream keeps its live decoder rather than resetting it.
        const canvas = canvases.get(env.stream_id);
        if (canvas && !decoders.has(env.stream_id)) {
          clearShareError(env.stream_id);
          const decoder = new ScreenShareDecoder(canvas);
          decoders.set(env.stream_id, decoder);
          decoder.configure(env.codec, new Uint8Array(env.extradata));
          setPlaying((prev) =>
            prev.includes(env.stream_id) ? prev : [...prev, env.stream_id],
          );
        }
      } else if (env.type === "file_progress") {
        // Merge into the still-placeholder message (no attachment yet). Once the
        // file lands and the enriched message replaces it, `progress` is gone and
        // the bar disappears on its own.
        setMessages((prev) => {
          const i = prev.findIndex(
            (m) =>
              m.file_id === env.file_id &&
              m.timestamp_ms === env.timestamp_ms &&
              !m.attachment,
          );
          if (i < 0) return prev;
          const next = prev.slice();
          next[i] = { ...next[i], progress: { transferred: env.transferred, total: env.total } };
          return next;
        });
      } else if (env.type === "config") {
        setReadonly(env.readonly);
      } else if (env.type === "share_error") {
        setShareError(env.stream_id, env.message);
      } else if (env.type === "share_ended") {
        setShares((prev) => prev.filter((s) => s.stream_id !== env.stream_id));
        clearShareError(env.stream_id);
        if (fullscreenStream() === env.stream_id) {
          exitShareFullscreen();
        }
        closeDecoder(env.stream_id);
      }
    };
    ws.onclose = () => {
      setConnected(false);
      scheduleConnectionError();
      reconnectTimer = window.setTimeout(connect, 1000);
    };
    ws.onerror = () => ws.close();
    socket = ws;
  }

  onMount(() => {
    if (contentEl) {
      // Fires on any content-size change: new message, image/video decode, font
      // load, reflow. The wrapper grows with virtua's container, so media that
      // grows later is followed rather than stranded (rule 2).
      resizeObserver = new ResizeObserver(() => pin());
      resizeObserver.observe(contentEl);
    }
    if (logEl) {
      let observedHeight = -1;
      chatViewportResizeObserver = new ResizeObserver((entries) => {
        const rect = entries[entries.length - 1]?.contentRect;
        if (!rect || Math.abs(rect.height - observedHeight) < 0.01) return;
        observedHeight = rect.height;

        // The virtualizer observes this element too. Pin on the next frame so
        // its viewport measurement is current before scrollToIndex aligns the
        // final message. `pin` remains a no-op when the user is detached.
        if (viewportPinFrame) cancelAnimationFrame(viewportPinFrame);
        viewportPinFrame = requestAnimationFrame(() => {
          viewportPinFrame = 0;
          pin();
        });
      });
      chatViewportResizeObserver.observe(logEl);
    }
    if (mainEl) {
      let observedHeight = -1;
      splitResizeObserver = new ResizeObserver((entries) => {
        const rect = entries[entries.length - 1]?.contentRect;
        if (!rect || Math.abs(rect.height - observedHeight) < 0.01) return;
        observedHeight = rect.height;
        setSharePaneHeight((height) => clampSharePaneHeight(height));
        pin();
      });
      splitResizeObserver.observe(mainEl);
    }
    if (appBodyEl) {
      let observedWidth = -1;
      fileSplitResizeObserver = new ResizeObserver((entries) => {
        const rect = entries[entries.length - 1]?.contentRect;
        if (!rect || Math.abs(rect.width - observedWidth) < 0.01) return;
        observedWidth = rect.width;
        setFilePanelWidth((width) => clampFilePanelWidth(width));
        pin();
      });
      fileSplitResizeObserver.observe(appBodyEl);
    }
    document.addEventListener("fullscreenchange", onDocumentFullscreenChange);
    scheduleConnectionError();
    connect();
  });
  onCleanup(() => {
    resizeObserver?.disconnect();
    chatViewportResizeObserver?.disconnect();
    splitResizeObserver?.disconnect();
    fileSplitResizeObserver?.disconnect();
    if (viewportPinFrame) cancelAnimationFrame(viewportPinFrame);
    document.removeEventListener("fullscreenchange", onDocumentFullscreenChange);
    removePaneResizeListeners();
    removeFilePanelResizeListeners();
    if (reconnectTimer) clearTimeout(reconnectTimer);
    if (connectionErrorTimer !== undefined) clearTimeout(connectionErrorTimer);
    if (suppressTimer) clearTimeout(suppressTimer);
    if (idleTimer) clearTimeout(idleTimer);
    for (const decoder of decoders.values()) decoder.close();
    decoders.clear();
    for (const preload of imagePreloads.values()) preload.link.remove();
    imagePreloads.clear();
    socket?.close();
  });

  return (
    <div class="app">
      <Show when={connectionErrorVisible()}>
        <div class="conn-overlay" role="status" aria-live="polite">
          Unable to connect — retrying…
        </div>
      </Show>
      <div
        class="app-body"
        classList={{ "is-resizing-file-panel": filePanelResizing() }}
        ref={appBodyEl}
      >
        <main
          class="app-main"
          classList={{
            "has-video-pane": hasVideoPane(),
            "is-share-fullscreen": fullscreenStream() !== null,
          }}
          ref={mainEl}
          style={hasVideoPane() ? `--share-pane-height: ${sharePaneHeight()}px` : undefined}
        >
          <Show when={shares().length > 0}>
            <section class="share-pane">
              <ScreenShare
                shares={shares()}
                playing={playing()}
                errors={shareErrors()}
                fullscreenStream={fullscreenStream()}
                onPlay={playShare}
                onStop={stopShare}
                onToggleFullscreen={toggleShareFullscreen}
                canvasRef={registerCanvas}
              />
              {/* Drag the bottom edge of the black video area to resize the chat
                * below. The grabber overlays the pane's lower edge; no separator
                * line is drawn. */}
              <Show when={hasVideoPane()}>
                <div
                  class="pane-resize"
                  role="separator"
                  aria-label="Resize chat"
                  aria-orientation="horizontal"
                  tabIndex={0}
                  onPointerDown={beginPaneResize}
                  onKeyDown={onDividerKeyDown}
                />
              </Show>
            </section>
          </Show>
          <div
            class="chat-log"
            ref={logEl}
            onWheel={markUser}
            onTouchStart={markUser}
            onTouchMove={markUser}
            onPointerDown={markUser}
            onKeyDown={onKeyDown}
          >
            <div class="chat-log-content" ref={contentEl}>
              <Virtualizer
                ref={(h) => (handle = h)}
                scrollRef={logEl}
                data={messages()}
                shift={prepend()}
                keepMounted={recentImageIndexes()}
                onScroll={onScroll}
                onScrollEnd={onScrollEnd}
              >
                {(message, index) => (
                  <MessageRow
                    message={message}
                    prev={messages()[index() - 1]}
                    onOpenFile={setOpenFile}
                  />
                )}
              </Virtualizer>
            </div>
          </div>
          <Show when={!readonly()}>
            <section
              class="composer"
              classList={{ "is-drag-active": dragActive() }}
              onDragOver={onComposeDragOver}
              onDragLeave={onComposeDragLeave}
              onDrop={onComposeDrop}
            >
              <Show when={queued().length > 0}>
                <div class="composer-files">
                  <For each={queued()}>
                    {(file, index) => (
                      <span class="composer-chip">
                        <span class="composer-chip-name">{file.name}</span>
                        <button
                          class="composer-chip-remove"
                          type="button"
                          aria-label={`Remove ${file.name}`}
                          onClick={() => removeQueued(index())}
                        >
                          ×
                        </button>
                      </span>
                    )}
                  </For>
                </div>
              </Show>
              <textarea
                class="composer-text"
                ref={composerTextEl}
                rows={1}
                placeholder="Write a message… (drag files to attach)"
                value={draft()}
                onInput={(event) => {
                  setDraft(event.currentTarget.value);
                  resizeComposer();
                }}
                onKeyDown={onComposeKeyDown}
              />
            </section>
          </Show>
        </main>
        <Show when={openFile()}>
          <>
            <div
              class="file-panel-resize"
              role="separator"
              aria-label="Resize code view"
              aria-orientation="vertical"
              aria-valuemin={MIN_FILE_PANEL_WIDTH}
              aria-valuenow={Math.round(filePanelWidth())}
              tabIndex={0}
              onPointerDown={beginFilePanelResize}
              onKeyDown={onFileDividerKeyDown}
            />
            <aside
              class="file-panel"
              style={`--file-panel-width: ${filePanelWidth()}px`}
            >
              <FileViewer name={openFile()!} onClose={() => setOpenFile(null)} />
            </aside>
          </>
        </Show>
      </div>
    </div>
  );
}
