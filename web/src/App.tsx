import {
  batch,
  createMemo,
  createSignal,
  createEffect,
  onCleanup,
  onMount,
  For,
  Show,
} from "solid-js";
import {
  Virtualizer,
  type VirtualizerHandle,
} from "./vendor/virtua/solid/Virtualizer";
import type {
  AutoplayMode,
  WebMessage,
  ServerEnvelope,
  ClientRequest,
  ShareInfo,
  Fragment,
} from "./types";
import ScreenShare from "./ScreenShare";
import { ScreenShareDecoder, parseFrame } from "./video-decode";
import { renderInline } from "./highlight";
import { decodeFeed } from "./feed";
import { markImageError, markImageLoaded } from "./image-cache";
import Icon, { IconSprite } from "./Icon";
import PreviewPanel, { previewKey, type PreviewItem } from "./PreviewPanel";
import VideoPlayer from "./VideoPlayer";

// Pixel tolerance when deciding the view is "at the bottom". Scroll positions
// are fractional, so an exact comparison would intermittently read as
// not-at-bottom right after a programmatic scroll.
const BOTTOM_EPSILON = 4;

// Distance from the top, in pixels, at which scrolling up requests older history.
const TOP_THRESHOLD = 200;

// How many older messages one paging request asks for.
const PAGE = 100;

// Size of each file-upload message. Kept well under the server's payload cap;
// the server also accepts webviews that fragment the WebSocket frame on the wire.
const UPLOAD_CHUNK_BYTES = 256 * 1024;
const UPLOAD_MAX_BUFFERED_BYTES = 1024 * 1024;
const UPLOAD_DRAIN_POLL_MS = 10;

// Warm a small number of image attachments from the edge of each message batch.
// This keeps near-viewport images responsive without fetching the whole sync or
// history page when a room contains many images.
const IMAGE_PRELOAD_BATCH_LIMIT = 6;
const IMAGE_PRELOAD_SCAN_LIMIT = 24;
const IMAGE_PRELOAD_CACHE_LIMIT = 32;
const PREVIEW_HISTORY_LIMIT = 16;

// Consecutive messages from one sender within this window form a group:
// only the first carries the sender/time header (Discord-style).
const GROUP_WINDOW_MS = 5 * 60 * 1000;

type MessageGroupInfo = {
  key: string;
  continuation: boolean;
  messageCount: number;
  collapsed: boolean;
};

type MessageList = {
  visible: WebMessage[];
  groups: Map<WebMessage, MessageGroupInfo>;
};

function isMessageContinuation(
  message: WebMessage,
  previous: WebMessage | undefined
): boolean {
  return (
    !!previous &&
    previous.sender === message.sender &&
    message.timestamp_ms - previous.timestamp_ms < GROUP_WINDOW_MS
  );
}

function messageGroupKey(message: WebMessage): string {
  return `${message.timestamp_ms}:${message.message_id}:${message.id}`;
}

// Projects the flat feed into header-led sender groups. A collapsed group keeps
// one compact header row in the virtualizer and omits every message body.
function buildMessageList(
  messages: readonly WebMessage[],
  collapsedGroups: ReadonlySet<string>
): MessageList {
  const visible: WebMessage[] = [];
  const groups = new Map<WebMessage, MessageGroupInfo>();

  for (let start = 0; start < messages.length; ) {
    let end = start + 1;
    while (
      end < messages.length &&
      isMessageContinuation(messages[end]!, messages[end - 1])
    ) {
      end++;
    }

    const header = messages[start]!;
    const key = messageGroupKey(header);
    const messageCount = end - start;
    const collapsed = collapsedGroups.has(key);
    visible.push(header);
    groups.set(header, {
      key,
      continuation: false,
      messageCount,
      collapsed,
    });

    for (let index = start + 1; index < end; index++) {
      const child = messages[index]!;
      groups.set(child, {
        key,
        continuation: true,
        messageCount,
        collapsed,
      });
      if (!collapsed) visible.push(child);
    }
    start = end;
  }

  return { visible, groups };
}

// Do not flash a connection error while the initial WebSocket handshake (or a
// quick reconnect) is still in progress.
const CONNECTION_ERROR_DELAY_MS = 3_000;

type ImagePreload = {
  image: HTMLImageElement;
};

const DEFAULT_SHARE_PANE_HEIGHT = 360;
const MIN_SHARE_PANE_HEIGHT = 160;
const MIN_CHAT_PANE_HEIGHT = 140;
const DIVIDER_SIZE = 9;
const PANE_KEY_STEP = 32;
const DEFAULT_PREVIEW_PANEL_WIDTH = 560;
const MIN_PREVIEW_PANEL_WIDTH = 320;
const MIN_CHAT_SPLIT_WIDTH = 320;
const PREVIEW_PANEL_DIVIDER_SIZE = 3;
const PREVIEW_PANEL_KEY_STEP = 32;
const CHAT_END_MARGIN_PX = 8;

// Builds the asset URL for an attachment served from the client's receive dir.
function fileUrl(name: string): string {
  return `/files/${encodeURIComponent(name)}`;
}

function previewKind(value: string | undefined): PreviewItem["kind"] | null {
  switch (value) {
    case "image":
    case "video":
    case "audio":
    case "file":
      return value;
    default:
      return null;
  }
}

function optionalDataNumber(value: string | undefined): number | null {
  if (!value) return null;
  const parsed = Number(value);
  return Number.isFinite(parsed) ? parsed : null;
}

function previewItemFromRef(anchor: HTMLElement): PreviewItem | null {
  const name = anchor.dataset.mediaName;
  const kind = previewKind(anchor.dataset.mediaKind);
  if (!name || !kind) return null;
  if (kind === "image") {
    return {
      kind,
      name,
      width: optionalDataNumber(anchor.dataset.mediaWidth),
      height: optionalDataNumber(anchor.dataset.mediaHeight),
    };
  }
  return { kind, name };
}

function standalonePreviewFromLocation(): {
  item: PreviewItem;
  autoplay: AutoplayMode;
} | null {
  const params = new URLSearchParams(location.search);
  const kind = previewKind(params.get("preview") ?? undefined);
  const name = params.get("name");
  if (!kind || !name) return null;

  const autoplayValue = params.get("autoplay");
  const autoplay: AutoplayMode =
    autoplayValue === "muted" || autoplayValue === "with-audio"
      ? autoplayValue
      : "disabled";
  if (kind !== "image") return { item: { kind, name }, autoplay };

  return {
    item: {
      kind,
      name,
      width: optionalDataNumber(params.get("width") ?? undefined),
      height: optionalDataNumber(params.get("height") ?? undefined),
    },
    autoplay,
  };
}

