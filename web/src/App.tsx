import { createSignal, createEffect, onCleanup, onMount, Show } from "solid-js";
import { Virtualizer, type VirtualizerHandle } from "./vendor/virtua/solid/Virtualizer";
import type { WebMessage, ServerEnvelope, ClientRequest, ShareInfo } from "./types";
import ScreenShare from "./ScreenShare";
import { ScreenShareDecoder, parseFrame } from "./video-decode";

// Pixel tolerance when deciding the view is "at the bottom". Scroll positions
// are fractional, so an exact comparison would intermittently read as
// not-at-bottom right after a programmatic scroll.
const BOTTOM_EPSILON = 4;

// Distance from the top, in pixels, at which scrolling up requests older history.
const TOP_THRESHOLD = 200;

// How many older messages one paging request asks for.
const PAGE = 100;

const DEFAULT_SHARE_PANE_HEIGHT = 360;
const MIN_SHARE_PANE_HEIGHT = 160;
const MIN_CHAT_PANE_HEIGHT = 140;
const DIVIDER_SIZE = 9;
const PANE_KEY_STEP = 32;

// Builds the asset URL for an attachment served from the client's receive dir.
function fileUrl(name: string): string {
  return `/files/${encodeURIComponent(name)}`;
}

function clamp(value: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, value));
}

function formatTime(ms: number): string {
  if (!ms) return "";
  const d = new Date(ms);
  return d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}

function Attachment(props: { message: WebMessage }) {
  const att = () => props.message.attachment!;
  const url = () => fileUrl(att().name);
  // Fades the image in on decode instead of snapping. The box is already
  // reserved by width/height, so this only affects the pixels, never layout.
  const [loaded, setLoaded] = createSignal(false);
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
          onLoad={() => setLoaded(true)}
          onError={() => setLoaded(true)}
        />
      </Show>
      <Show when={att().kind === "video"}>
        <video class="media-video" src={url()} controls preload="metadata" />
      </Show>
      <Show when={att().kind === "audio"}>
        <audio class="media-audio" src={url()} controls preload="metadata" />
      </Show>
      <Show when={att().kind === "file"}>
        <a class="media-file" href={url()} download={att().name}>
          {att().name}
        </a>
      </Show>
    </div>
  );
}

function MessageRow(props: { message: WebMessage }) {
  return (
    <div class="message">
      <div class="message-meta">
        <span class="message-sender">{props.message.sender}</span>
        <span class="message-time">{formatTime(props.message.timestamp_ms)}</span>
      </div>
      <Show when={props.message.body}>
        <div class="message-body">{props.message.body}</div>
      </Show>
      <Show when={props.message.attachment}>
        <Attachment message={props.message} />
      </Show>
    </div>
  );
}

export default function App() {
  const [messages, setMessages] = createSignal<WebMessage[]>([]);
  const [connected, setConnected] = createSignal(false);
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
  let logEl: HTMLDivElement | undefined;
  let contentEl: HTMLDivElement | undefined;
  let handle: VirtualizerHandle | undefined;
  let resizeObserver: ResizeObserver | undefined;
  let splitResizeObserver: ResizeObserver | undefined;
  let socket: WebSocket | undefined;
  let reconnectTimer: number | undefined;
  let paneResize:
    | {
        move: (event: PointerEvent) => void;
        up: (event: PointerEvent) => void;
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
    if (!handle || !following) return;
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

  function removePaneResizeListeners() {
    if (!paneResize) return;
    window.removeEventListener("pointermove", paneResize.move);
    window.removeEventListener("pointerup", paneResize.up);
    window.removeEventListener("pointercancel", paneResize.up);
    paneResize = undefined;
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
    ws.onopen = () => setConnected(true);
    ws.onmessage = (ev) => {
      if (typeof ev.data !== "string") {
        const frame = parseFrame(ev.data as ArrayBuffer);
        if (frame) decoders.get(frame.streamId)?.decode(frame);
        return;
      }
      const env: ServerEnvelope = JSON.parse(ev.data);
      if (env.type === "sync") {
        setMessages(env.messages);
        oldestSeq = env.oldest_seq;
        hasMore = env.has_more;
        following = true;
        pin();
      } else if (env.type === "older") {
        loadingOlder = false;
        if (env.messages.length > 0) {
          setPrepend(true);
          setMessages((prev) => [...env.messages, ...prev]);
        }
        oldestSeq = env.oldest_seq;
        hasMore = env.has_more;
      } else if (env.type === "share_available") {
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
      } else if (env.type === "share_error") {
        setShareError(env.stream_id, env.message);
      } else if (env.type === "share_ended") {
        setShares((prev) => prev.filter((s) => s.stream_id !== env.stream_id));
        clearShareError(env.stream_id);
        if (fullscreenStream() === env.stream_id) {
          exitShareFullscreen();
        }
        closeDecoder(env.stream_id);
      } else {
        // Upsert by the announcement timestamp and file id. Transfer ids are
        // reused after server restarts, while the pair identifies one file.
        const msg = env.message;
        setMessages((prev) => {
          if (msg.file_id !== null) {
            const i = prev.findIndex(
              (m) =>
                m.file_id === msg.file_id &&
                m.timestamp_ms === msg.timestamp_ms,
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
    };
    ws.onclose = () => {
      setConnected(false);
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
    if (mainEl) {
      splitResizeObserver = new ResizeObserver(() => {
        setSharePaneHeight((height) => clampSharePaneHeight(height));
        pin();
      });
      splitResizeObserver.observe(mainEl);
    }
    document.addEventListener("fullscreenchange", onDocumentFullscreenChange);
    connect();
  });
  onCleanup(() => {
    resizeObserver?.disconnect();
    splitResizeObserver?.disconnect();
    document.removeEventListener("fullscreenchange", onDocumentFullscreenChange);
    removePaneResizeListeners();
    if (reconnectTimer) clearTimeout(reconnectTimer);
    if (suppressTimer) clearTimeout(suppressTimer);
    if (idleTimer) clearTimeout(idleTimer);
    for (const decoder of decoders.values()) decoder.close();
    decoders.clear();
    socket?.close();
  });

  return (
    <div class="app" classList={{ "is-share-fullscreen": fullscreenStream() !== null }}>
      <header class="app-header">
        <span class="app-title">chatt</span>
        <span class="conn-status" classList={{ "is-online": connected() }}>
          {connected() ? "live" : "offline"}
        </span>
      </header>
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
          </section>
          <Show when={hasVideoPane()}>
            <div
              class="pane-divider"
              role="separator"
              aria-label="Resize chat"
              aria-orientation="horizontal"
              tabIndex={0}
              onPointerDown={beginPaneResize}
              onKeyDown={onDividerKeyDown}
            />
          </Show>
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
          <div class="chat-log-spacer" />
          <div class="chat-log-content" ref={contentEl}>
            <Virtualizer
              ref={(h) => (handle = h)}
              scrollRef={logEl}
              data={messages()}
              shift={prepend()}
              onScroll={onScroll}
              onScrollEnd={onScrollEnd}
            >
              {(message) => <MessageRow message={message} />}
            </Virtualizer>
          </div>
        </div>
      </main>
    </div>
  );
}
