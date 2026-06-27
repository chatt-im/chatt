import { createSignal, onCleanup, onMount, For, Show } from "solid-js";
import type { WebMessage, ServerEnvelope } from "./types";

// Pixel tolerance when deciding the view is "at the bottom". Scroll positions
// are fractional, so an exact comparison would intermittently read as
// not-at-bottom right after a programmatic scroll.
const BOTTOM_EPSILON = 4;

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
  return (
    <div class="message-media">
      <Show when={att().kind === "image"}>
        <img class="media-image" src={url()} alt={att().name} />
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

export default function App() {
  const [messages, setMessages] = createSignal<WebMessage[]>([]);
  const [connected, setConnected] = createSignal(false);
  let logEl: HTMLDivElement | undefined;
  let contentEl: HTMLDivElement | undefined;
  let resizeObserver: ResizeObserver | undefined;
  let socket: WebSocket | undefined;
  let reconnectTimer: number | undefined;

  // HARD REQUIREMENT: while following, the view MUST stay glued to the newest
  // message. It must NOT break on async layout growth (an image or video
  // finishing load and resizing after the message arrives). See docs/web.md.
  //
  // The earlier design derived the follow state from the scroll position on
  // every scroll event. That has a terminal-failure mode: media that loads and
  // grows the layout *after* the scroll leaves the view above the bottom, and
  // the next scroll event latches the follow flag to false. Autoscroll then
  // stays dead until the user manually scrolls back down. Two rules prevent it:
  //   1. `following` flips ONLY on a genuine user scroll, never from a
  //      programmatic scroll or a layout change.
  //   2. EVERY content-size change re-pins (ResizeObserver), so any media that
  //      grows later is followed, not stranded.
  let following = true;
  // Set while we drive scrollTop ourselves so onScroll ignores the event our
  // own scroll produces and never reinterprets it as user intent.
  let suppressScroll = false;

  function atBottom(): boolean {
    if (!logEl) return true;
    return (
      logEl.scrollHeight - logEl.scrollTop - logEl.clientHeight <= BOTTOM_EPSILON
    );
  }

  // Re-pin to the newest message. Called after every content growth. Safe to
  // over-call. A no-op while the user has scrolled away.
  function pin() {
    if (!logEl || !following) return;
    suppressScroll = true;
    logEl.scrollTop = logEl.scrollHeight;
    requestAnimationFrame(() => {
      suppressScroll = false;
    });
  }

  function onScroll() {
    // Ignore the scroll our own pin() produced and any layout-driven event
    // around it. Only a real user scroll updates the follow intent.
    if (suppressScroll) return;
    following = atBottom();
  }

  function connect() {
    const ws = new WebSocket(`ws://${location.host}/ws`);
    ws.onopen = () => setConnected(true);
    ws.onmessage = (ev) => {
      const env: ServerEnvelope = JSON.parse(ev.data);
      if (env.type === "sync") {
        setMessages(env.messages);
        following = true;
      } else {
        setMessages((prev) => [...prev, env.message]);
      }
      // The ResizeObserver also pins on the resulting growth; this just makes
      // text-only messages snap without waiting for the observer callback.
      queueMicrotask(pin);
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
      // Fires on any content-size change: new message, image/video decode,
      // font load, reflow. This is what makes media autoscroll reliable rather
      // than racing per-element load events.
      resizeObserver = new ResizeObserver(() => pin());
      resizeObserver.observe(contentEl);
    }
    connect();
  });
  onCleanup(() => {
    resizeObserver?.disconnect();
    if (reconnectTimer) clearTimeout(reconnectTimer);
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
      <div class="chat-log" ref={logEl} onScroll={onScroll}>
        <div class="chat-log-content" ref={contentEl}>
          <For each={messages()}>
            {(message) => (
              <div class="message">
                <div class="message-meta">
                  <span class="message-sender">{message.sender}</span>
                  <span class="message-time">
                    {formatTime(message.timestamp_ms)}
                  </span>
                </div>
                <Show when={message.body}>
                  <div class="message-body">{message.body}</div>
                </Show>
                <Show when={message.attachment}>
                  <Attachment message={message} />
                </Show>
              </div>
            )}
          </For>
        </div>
      </div>
    </div>
  );
}