function standalonePreviewUrl(
  item: PreviewItem,
  autoplay: AutoplayMode,
): string {
  const url = new URL("/", location.href);
  url.searchParams.set("preview", item.kind);
  url.searchParams.set("name", item.name);
  url.searchParams.set("autoplay", autoplay);
  if (item.kind === "image") {
    if (item.width !== null) url.searchParams.set("width", String(item.width));
    if (item.height !== null)
      url.searchParams.set("height", String(item.height));
  }
  return url.href;
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

function debugFlagEnabled(queryParam: string, storageKey: string): boolean {
  if (typeof location === "undefined") return false;
  if (new URLSearchParams(location.search).has(queryParam)) return true;
  try {
    return localStorage.getItem(storageKey) === "1";
  } catch {
    return false;
  }
}

function uploadDebugEnabled(): boolean {
  return debugFlagEnabled("debugUpload", "chatt.debugUpload");
}

function socketDebugEnabled(): boolean {
  return (
    debugFlagEnabled("debugSocket", "chatt.debugSocket") || uploadDebugEnabled()
  );
}

function debugImageTiming(stage: string, name: string, url: string) {
  if (!imageDebugEnabled() || typeof performance === "undefined") return;
  const href = new URL(url, location.href).href;
  const entries = performance.getEntriesByName(href);
  const timing = entries[entries.length - 1] as
    | PerformanceResourceTiming
    | undefined;
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

function debugUpload(stage: string, fields: Record<string, unknown>) {
  if (!uploadDebugEnabled()) return;
  console.debug("[chatt:upload]", {
    stage,
    t:
      typeof performance === "undefined"
        ? undefined
        : Math.round(performance.now() * 10) / 10,
    ...fields,
  });
}

function debugSocket(stage: string, fields: Record<string, unknown> = {}) {
  if (!socketDebugEnabled()) return;
  console.debug("[chatt:ws]", {
    stage,
    t:
      typeof performance === "undefined"
        ? undefined
        : Math.round(performance.now() * 10) / 10,
    ...fields,
  });
}

function delay(ms: number): Promise<void> {
  return new Promise((resolve) => window.setTimeout(resolve, ms));
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

function imageExtension(type: string): string {
  const subtype = type.split("/")[1]?.split(";")[0]?.toLowerCase() ?? "";
  switch (subtype) {
    case "jpeg":
    case "pjpeg":
      return "jpg";
    case "svg+xml":
      return "svg";
    case "png":
    case "gif":
    case "webp":
    case "bmp":
    case "avif":
    case "heic":
    case "heif":
      return subtype;
    default:
      return subtype.replace(/[^a-z0-9]+/g, "") || "png";
  }
}

function timestampForFileName(date: Date): string {
  return date
    .toISOString()
    .replace(/\.\d{3}Z$/, "Z")
    .replace(/[:.]/g, "-");
}

function withPastedImageName(file: File, pastedAt: Date, index: number): File {
  const name = file.name.trim();
  if (
    name &&
    !/^image\.(?:png|jpe?g|gif|webp|bmp|svg|avif|heic|heif)$/i.test(name)
  ) {
    return file;
  }

  const suffix = index === 0 ? "" : `-${index + 1}`;
  return new File(
    [file],
    `pasted-image-${timestampForFileName(pastedAt)}${suffix}.${imageExtension(
      file.type
    )}`,
    {
      type: file.type,
      lastModified: file.lastModified,
    }
  );
}

// Progress bar shown on a file's placeholder message while the host client
// pulls the file off the relay. Replaced by the attachment on completion.
function TransferProgressBar(props: {
  progress: { transferred: number; total: number };
}) {
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
        receiving {formatBytes(props.progress.transferred)} /{" "}
        {formatBytes(props.progress.total)} ({pct()}%)
      </span>
    </div>
  );
}

// The virtualizer unmounts rows that scroll out of the window, so a fragment's
// body HTML would otherwise be re-read every time its row scrolls back in. A
// fragment object is created once per decoded message and never mutated
// (progress merges keep the same fragments array), so it is a stable cache
// key; replaced messages simply fall out with GC.
const fragmentHtmlCache = new WeakMap<Fragment, string>();
type CodeFragment = Extract<Fragment, { kind: "code" }>;
const codeTextDecoder = new TextDecoder();

function fragmentHtml(fragment: Fragment): string {
  let html = fragmentHtmlCache.get(fragment);
  if (html === undefined) {
    html =
      fragment.kind === "text"
        ? fragment.html
        : renderInline(fragment.text, fragment.spans);
    fragmentHtmlCache.set(fragment, html);
  }
  return html;
}

function codeFragmentText(fragment: CodeFragment): string {
  return codeTextDecoder.decode(fragment.text);
}

async function copyTextToClipboard(text: string): Promise<void> {
  if (navigator.clipboard?.writeText && window.isSecureContext) {
    await navigator.clipboard.writeText(text);
    return;
  }

  const previousSelection = document.getSelection();
  const previousRange =
    previousSelection && previousSelection.rangeCount > 0
      ? previousSelection.getRangeAt(0)
      : null;
  const textarea = document.createElement("textarea");
  textarea.value = text;
  textarea.setAttribute("readonly", "");
  textarea.style.position = "fixed";
  textarea.style.top = "0";
  textarea.style.left = "-9999px";
  document.body.append(textarea);
  textarea.select();
  const copied = document.execCommand("copy");
  textarea.remove();

  if (previousSelection) {
    previousSelection.removeAllRanges();
    if (previousRange) previousSelection.addRange(previousRange);
  }

  if (!copied) throw new Error("copy command was rejected");
}

function CodeBlock(props: { fragment: CodeFragment }) {
  const [copied, setCopied] = createSignal(false);
  let resetTimer: number | undefined;

  onCleanup(() => {
    if (resetTimer !== undefined) clearTimeout(resetTimer);
  });

  async function copyCode() {
    try {
      await copyTextToClipboard(codeFragmentText(props.fragment));
      setCopied(true);
      if (resetTimer !== undefined) clearTimeout(resetTimer);
      resetTimer = window.setTimeout(() => {
        setCopied(false);
        resetTimer = undefined;
      }, 1500);
    } catch (error) {
      console.warn("[chatt:clipboard] copy failed", error);
    }
  }

  return (
    <div class="code-block-frame">
      <pre class="code-block">
        <code innerHTML={fragmentHtml(props.fragment)} />
      </pre>
      <button
        class="code-block-copy"
        type="button"
        aria-label={copied() ? "Copied code" : "Copy code"}
        title={copied() ? "Copied" : "Copy"}
        onClick={copyCode}
      >
        <Icon name={copied() ? "check" : "copy"} />
      </button>
    </div>
  );
}

function MessageFragment(props: { fragment: Fragment }) {
  const content = () =>
    props.fragment.kind === "text" ? (
      <div class="message-body" innerHTML={fragmentHtml(props.fragment)} />
    ) : (
      <CodeBlock fragment={props.fragment} />
    );

  if (props.fragment.quote_depth === 0) return content();
  return (
    <blockquote class="message-quote">
      <span class="message-quote-markers" aria-hidden="true">
        {"> ".repeat(props.fragment.quote_depth)}
      </span>
      <div class="message-quote-content">{content()}</div>
    </blockquote>
  );
}

// Renders a message body from Rust-produced subset HTML and precomputed code
// highlight spans. Nothing is parsed or highlighted in the browser.
function MessageBody(props: { fragments: Fragment[] }) {
  return (
    <For each={props.fragments}>
      {(fragment) => <MessageFragment fragment={fragment} />}
    </For>
  );
}

function Attachment(props: {
  message: WebMessage;
  onOpenPreview: (item: PreviewItem, opener: HTMLElement) => void;
  autoplay: AutoplayMode;
}) {
  const att = () => props.message.attachment!;
  const url = () => fileUrl(att().name);
  // Fades the image in on decode instead of snapping. The box is already
  // reserved by width/height, so this only affects the pixels, never layout.
  const [loaded, setLoaded] = createSignal(false);
  onMount(() => {
    if (att().kind === "image")
      debugImageTiming("img:mount", att().name, url());
  });
  return (
    <div class="message-media">
      <Show when={att().kind === "image"}>
        <a
          class="media-image-link"
          href={url()}
          onClick={(event) => {
            if (
              event.button !== 0 ||
              event.ctrlKey ||
              event.metaKey ||
              event.shiftKey ||
              event.altKey
            ) {
              return;
            }
            event.preventDefault();
            props.onOpenPreview(
              {
                kind: "image",
                name: att().name,
                width: att().width,
                height: att().height,
              },
              event.currentTarget
            );
          }}
        >
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
            onLoad={(event) => {
              markImageLoaded(url(), event.currentTarget);
              debugImageTiming("img:load", att().name, url());
              setLoaded(true);
            }}
            onError={() => {
              markImageError(url());
              debugImageTiming("img:error", att().name, url());
              setLoaded(true);
            }}
          />
        </a>
      </Show>
      <Show when={att().kind === "video"}>
        <VideoPlayer
          class="media-video"
          src={url()}
          autoplay={props.autoplay}
        />
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
            onClick={(event) =>
              props.onOpenPreview(
                { kind: "file", name: att().name },
                event.currentTarget
              )
            }
          >
            <Icon name="file-text" />
            <span class="media-file-name">{att().name}</span>
          </button>
          <a
            class="media-file-download"
            href={url()}
            download={att().name}
            aria-label={`Download ${att().name}`}
            title="Download"
          >
            <Icon name="download" />
          </a>
        </div>
      </Show>
    </div>
  );
}

function MessageRow(props: {
  message: WebMessage;
  group: MessageGroupInfo;
  onToggleGroup: (key: string) => void;
  onOpenPreview: (item: PreviewItem, opener: HTMLElement) => void;
  onQuoteRef?: (refCode: string) => void;
  autoplay: AutoplayMode;
}) {
  // A continuation hides the header and shows its time only on hover, in the
  // reserved left gutter. Group metadata is projected reactively from the full
  // feed so prepended history can still change the boundary row's grouping.
  const continuation = () => props.group.continuation;
  const groupLabel = () => {
    const action = props.group.collapsed ? "Expand" : "Collapse";
    const noun = props.group.messageCount === 1 ? "message" : "messages";
    return `${action} ${props.group.messageCount} ${noun} from ${props.message.sender}`;
  };
  const [refCopied, setRefCopied] = createSignal(false);
  let refCopyResetTimer: number | undefined;
  onCleanup(() => {
    if (refCopyResetTimer !== undefined) clearTimeout(refCopyResetTimer);
  });
  async function copyRef() {
    try {
      await copyTextToClipboard(`@@${props.message.ref_code}`);
      setRefCopied(true);
      if (refCopyResetTimer !== undefined) clearTimeout(refCopyResetTimer);
      refCopyResetTimer = window.setTimeout(() => {
        setRefCopied(false);
        refCopyResetTimer = undefined;
      }, 1500);
    } catch (error) {
      console.warn("[chatt:clipboard] reference copy failed", error);
    }
  }
  return (
    <div
      class="message"
      classList={{
        "is-continuation": continuation(),
        "is-group-collapsed": props.group.collapsed,
      }}
      data-ts={props.message.timestamp_ms}
      data-mid={props.message.message_id}
      onClick={() => {
        if (props.group.collapsed) props.onToggleGroup(props.group.key);
      }}
    >
      {/* The time always lives in the left gutter so it sits in one consistent
       * column: shown on a group's first row, revealed on hover for the rest. */}
      <span class="message-time-gutter">
        {formatTime(props.message.timestamp_ms)}
      </span>
      <Show when={!continuation()}>
        <div class="message-meta">
          <span class="message-sender">{props.message.sender}</span>
          <Show when={props.group.collapsed}>
            <span class="message-group-summary">
              {props.group.messageCount}{" "}
              {props.group.messageCount === 1 ? "message" : "messages"} collapsed
            </span>
          </Show>
        </div>
        <button
          class="message-group-toggle"
          type="button"
          aria-expanded={!props.group.collapsed}
          aria-label={groupLabel()}
          title={
            props.group.collapsed
              ? "Expand message group"
              : "Collapse message group"
          }
          onClick={(event) => {
            event.stopPropagation();
            props.onToggleGroup(props.group.key);
          }}
        >
          <Icon
            name={
              props.group.collapsed
                ? "list-chevrons-up-down"
                : "list-chevrons-down-up"
            }
          />
        </button>
      </Show>
      <Show when={!props.group.collapsed}>
        <MessageBody fragments={props.message.fragments} />
        <Show when={props.message.attachment}>
          <Attachment
            message={props.message}
            onOpenPreview={props.onOpenPreview}
            autoplay={props.autoplay}
          />
        </Show>
        <Show when={!props.message.attachment && props.message.progress}>
          <TransferProgressBar progress={props.message.progress!} />
        </Show>
        <Show when={props.message.ref_code}>
          <div class="message-actions">
            <Show when={props.onQuoteRef}>
              <button
                class="message-action"
                type="button"
                aria-label="Quote message"
                title="Quote"
                onClick={() => props.onQuoteRef!(props.message.ref_code)}
              >
                <Icon name="corner-up-left" />
              </button>
            </Show>
            <button
              class="message-action"
              type="button"
              aria-label={refCopied() ? "Copied reference" : "Copy reference"}
              title={refCopied() ? "Copied" : "Copy reference"}
              onClick={copyRef}
            >
              <Icon name={refCopied() ? "check" : "at-sign"} />
            </button>
          </div>
        </Show>
      </Show>
    </div>
  );
}

export default function App() {
  const standalone = standalonePreviewFromLocation();
  if (standalone) {
    const key = previewKey(standalone.item);
    document.title = `${standalone.item.name} — chatt`;
    return (
      <div class="app">
        <IconSprite />
        <PreviewPanel
          history={[standalone.item]}
          active={standalone.item}
          activeKey={key}
          onSelect={() => {}}
          onClose={() => window.close()}
          onCloseTab={() => window.close()}
          autoplay={standalone.autoplay}
          standalone
        />
      </div>
    );
  }

  const [messages, setMessages] = createSignal<WebMessage[]>([]);
  const [collapsedGroups, setCollapsedGroups] = createSignal<
    ReadonlySet<string>
  >(new Set());
  const messageList = createMemo(() =>
    buildMessageList(messages(), collapsedGroups())
  );
  const [refToast, setRefToast] = createSignal<string | null>(null);
  const [connected, setConnected] = createSignal(false);
  const [connectionErrorVisible, setConnectionErrorVisible] =
    createSignal(false);
  // Drives virtua's `shift`: while true a data change is treated as a prepend so
  // scroll position is anchored from the end (reverse infinite scroll).
  const [prepend, setPrepend] = createSignal(false);

  // Screen shares this browser can watch, and the stream ids currently playing.
  const [shares, setShares] = createSignal<ShareInfo[]>([]);
  const [playing, setPlaying] = createSignal<number[]>([]);
  // Per-stream play-failure messages reported by the client, shown on the row.
  const [shareErrors, setShareErrors] = createSignal<Record<number, string>>(
    {}
  );
  const [sharePaneHeight, setSharePaneHeight] = createSignal(
    DEFAULT_SHARE_PANE_HEIGHT
  );
  const [fullscreenStream, setFullscreenStream] = createSignal<number | null>(
    null
  );
  // Direct opens promote an item to the front of this bounded history. Closing
  // the panel clears only the active key, so history survives until reload.
  const [previewHistory, setPreviewHistory] = createSignal<PreviewItem[]>([]);
  const [activePreviewKey, setActivePreviewKey] = createSignal<string | null>(
    null
  );
  const [previewPanelWidth, setPreviewPanelWidth] = createSignal(
    DEFAULT_PREVIEW_PANEL_WIDTH
  );
  const [previewPanelResizing, setPreviewPanelResizing] = createSignal(false);
  const [autoplay, setAutoplay] =
    createSignal<AutoplayMode>("disabled");
  const [viewerInSeparateBrowserTab, setViewerInSeparateBrowserTab] =
    createSignal(false);
  let previewOpener: HTMLElement | undefined;

  const activePreview = () => {
    const key = activePreviewKey();
    return key
      ? previewHistory().find((item) => previewKey(item) === key) ?? null
      : null;
  };

  function openPreview(item: PreviewItem, opener: HTMLElement) {
    if (viewerInSeparateBrowserTab()) {
      window.open(standalonePreviewUrl(item, autoplay()), "_blank", "noopener");
      return;
    }

    const key = previewKey(item);
    previewOpener = opener;
    batch(() => {
      setPreviewHistory((current) => {
        const existing = current.find(
          (candidate) => previewKey(candidate) === key
        );
        const nextItem = existing ?? item;
        if (current[0] === nextItem) return current;
        return [
          nextItem,
          ...current.filter((candidate) => previewKey(candidate) !== key),
        ].slice(0, PREVIEW_HISTORY_LIMIT);
      });
      setActivePreviewKey(key);
    });
  }

  function closePreview() {
    setActivePreviewKey(null);
    queueMicrotask(() => {
      if (previewOpener?.isConnected)
        previewOpener.focus({ preventScroll: true });
    });
  }

  function closePreviewTab(key: string) {
    const current = previewHistory();
    const index = current.findIndex((item) => previewKey(item) === key);
    if (index < 0) return;

    const next = current.filter((item) => previewKey(item) !== key);
    batch(() => {
      setPreviewHistory(next);
      if (activePreviewKey() !== key) return;

      const replacement = next[index] ?? next[index - 1] ?? null;
      if (replacement) setActivePreviewKey(previewKey(replacement));
      else closePreview();
    });
  }

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
  let composerFileInputEl: HTMLInputElement | undefined;
  let handle: VirtualizerHandle | undefined;
  let resizeObserver: ResizeObserver | undefined;
  let chatViewportResizeObserver: ResizeObserver | undefined;
  let splitResizeObserver: ResizeObserver | undefined;
  let previewSplitResizeObserver: ResizeObserver | undefined;
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
  let previewPanelResize:
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
  // True while a scroll we initiated is in flight, so onScroll ignores it.
  let suppress = false;
  let suppressTimer: number | undefined;
  let idleTimer: number | undefined;
  let refJumping = false;
  let refJumpTimer: number | undefined;
  let pendingJumpFrame: number | undefined;

  // Use virtua's measured geometry, not DOM scrollHeight: when tail items are
  // still unmeasured, totalSize is an estimate and scrollHeight can disagree.
  function atBottom(): boolean {
    if (!handle) return true;
    return (
      handle.scrollOffset >=
      handle.scrollSize - handle.viewportSize - BOTTOM_EPSILON
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

  function clearSuppressTimer() {
    if (suppressTimer) {
      clearTimeout(suppressTimer);
      suppressTimer = undefined;
    }
  }

  function suppressProgrammaticScroll(ms: number) {
    suppress = true;
    clearSuppressTimer();
    suppressTimer = window.setTimeout(() => {
      suppress = false;
      suppressTimer = undefined;
    }, ms);
  }

  function clearRefJumpTimer() {
    if (refJumpTimer) {
      clearTimeout(refJumpTimer);
      refJumpTimer = undefined;
    }
  }

  function clearRefJumpFrame() {
    if (pendingJumpFrame) {
      cancelAnimationFrame(pendingJumpFrame);
      pendingJumpFrame = undefined;
    }
  }

  function holdRefJump(ms: number) {
    refJumping = true;
    clearRefJumpTimer();
    refJumpTimer = window.setTimeout(() => {
      refJumping = false;
      refJumpTimer = undefined;
    }, ms);
  }

  function detachForRefJump() {
    following = false;
    userDriving = false;
    suppress = false;
    clearSuppressTimer();
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
    if (previewPanelResize || !handle || refJumping || !following) return;
    const last = messages().length - 1;
    if (last < 0) return;
    suppressProgrammaticScroll(250);
    // Scroll to the virtual bottom so the configured end margin remains visible
    // after the newest message.
    handle.scrollTo(Math.max(0, handle.scrollSize - handle.viewportSize));
    // Programmatic scrolls may emit zero scroll events when already at the
    // destination, so clearing `suppress` from onScrollEnd would deadlock it.
    // Always clear on a timer that outlives the measurement window.
  }

  function preloadImage(message: WebMessage): boolean {
    const att = message.attachment;
    if (!att || att.kind !== "image") return false;

    const url = fileUrl(att.name);
    if (imagePreloads.has(url)) return false;

    const img = new Image(att.width ?? undefined, att.height ?? undefined);
    img.decoding = "async";
    img.loading = "eager";
    img.fetchPriority = "auto";
    img.addEventListener(
      "load",
      () => {
        markImageLoaded(url, img);
        debugImageTiming("preload:load", att.name, url);
      },
      { once: true }
    );
    img.addEventListener(
      "error",
      () => {
        markImageError(url);
        debugImageTiming("preload:error", att.name, url);
      },
      { once: true }
    );
    imagePreloads.set(url, { image: img });
    img.src = url;

    while (imagePreloads.size > IMAGE_PRELOAD_CACHE_LIMIT) {
      const oldest = imagePreloads.keys().next().value;
      if (oldest === undefined) break;
      imagePreloads.delete(oldest);
    }
    return true;
  }

  function preloadRecentImages(batch: readonly WebMessage[]) {
    let started = 0;
    let scanned = 0;
    for (
      let i = batch.length - 1;
      i >= 0 &&
      started < IMAGE_PRELOAD_BATCH_LIMIT &&
      scanned < IMAGE_PRELOAD_SCAN_LIMIT;
      i--
    ) {
      scanned++;
      if (preloadImage(batch[i]!)) started++;
    }
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

  // A click on a `@@` reference jumps to its target, paging older history in
  // until the target loads. Capped so a reference outside this room's history
  // does not drain the whole backlog.
  const MAX_JUMP_PAGES = 10;
  let pendingJump: { ts: number; mid: number; tries: number } | undefined;
  let refToastTimer: number | undefined;

  function showRefToast(text: string) {
    setRefToast(text);
    if (refToastTimer) clearTimeout(refToastTimer);
    refToastTimer = window.setTimeout(() => setRefToast(null), 2500);
  }

  function findMessageIndex(ts: number, mid: number): number {
    return messages().findIndex(
      (m) => m.message_id === mid && m.timestamp_ms === ts
    );
  }

  function toggleMessageGroup(key: string) {
    setCollapsedGroups((current) => {
      const next = new Set(current);
      if (next.has(key)) next.delete(key);
      else next.add(key);
      return next;
    });
  }

  function flashMessage(ts: number, mid: number) {
    const row = logEl?.querySelector(
      `.message[data-ts="${ts}"][data-mid="${mid}"]`
    );
    if (!(row instanceof HTMLElement)) return;
    row.classList.remove("msg-flash");
    // Force a reflow so a repeated jump restarts the animation.
    void row.offsetWidth;
    row.classList.add("msg-flash");
    window.setTimeout(() => row.classList.remove("msg-flash"), 1600);
  }

  function scrollToMessage(index: number, ts: number, mid: number) {
    detachForRefJump();
    holdRefJump(1200);
    suppressProgrammaticScroll(1200);
    const target = messages()[index];
    if (!target) return;

    const group = messageList().groups.get(target);
    const groupKeyToExpand = group?.collapsed ? group.key : null;
    if (groupKeyToExpand) {
      setCollapsedGroups((current) => {
        const next = new Set(current);
        next.delete(groupKeyToExpand);
        return next;
      });
    }

    const scroll = () => {
      const visibleIndex = messageList().visible.indexOf(target);
      if (visibleIndex < 0) return;
      handle?.scrollToIndex(visibleIndex, { align: "center" });
      // The row mounts as the virtualizer approaches it; flash once it settles.
      window.setTimeout(() => flashMessage(ts, mid), 350);
    };

    // Let the virtualizer register restored child rows before addressing one.
    if (groupKeyToExpand) requestAnimationFrame(scroll);
    else scroll();
  }

  function jumpToRef(ts: number, mid: number) {
    detachForRefJump();
    holdRefJump(2000);
    const index = findMessageIndex(ts, mid);
    if (index >= 0) {
      pendingJump = undefined;
      scrollToMessage(index, ts, mid);
      return;
    }
    if (hasMore) {
      if (!pendingJump || pendingJump.ts !== ts || pendingJump.mid !== mid) {
        pendingJump = { ts, mid, tries: 0 };
      }
      requestOlder();
      return;
    }
    pendingJump = undefined;
    showRefToast("Referenced message isn't in the loaded history");
  }

  function scheduleResumePendingJump() {
    clearRefJumpFrame();
    pendingJumpFrame = requestAnimationFrame(() => {
      pendingJumpFrame = requestAnimationFrame(() => {
        pendingJumpFrame = undefined;
        resumePendingJump();
      });
    });
  }

  // Called after an `older` page lands: keep driving a jump that is still
  // waiting for its target to load.
  function resumePendingJump() {
    if (!pendingJump) return;
    holdRefJump(2000);
    const { ts, mid } = pendingJump;
    const index = findMessageIndex(ts, mid);
    if (index >= 0) {
      pendingJump = undefined;
      scrollToMessage(index, ts, mid);
      return;
    }
    pendingJump.tries += 1;
    if (pendingJump.tries >= MAX_JUMP_PAGES || !hasMore) {
      pendingJump = undefined;
      showRefToast("Referenced message isn't in the loaded history");
      return;
    }
    requestOlder();
  }

  // Splices a `@@code ` reference into the composer draft, from a message row's
  // quote button.
  function quoteRef(refCode: string) {
    const code = `@@${refCode} `;
    setDraft((prev) =>
      prev.length > 0 && !prev.endsWith(" ") ? `${prev} ${code}` : prev + code
    );
    composerTextEl?.focus();
    resizeComposer();
  }

  // Reference anchors live inside Rust-rendered fragment HTML. Media references
  // include backend-filled preview metadata; shift-click keeps the jump action.
  function onLogClick(event: MouseEvent) {
    const target = event.target as HTMLElement;
    const anchor = target.closest?.("a.msg-ref");
    if (!(anchor instanceof HTMLElement)) return;
    event.preventDefault();
    if (!event.shiftKey) {
      const preview = previewItemFromRef(anchor);
      if (preview) {
        openPreview(preview, anchor);
        return;
      }
    }
    const ts = Number(anchor.dataset.ts);
    const mid = Number(anchor.dataset.mid);
    if (!Number.isFinite(ts) || !Number.isFinite(mid)) return;
    jumpToRef(ts, mid);
  }

  function onLogPointerDown(event: PointerEvent) {
    const target = event.target;
    if (target instanceof Element && target.closest("a.msg-ref")) return;
    if (target !== logEl) return;
    markUser();
  }

  function onLogTouchStart(event: TouchEvent) {
    const target = event.target;
    if (target instanceof Element && target.closest("a.msg-ref")) return;
    markUser();
  }

  function playShare(streamId: number) {
    if (socket && socket.readyState === WebSocket.OPEN) {
      clearShareError(streamId);
      socket.send(
        JSON.stringify({
          type: "play_share",
          stream_id: streamId,
        } as ClientRequest)
      );
    }
  }

  function exitShareFullscreen() {
    setFullscreenStream(null);
    if (document.fullscreenElement === mainEl)
      document.exitFullscreen().catch(() => {});
  }

  function stopShare(streamId: number) {
    if (socket && socket.readyState === WebSocket.OPEN) {
      socket.send(
        JSON.stringify({
          type: "stop_share",
          stream_id: streamId,
        } as ClientRequest)
      );
    }
    if (fullscreenStream() === streamId) exitShareFullscreen();
    closeDecoder(streamId);
  }

  function sendJson(req: ClientRequest) {
    if (socket && socket.readyState === WebSocket.OPEN) {
      socket.send(JSON.stringify(req));
    }
  }

  function openSocketOrThrow(): WebSocket {
    if (!socket || socket.readyState !== WebSocket.OPEN) {
      throw new Error("websocket is not open");
    }
    return socket;
  }

  async function waitForUploadDrain(uploadId: number) {
    let waiting = false;
    while (true) {
      const ws = openSocketOrThrow();
      if (ws.bufferedAmount <= UPLOAD_MAX_BUFFERED_BYTES) {
        if (waiting) {
          debugUpload("buffer_drained", {
            upload_id: uploadId,
            buffered_amount: ws.bufferedAmount,
          });
        }
        return;
      }
      if (!waiting) {
        waiting = true;
        debugUpload("buffer_wait", {
          upload_id: uploadId,
          buffered_amount: ws.bufferedAmount,
          max_buffered_bytes: UPLOAD_MAX_BUFFERED_BYTES,
        });
      }
      await delay(UPLOAD_DRAIN_POLL_MS);
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
      Number.isFinite(maxHeight) && textArea.scrollHeight > maxHeight
        ? "auto"
        : "hidden";
  }

  // Streams one queued file to the client: an `upload_start`, then binary chunks
  // each prefixed with the little-endian upload id, then `upload_finish`. The
  // server reassembles them into a temp file and relays it as a normal upload.
  async function sendFile(file: File) {
    const ws = openSocketOrThrow();
    const uploadId = nextUploadId++;
    debugUpload("start", {
      upload_id: uploadId,
      name: file.name,
      size: file.size,
      buffered_amount: ws.bufferedAmount,
    });
    ws.send(
      JSON.stringify({
        type: "upload_start",
        upload_id: uploadId,
        name: file.name,
        size: file.size,
      })
    );
    for (let offset = 0; offset < file.size; offset += UPLOAD_CHUNK_BYTES) {
      const end = Math.min(file.size, offset + UPLOAD_CHUNK_BYTES);
      const chunk = new Uint8Array(await file.slice(offset, end).arrayBuffer());
      const frame = new Uint8Array(4 + chunk.length);
      new DataView(frame.buffer).setUint32(0, uploadId, true);
      frame.set(chunk, 4);
      const current = openSocketOrThrow();
      current.send(frame);
      debugUpload("chunk", {
        upload_id: uploadId,
        name: file.name,
        offset,
        chunk_bytes: chunk.length,
        sent_bytes: end,
        total_bytes: file.size,
        buffered_amount: current.bufferedAmount,
      });
      await waitForUploadDrain(uploadId);
    }
    const current = openSocketOrThrow();
    current.send(
      JSON.stringify({ type: "upload_finish", upload_id: uploadId })
    );
    debugUpload("finish", {
      upload_id: uploadId,
      name: file.name,
      size: file.size,
      buffered_amount: current.bufferedAmount,
    });
  }

  async function sendQueuedFiles(files: File[]) {
    for (const file of files) {
      try {
        await sendFile(file);
      } catch (error) {
        console.warn("[chatt:upload] failed", {
          name: file.name,
          size: file.size,
          error,
        });
        debugUpload("error", {
          name: file.name,
          size: file.size,
          error: error instanceof Error ? error.message : String(error),
        });
        return;
      }
    }
  }

  function submitCompose() {
    const body = draft().trim();
    const files = queued();
    if (!body && files.length === 0) return;
    if (body) sendJson({ type: "send_message", body });
    if (files.length > 0) void sendQueuedFiles(files);
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
    const files = event.dataTransfer
      ? Array.from(event.dataTransfer.files)
      : [];
    if (files.length > 0) setQueued((prev) => [...prev, ...files]);
  }

  function openComposeFileDialog() {
    composerFileInputEl?.click();
  }

  function onComposeFileInput(
    event: Event & { currentTarget: HTMLInputElement }
  ) {
    const files = Array.from(event.currentTarget.files ?? []);
    if (files.length > 0) setQueued((prev) => [...prev, ...files]);
    event.currentTarget.value = "";
  }

  function pastedImageFiles(data: DataTransfer | null): File[] {
    if (!data) return [];

    const files: File[] = [];
    for (const item of Array.from(data.items)) {
      if (item.kind !== "file" || !item.type.startsWith("image/")) continue;
      const file = item.getAsFile();
      if (file) files.push(file);
    }

    if (files.length === 0) {
      for (const file of Array.from(data.files)) {
        if (file.type.startsWith("image/")) files.push(file);
      }
    }

    const pastedAt = new Date();
    return files.map((file, index) =>
      withPastedImageName(file, pastedAt, index)
    );
  }

  function onComposePaste(event: ClipboardEvent) {
    const files = pastedImageFiles(event.clipboardData);
    if (files.length === 0) return;
    event.preventDefault();
    setQueued((prev) => [...prev, ...files]);
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
      Math.max(96, total - MIN_CHAT_PANE_HEIGHT - DIVIDER_SIZE)
    );
    const maxShare = Math.max(
      minShare,
      total - MIN_CHAT_PANE_HEIGHT - DIVIDER_SIZE
    );
    return clamp(height, minShare, maxShare);
  }

  function setClampedSharePaneHeight(height: number) {
    setSharePaneHeight(clampSharePaneHeight(height));
    pin();
  }

  function clampPreviewPanelWidth(width: number): number {
    const total = appBodyEl?.clientWidth ?? 0;
    if (total <= 0) return Math.max(MIN_PREVIEW_PANEL_WIDTH, width);
    const minPreview = Math.min(
      MIN_PREVIEW_PANEL_WIDTH,
      Math.max(240, total - MIN_CHAT_SPLIT_WIDTH - PREVIEW_PANEL_DIVIDER_SIZE)
    );
    const maxPreview = Math.max(
      minPreview,
      total - MIN_CHAT_SPLIT_WIDTH - PREVIEW_PANEL_DIVIDER_SIZE
    );
    return clamp(width, minPreview, maxPreview);
  }

  function setClampedPreviewPanelWidth(width: number) {
    setPreviewPanelWidth(clampPreviewPanelWidth(width));
    pin();
  }

  function removePaneResizeListeners() {
    if (!paneResize) return;
    window.removeEventListener("pointermove", paneResize.move);
    window.removeEventListener("pointerup", paneResize.up);
    window.removeEventListener("pointercancel", paneResize.up);
    paneResize = undefined;
  }

  function removePreviewPanelResizeListeners() {
    if (!previewPanelResize) return;
    window.removeEventListener("pointermove", previewPanelResize.move);
    window.removeEventListener("pointerup", previewPanelResize.up);
    window.removeEventListener("pointercancel", previewPanelResize.up);
    previewPanelResize.cancelFrame();
    previewPanelResize = undefined;
    setPreviewPanelResizing(false);
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

  function beginPreviewPanelResize(event: PointerEvent) {
    event.preventDefault();
    removePreviewPanelResizeListeners();
    const startX = event.clientX;
    const startWidth = previewPanelWidth();
    // The app body's width does not change as its children are resized. Read
    // it once so pointer moves never force layout merely to recompute bounds.
    const total = appBodyEl?.clientWidth ?? 0;
    const minWidth =
      total > 0
        ? Math.min(
            MIN_PREVIEW_PANEL_WIDTH,
            Math.max(
              240,
              total - MIN_CHAT_SPLIT_WIDTH - PREVIEW_PANEL_DIVIDER_SIZE
            )
          )
        : MIN_PREVIEW_PANEL_WIDTH;
    const maxWidth =
      total > 0
        ? Math.max(
            minWidth,
            total - MIN_CHAT_SPLIT_WIDTH - PREVIEW_PANEL_DIVIDER_SIZE
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
      setPreviewPanelWidth(nextWidth);
    };
    const move = (moveEvent: PointerEvent) => {
      moveEvent.preventDefault();
      updateWidth(moveEvent.clientX);
      // Chrome can deliver pointer events faster than it can lay out both
      // panes. Coalesce them so there is at most one flex relayout per paint.
      if (!frame) {
        frame = requestAnimationFrame(() => {
          frame = 0;
          setPreviewPanelWidth(nextWidth);
        });
      }
    };
    const up = (upEvent: PointerEvent) => {
      upEvent.preventDefault();
      if (upEvent.type === "pointerup") updateWidth(upEvent.clientX);
      flush();
      removePreviewPanelResizeListeners();
      pin();
    };
    previewPanelResize = {
      move,
      up,
      cancelFrame: () => {
        if (frame) cancelAnimationFrame(frame);
        frame = 0;
      },
    };
    setPreviewPanelResizing(true);
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

  function onPreviewDividerKeyDown(event: KeyboardEvent) {
    if (event.key === "ArrowLeft") {
      event.preventDefault();
      setClampedPreviewPanelWidth(previewPanelWidth() + PREVIEW_PANEL_KEY_STEP);
    } else if (event.key === "ArrowRight") {
      event.preventDefault();
      setClampedPreviewPanelWidth(previewPanelWidth() - PREVIEW_PANEL_KEY_STEP);
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
    const url = `ws://${location.host}/ws`;
    debugSocket("connect", { url });
    const ws = new WebSocket(url);
    // Video frames arrive as binary messages; everything else is JSON text.
    ws.binaryType = "arraybuffer";
    ws.onopen = () => {
      debugSocket("open", { url });
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
          scheduleResumePendingJump();
        } else {
          // A live message. Upsert by the announcement timestamp and file id;
          // transfer ids are reused after server restarts, while the pair
          // identifies one file.
          const msg = feed.message;
          preloadImage(msg);
          if (msg.attachment?.kind === "video" && autoplay() !== "disabled") {
            msg.autoplay = autoplay();
          }
          setMessages((prev) => {
            if (msg.file_id !== null) {
              const i = prev.findIndex(
                (m) =>
                  m.file_id === msg.file_id &&
                  m.timestamp_ms === msg.timestamp_ms
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
            prev.includes(env.stream_id) ? prev : [...prev, env.stream_id]
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
              !m.attachment
          );
          if (i < 0) return prev;
          const next = prev.slice();
          next[i] = {
            ...next[i],
            progress: { transferred: env.transferred, total: env.total },
          };
          return next;
        });
      } else if (env.type === "config") {
        setReadonly(env.readonly);
        setAutoplay(env.autoplay);
        setViewerInSeparateBrowserTab(
          env.viewer_in_seperate_browser_tab
        );
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
    ws.onclose = (event) => {
      console.warn("[chatt:ws] closed", {
        code: event.code,
        reason: event.reason,
        was_clean: event.wasClean,
      });
      debugSocket("close", {
        code: event.code,
        reason: event.reason,
        was_clean: event.wasClean,
      });
      setConnected(false);
      scheduleConnectionError();
      reconnectTimer = window.setTimeout(connect, 1000);
    };
    ws.onerror = (event) => {
      console.warn("[chatt:ws] error", event);
      debugSocket("error");
      ws.close();
    };
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
      previewSplitResizeObserver = new ResizeObserver((entries) => {
        const rect = entries[entries.length - 1]?.contentRect;
        if (!rect || Math.abs(rect.width - observedWidth) < 0.01) return;
        observedWidth = rect.width;
        setPreviewPanelWidth((width) => clampPreviewPanelWidth(width));
        pin();
      });
      previewSplitResizeObserver.observe(appBodyEl);
    }
    document.addEventListener("fullscreenchange", onDocumentFullscreenChange);
    scheduleConnectionError();
    connect();
  });
  onCleanup(() => {
    resizeObserver?.disconnect();
    chatViewportResizeObserver?.disconnect();
    splitResizeObserver?.disconnect();
    previewSplitResizeObserver?.disconnect();
    if (viewportPinFrame) cancelAnimationFrame(viewportPinFrame);
    document.removeEventListener(
      "fullscreenchange",
      onDocumentFullscreenChange
    );
    removePaneResizeListeners();
    removePreviewPanelResizeListeners();
    if (reconnectTimer) clearTimeout(reconnectTimer);
    if (connectionErrorTimer !== undefined) clearTimeout(connectionErrorTimer);
    if (suppressTimer) clearTimeout(suppressTimer);
    if (idleTimer) clearTimeout(idleTimer);
    if (refJumpTimer) clearTimeout(refJumpTimer);
    if (pendingJumpFrame) cancelAnimationFrame(pendingJumpFrame);
    if (refToastTimer) clearTimeout(refToastTimer);
    for (const decoder of decoders.values()) decoder.close();
    decoders.clear();
    imagePreloads.clear();
    socket?.close();
  });

  return (
    <div class="app">
      <IconSprite />
      <Show when={connectionErrorVisible()}>
        <div class="conn-overlay" role="status" aria-live="polite">
          Unable to connect — retrying…
        </div>
      </Show>
      <Show when={refToast()}>
        <div class="ref-toast" role="status" aria-live="polite">
          {refToast()}
        </div>
      </Show>
      <div
        class="app-body"
        classList={{ "is-resizing-preview-panel": previewPanelResizing() }}
        ref={appBodyEl}
      >
        <main
          class="app-main"
          classList={{
            "has-video-pane": hasVideoPane(),
            "is-share-fullscreen": fullscreenStream() !== null,
          }}
          ref={mainEl}
          style={
            hasVideoPane()
              ? `--share-pane-height: ${sharePaneHeight()}px`
              : undefined
          }
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
            onTouchStart={onLogTouchStart}
            onTouchMove={markUser}
            onPointerDown={onLogPointerDown}
            onKeyDown={onKeyDown}
            onClick={onLogClick}
          >
            <div class="chat-log-content" ref={contentEl}>
              <Virtualizer
                ref={(h) => (handle = h)}
                scrollRef={logEl}
                data={messageList().visible}
                endMargin={CHAT_END_MARGIN_PX}
                shift={prepend()}
                onScroll={onScroll}
                onScrollEnd={onScrollEnd}
              >
                {(message) => (
                  <MessageRow
                    message={message}
                    group={messageList().groups.get(message)!}
                    onToggleGroup={toggleMessageGroup}
                    onOpenPreview={openPreview}
                    onQuoteRef={readonly() ? undefined : quoteRef}
                    autoplay={message.autoplay ?? "disabled"}
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
                          title="Remove"
                          onClick={() => removeQueued(index())}
                        >
                          <Icon name="x" />
                        </button>
                      </span>
                    )}
                  </For>
                </div>
              </Show>
              <div class="composer-input-row">
                <button
                  class="composer-attach"
                  type="button"
                  aria-label="Attach files"
                  title="Attach files"
                  onClick={openComposeFileDialog}
                >
                  <Icon name="plus" />
                </button>
                <textarea
                  class="composer-text"
                  ref={composerTextEl}
                  rows={1}
                  placeholder="Write a message…"
                  value={draft()}
                  onInput={(event) => {
                    setDraft(event.currentTarget.value);
                    resizeComposer();
                  }}
                  onKeyDown={onComposeKeyDown}
                  onPaste={onComposePaste}
                />
                <input
                  class="composer-file-input"
                  ref={composerFileInputEl}
                  type="file"
                  multiple
                  tabIndex={-1}
                  onChange={onComposeFileInput}
                />
              </div>
            </section>
          </Show>
        </main>
        <Show when={activePreview()}>
          {(preview) => (
            <>
              <div
                class="preview-panel-resize"
                role="separator"
                aria-label="Resize preview panel"
                aria-orientation="vertical"
                aria-valuemin={MIN_PREVIEW_PANEL_WIDTH}
                aria-valuenow={Math.round(previewPanelWidth())}
                tabIndex={0}
                onPointerDown={beginPreviewPanelResize}
                onKeyDown={onPreviewDividerKeyDown}
              />
              <aside
                class="preview-panel-aside"
                style={`--preview-panel-width: ${previewPanelWidth()}px`}
              >
                <PreviewPanel
                  history={previewHistory()}
                  active={preview()}
                  activeKey={activePreviewKey()!}
                  onSelect={setActivePreviewKey}
                  onClose={closePreview}
                  onCloseTab={closePreviewTab}
                  autoplay={autoplay()}
                />
              </aside>
            </>
          )}
        </Show>
      </div>
    </div>
  );
}
