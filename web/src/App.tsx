import { createSignal, createEffect, onCleanup, onMount, Show } from "solid-js";
import { Virtualizer, type VirtualizerHandle } from "./vendor/virtua/solid/Virtualizer";
import type { WebMessage, ServerEnvelope, ClientRequest } from "./types";

// Pixel tolerance when deciding the view is "at the bottom". Scroll positions
// are fractional, so an exact comparison would intermittently read as
// not-at-bottom right after a programmatic scroll.
const BOTTOM_EPSILON = 4;

// Distance from the top, in pixels, at which scrolling up requests older history.
const TOP_THRESHOLD = 200;

// How many older messages one paging request asks for.
const PAGE = 100;

// Builds the asset URL for an attachment served from the client's receive dir.
function fileUrl(name: string): string {
  return `/files/${encodeURIComponent(name)}`;
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

  let logEl: HTMLDivElement | undefined;
  let contentEl: HTMLDivElement | undefined;
  let handle: VirtualizerHandle | undefined;
  let resizeObserver: ResizeObserver | undefined;
  let socket: WebSocket | undefined;
  let reconnectTimer: number | undefined;

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
    ws.onopen = () => setConnected(true);
    ws.onmessage = (ev) => {
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
      } else {
        // Upsert by file_id: a file's announcement placeholder and its later
        // inline version share an id, so the second arrival enriches the first
        // in place rather than appearing as a separate message.
        const msg = env.message;
        setMessages((prev) => {
          if (msg.file_id !== null) {
            const i = prev.findIndex((m) => m.file_id === msg.file_id);
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
    connect();
  });
  onCleanup(() => {
    resizeObserver?.disconnect();
    if (reconnectTimer) clearTimeout(reconnectTimer);
    if (suppressTimer) clearTimeout(suppressTimer);
    if (idleTimer) clearTimeout(idleTimer);
    socket?.close();
  });

  return (
    <div class="app">
      <header class="app-header">
        <span class="app-title">chatt</span>
        <span class="conn-status" classList={{ "is-online": connected() }}>
          {connected() ? "live" : "offline"}
        </span>
      </header>
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
    </div>
  );
}
