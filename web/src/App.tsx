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
  ViewerMode,
  WebMessage,
  ServerEnvelope,
  ClientRequest,
  ShareInfo,
  Fragment,
  TransferDirection,
} from "./types";
import ScreenShare from "./ScreenShare";
import { ScreenShareDecoder, parseFrame } from "./video-decode";
import { renderInline } from "./highlight";
import { decodeFeed } from "./feed";
import {
  cachedImageState,
  markImageError,
  markImageLoaded,
} from "./image-cache";
import { appendDebugLog, debugFlagEnabled } from "./debug-log";
import Icon, { IconSprite } from "./Icon";
import PreviewPanel, { previewKey, type PreviewItem } from "./PreviewPanel";
import VideoPlayer from "./VideoPlayer";

// Pixel tolerance when deciding the view is "at the bottom". Scroll positions
// are fractional, so an exact comparison would intermittently read as
// not-at-bottom right after a programmatic scroll.
const BOTTOM_EPSILON = 4;

// Request older history before the user reaches the hard top. A viewport-based
// threshold hides normal paging latency without turning every small correction
// near the top into another request.
const TOP_PREFETCH_VIEWPORTS = 2;
const TOP_PREFETCH_MIN = 800;
const TOP_PREFETCH_MAX = 2400;

// How many older messages one paging request asks for.
const PAGE = 100;

// Size of each file-upload message. Kept well under the server's payload cap;
// the server also accepts webviews that fragment the WebSocket frame on the wire.
const UPLOAD_CHUNK_BYTES = 256 * 1024;
const UPLOAD_MAX_BUFFERED_BYTES = 1024 * 1024;
const UPLOAD_DRAIN_POLL_MS = 10;
const DRAFT_STORAGE_KEY = "chatt.web.compose-draft";
const REQUEST_TIMEOUT_MS = 15_000;

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

// Mirror the client's conservative edit window and the protocol's hard
// mutation window. The browser does not receive folded mutation records, so
// these counts hide only messages that are unambiguously too old; Rust still
// performs the authoritative preflight.
const EDIT_ACTION_WINDOW = 200;
const DELETE_ACTION_WINDOW = 256;

type MessageGroupInfo = {
  // Hidden messages inherit their root section's key so reference jumps can
  // expand exactly that section.
  key: string;
  continuation: boolean;
  messageCount: number;
  collapsed: boolean;
  newestOffset: number;
};

type MessageList = {
  visible: WebMessage[];
  groups: Map<WebMessage, MessageGroupInfo>;
};

// Each value is the first message after a collapsed section, or null when the
// section extends to the end of its sender group. Explicit ends keep sibling
// sections independent when either one is expanded.
type CollapsedSections = ReadonlyMap<string, string | null>;

type PendingWebEdit = {
  target: number;
  original: string;
  parkedDraft: string;
  parkedFiles: File[];
};

function isMessageContinuation(
  message: WebMessage,
  previous: WebMessage | undefined
): boolean {
  return (
    !!previous &&
    !message.edited &&
    !previous.edited &&
    previous.sender === message.sender &&
    message.timestamp_ms - previous.timestamp_ms < GROUP_WINDOW_MS
  );
}

function messageCollapseKey(message: WebMessage): string {
  return `${message.timestamp_ms}:${message.message_id}:${message.id}`;
}

function debugMessageKey(message: WebMessage | undefined): string | undefined {
  if (!message) return undefined;
  return `${message.timestamp_ms}:${message.message_id}:${message.id}`;
}

// Projects the flat feed into sender groups. Collapsed ranges are root-level
// siblings: an existing section bounds the section before it instead of being
// nested inside that section.
function buildMessageList(
  messages: readonly WebMessage[],
  collapsedSections: CollapsedSections
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

    const sections: { start: number; end: number; key: string }[] = [];
    for (let index = start; index < end; index++) {
      const key = messageCollapseKey(messages[index]!);
      if (!collapsedSections.has(key)) continue;

      const endKey = collapsedSections.get(key);
      let sectionEnd = end;
      if (endKey) {
        for (let candidate = index + 1; candidate < end; candidate++) {
          if (messageCollapseKey(messages[candidate]!) === endKey) {
            sectionEnd = candidate;
            break;
          }
        }
      }
      sections.push({ start: index, end: sectionEnd, key });
    }

    // Group edits or prepended history can bring formerly separate sections
    // together. Clamp them at the next root boundary to preserve no-nesting.
    for (let index = 0; index < sections.length; index++) {
      const nextStart = sections[index + 1]?.start ?? end;
      sections[index]!.end = Math.min(sections[index]!.end, nextStart);
    }

    let sectionIndex = 0;
    for (let index = start; index < end; index++) {
      const message = messages[index]!;
      while (
        sectionIndex < sections.length &&
        index >= sections[sectionIndex]!.end
      ) {
        sectionIndex++;
      }
      const section = sections[sectionIndex];
      const collapsed =
        !!section && index >= section.start && index < section.end;
      const collapseStart = collapsed && index === section!.start;
      const nextBoundary = section?.start ?? end;
      groups.set(message, {
        key: collapsed ? section!.key : messageCollapseKey(message),
        // A collapsed continuation becomes a compact header of its own until
        // expanded, while preceding rows retain their original grouping.
        continuation: index > start && !collapseStart,
        messageCount: collapsed
          ? section!.end - section!.start
          : nextBoundary - index,
        collapsed,
        newestOffset: messages.length - index,
      });
      if (!collapsed || collapseStart) visible.push(message);
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

type OlderRequestSource = "scroll" | "ref";

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

function uploadDebugEnabled(): boolean {
  return debugFlagEnabled("debugUpload", "chatt.debugUpload");
}

function socketDebugEnabled(): boolean {
  return (
    debugFlagEnabled("debugSocket", "chatt.debugSocket") || uploadDebugEnabled()
  );
}

function scrollDebugEnabled(): boolean {
  return debugFlagEnabled("debugScroll", "chatt.debugScroll");
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

function debugScroll(stage: string, fields: Record<string, unknown> = {}) {
  if (!scrollDebugEnabled()) return;
  appendDebugLog("scroll", stage, fields);
}

function delay(ms: number): Promise<void> {
  return new Promise((resolve) => window.setTimeout(resolve, ms));
}

function loadSessionDraft(): string {
  try {
    return typeof sessionStorage === "undefined"
      ? ""
      : sessionStorage.getItem(DRAFT_STORAGE_KEY) ?? "";
  } catch {
    return "";
  }
}

function clamp(value: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, value));
}

function formatTime(ms: number): string {
  if (!ms) return "";
  const d = new Date(ms);
  return d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}

function formatExactTime(ms: number): string {
  return ms ? new Date(ms).toLocaleString([], { dateStyle: "full", timeStyle: "medium" }) : "";
}

function dateKey(ms: number): string {
  const date = new Date(ms);
  return `${date.getFullYear()}-${date.getMonth()}-${date.getDate()}`;
}

function formatDateLabel(ms: number): string {
  const date = new Date(ms);
  const today = new Date();
  const yesterday = new Date(today);
  yesterday.setDate(today.getDate() - 1);
  if (dateKey(ms) === dateKey(today.getTime())) return "Today";
  if (dateKey(ms) === dateKey(yesterday.getTime())) return "Yesterday";
  return date.toLocaleDateString([], { dateStyle: "long" });
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

// Progress bar shown on a file's placeholder message while a transfer is in
// flight, replaced by the attachment on completion. `direction` picks the verb
// and, when `onAbort` is provided (writable view), the button: an incoming
// download offers [skip], an outgoing upload offers [cancel].
function TransferProgressBar(props: {
  progress: { transferred: number; total: number; direction: TransferDirection };
  onAbort?: () => void;
}) {
  const ratio = () => {
    const { transferred, total } = props.progress;
    return total > 0 ? Math.min(1, transferred / total) : 0;
  };
  const pct = () => Math.round(ratio() * 100);
  const incoming = () => props.progress.direction === "incoming";
  const verb = () => (incoming() ? "receiving" : "sending");
  const abortLabel = () => (incoming() ? "skip" : "cancel");
  return (
    <div class="message-progress-row">
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
          {verb()} {formatBytes(props.progress.transferred)} /{" "}
          {formatBytes(props.progress.total)} ({pct()}%)
        </span>
      </div>
      <Show when={props.onAbort}>
        <button
          class="message-progress-abort"
          type="button"
          aria-label={incoming() ? "Skip download" : "Cancel upload"}
          title={incoming() ? "Skip download" : "Cancel upload"}
          onClick={() => props.onAbort!()}
        >
          {abortLabel()}
        </button>
      </Show>
    </div>
  );
}

// The virtualizer unmounts rows that scroll out of the window, so a fragment's
// body HTML would otherwise be re-read every time its row scrolls back in. A
// fragment object is created once per decoded message and never mutated
// (progress merges keep the same fragments array), so it is a stable cache
// key; replaced messages simply fall out with GC.
type TextFragment = Extract<Fragment, { kind: "text" }>;
type CodeFragment = Extract<Fragment, { kind: "code" }>;
type ContentFragment = TextFragment | CodeFragment;

const fragmentHtmlCache = new WeakMap<ContentFragment, string>();
const codeTextDecoder = new TextDecoder();

const ESTIMATE_TEXT_LINE_HEIGHT = 22;
const ESTIMATE_CODE_LINE_HEIGHT = 19;
const ESTIMATE_CHAT_CONTENT_WIDTH = 720;
const ESTIMATE_TEXT_CHARS_PER_LINE = 86;
const ESTIMATE_TEXT_AVG_CHAR_WIDTH =
  ESTIMATE_CHAT_CONTENT_WIDTH / ESTIMATE_TEXT_CHARS_PER_LINE;
const ESTIMATE_IMAGE_MAX_HEIGHT = 460;
const ESTIMATE_IMAGE_VIEWPORT_RATIO = 0.5;
const ESTIMATE_MESSAGE_HORIZONTAL_PADDING = 72;
const ESTIMATE_MIN_CONTENT_WIDTH = 240;
const ESTIMATE_MEDIA_MARGIN_Y = 10;
const ESTIMATE_VIDEO_HEIGHT = 240;

type MessageEstimateLayout = {
  contentWidth: number;
  imageMaxHeight: number;
};

const DEFAULT_MESSAGE_ESTIMATE_LAYOUT: MessageEstimateLayout = {
  contentWidth: ESTIMATE_CHAT_CONTENT_WIDTH,
  imageMaxHeight: ESTIMATE_IMAGE_MAX_HEIGHT,
};

function countMatches(text: string, pattern: RegExp): number {
  return text.match(pattern)?.length ?? 0;
}

function htmlTextForEstimate(html: string): string {
  return html
    .replace(/<br\s*\/?>/gi, "\n")
    .replace(/<\/(?:p|li|h3|div|blockquote)>/gi, "\n")
    .replace(/<[^>]*>/g, "")
    .replace(/&(?:#\d+|#x[\da-f]+|[a-z][a-z\d]+);/gi, "x")
    .replace(/[ \t\r\f\v]+/g, " ")
    .trim();
}

function estimateWrappedLines(text: string, charsPerLine: number): number {
  if (!text) return 0;
  return text
    .split("\n")
    .reduce(
      (lines, segment) =>
        lines + Math.max(1, Math.ceil(segment.trim().length / charsPerLine)),
      0
    );
}

function estimateTextCharsPerLine(layout: MessageEstimateLayout): number {
  return Math.max(
    24,
    Math.floor(layout.contentWidth / ESTIMATE_TEXT_AVG_CHAR_WIDTH)
  );
}

function estimateTextFragmentHeight(
  fragment: TextFragment,
  layout: MessageEstimateLayout
): number {
  const html = fragment.html;
  const text = htmlTextForEstimate(html);
  if (!text) return 0;

  const blockCount = Math.max(
    1,
    countMatches(html, /<(?:p|li|h3)\b/gi) +
      countMatches(html, /<br\s*\/?>/gi)
  );
  const lines = Math.max(
    blockCount,
    estimateWrappedLines(text, estimateTextCharsPerLine(layout))
  );
  return lines * ESTIMATE_TEXT_LINE_HEIGHT + Math.max(0, blockCount - 1) * 6;
}

function estimateCodeFragmentHeight(fragment: CodeFragment): number {
  let lines = 1;
  for (const byte of fragment.text) {
    if (byte === 10) {
      lines += 1;
    }
  }
  // Code blocks use `white-space: pre` with horizontal scrolling, so long
  // lines affect width, not row height.
  // Frame margins, border, and vertical padding from `.code-block-frame` and
  // `.code-block`.
  return lines * ESTIMATE_CODE_LINE_HEIGHT + 42;
}

function estimateFragmentsHeight(
  fragments: readonly Fragment[],
  layout: MessageEstimateLayout
): number {
  let height = 0;
  for (const fragment of fragments) {
    switch (fragment.kind) {
      case "text":
        height += estimateTextFragmentHeight(fragment, layout);
        break;
      case "code":
        height += estimateCodeFragmentHeight(fragment);
        break;
      case "quote_start":
        height += 6;
        break;
      case "quote_end":
        break;
    }
  }
  return height;
}

function estimateAttachmentHeight(
  attachment: NonNullable<WebMessage["attachment"]>,
  layout: MessageEstimateLayout
): number {
  switch (attachment.kind) {
    case "image": {
      const width = attachment.width ?? 0;
      const height = attachment.height ?? 0;
      if (width > 0 && height > 0) {
        const scaledHeight =
          height * Math.min(1, layout.contentWidth / width) + 12;
        const imageHeight = Math.min(scaledHeight, layout.imageMaxHeight);
        return (
          ESTIMATE_MEDIA_MARGIN_Y + Math.max(72, Math.ceil(imageHeight))
        );
      }
      return ESTIMATE_MEDIA_MARGIN_Y + 160;
    }
    case "video":
      return ESTIMATE_MEDIA_MARGIN_Y + ESTIMATE_VIDEO_HEIGHT;
    case "audio":
      return ESTIMATE_MEDIA_MARGIN_Y + 40;
    case "file":
      return ESTIMATE_MEDIA_MARGIN_Y + 34;
  }
}

function estimateMessageRowSize(
  message: WebMessage,
  group: MessageGroupInfo,
  layout: MessageEstimateLayout = DEFAULT_MESSAGE_ESTIMATE_LAYOUT
): number {
  if (group.collapsed) return 38;

  let height = group.continuation ? 2 : 31;
  height += estimateFragmentsHeight(message.fragments, layout);

  if (message.attachment) {
    height += estimateAttachmentHeight(message.attachment, layout);
  } else if (message.progress) {
    height += 24;
  } else if (message.terminal) {
    height += 21;
  }

  return Math.max(24, Math.ceil(height));
}

function fragmentHtml(fragment: ContentFragment): string {
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

function MessageFragment(props: { fragment: ContentFragment }) {
  const content = () =>
    props.fragment.kind === "text" ? (
      <div class="message-body" innerHTML={fragmentHtml(props.fragment)} />
    ) : (
      <CodeBlock fragment={props.fragment} />
    );

  return content();
}

type MessageNode =
  | { kind: "fragment"; fragment: ContentFragment }
  | { kind: "quote"; children: MessageNode[] };

function pruneEmptyQuotes(nodes: MessageNode[]): MessageNode[] {
  const pruned: MessageNode[] = [];
  for (const node of nodes) {
    if (node.kind === "fragment") {
      pruned.push(node);
      continue;
    }

    const children = pruneEmptyQuotes(node.children);
    if (children.length > 0) pruned.push({ kind: "quote", children });
  }
  return pruned;
}

function messageNodes(fragments: readonly Fragment[]): MessageNode[] {
  const root: MessageNode[] = [];
  const stack: MessageNode[][] = [root];

  for (const fragment of fragments) {
    const current = stack[stack.length - 1]!;
    switch (fragment.kind) {
      case "quote_start": {
        const node: MessageNode = { kind: "quote", children: [] };
        current.push(node);
        stack.push(node.children);
        break;
      }
      case "quote_end":
        if (stack.length > 1) stack.pop();
        break;
      default:
        current.push({ kind: "fragment", fragment });
        break;
    }
  }

  return pruneEmptyQuotes(root);
}

function MessageNodeView(props: { node: MessageNode }) {
  if (props.node.kind === "fragment") {
    return <MessageFragment fragment={props.node.fragment} />;
  }

  return (
    <blockquote class="message-quote">
      <div class="message-quote-content">
        <For each={props.node.children}>
          {(child) => <MessageNodeView node={child} />}
        </For>
      </div>
    </blockquote>
  );
}

// Renders a message body from Rust-produced subset HTML and precomputed code
// highlight spans. Nothing is parsed or highlighted in the browser.
function MessageBody(props: { fragments: Fragment[] }) {
  const nodes = createMemo(() => messageNodes(props.fragments));

  return (
    <For each={nodes()}>
      {(node) => <MessageNodeView node={node} />}
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
  const cachedImage = () =>
    att().kind === "image" ? cachedImageState(url()) : undefined;
  const imageUnavailable = () => cachedImage()?.status === "error";
  const hasIntrinsicImageSize = () =>
    (att().width ?? 0) > 0 && (att().height ?? 0) > 0;
  const missingImageStyle = () => {
    const width = att().width ?? 0;
    const height = att().height ?? 0;
    if (width > 0 && height > 0) {
      return {
        // Unlike a replaced <img>, the placeholder div does not transfer its
        // max-height constraint back to its width. Apply the equivalent width
        // cap explicitly so the intrinsic ratio survives the 50vh limit.
        width: `min(${width}px, 100%, ${(50 * width) / height}vh)`,
        "aspect-ratio": `${width} / ${height}`,
      };
    }
    if (width > 0) return { width: `${width}px` };
    return undefined;
  };
  // Fades the image in on decode instead of snapping. The box is already
  // reserved by width/height, so this only affects the pixels, never layout.
  const [loaded, setLoaded] = createSignal(
    cachedImage()?.status === "loaded"
  );
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
          <Show
            when={imageUnavailable()}
            fallback={
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
            }
          >
            <div
              class="media-image media-image-missing"
              classList={{ "has-intrinsic-size": hasIntrinsicImageSize() }}
              style={missingImageStyle()}
              role="img"
              aria-label={`${att().name} failed to load`}
              title={`${att().name} failed to load`}
            >
              image unavailable
            </div>
          </Show>
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

const REF_HOVER_DELAY_MS = 200;
// Mirror the .ref-hover-card max-width/max-height in styles.css; used to clamp
// and flip the fixed-position card before it has rendered.
const REF_HOVER_MAX_WIDTH = 360;
const REF_HOVER_MAX_HEIGHT = 240;
const REF_HOVER_GAP = 4;
const REF_HOVER_MARGIN = 8;

// Exactly one of top/bottom is set: `bottom` anchors the card above the pill
// (growing upward, so its unknown height needs no measurement), `top` below.
interface RefHoverState {
  message: WebMessage;
  left: number;
  top: number | null;
  bottom: number | null;
}

// Floating preview of a referenced message, shown while hovering a resolved
// `@@` pill. Inert to the pointer; positioning is fixed viewport coordinates
// computed by the hover handlers in App.
function RefHoverCard(props: { hover: RefHoverState }) {
  const style = () => ({
    left: `${props.hover.left}px`,
    top: props.hover.top !== null ? `${props.hover.top}px` : undefined,
    bottom:
      props.hover.bottom !== null ? `${props.hover.bottom}px` : undefined,
  });
  return (
    <div class="ref-hover-card" style={style()} role="tooltip">
      <div class="ref-hover-meta">
        <span class="ref-hover-sender">{props.hover.message.sender}</span>
        <Show when={props.hover.message.edited}>
          <span class="message-edited">(edited)</span>
        </Show>
        <span class="ref-hover-time" title={formatExactTime(props.hover.message.timestamp_ms)}>
          {formatTime(props.hover.message.timestamp_ms)}
        </span>
      </div>
      <MessageBody fragments={props.hover.message.fragments} />
      <Show when={props.hover.message.attachment}>
        {(att) => {
          // Reserve the image box from its intrinsic size so the card does not
          // resize when the image decodes; bottom-anchored cards grow upward.
          const box = () => {
            const width = att().width ?? 0;
            const height = att().height ?? 0;
            if (width > 0 && height > 0) {
              return {
                width: `min(100%, ${width}px)`,
                "aspect-ratio": `${width} / ${height}`,
              };
            }
            return undefined;
          };
          return (
            <Show
              when={att().kind === "image"}
              fallback={
                <div class="ref-hover-attachment">
                  {att().kind}: {att().name}
                </div>
              }
            >
              <img
                class="ref-hover-image"
                src={fileUrl(att().name)}
                alt={att().name}
                style={box()}
              />
            </Show>
          );
        }}
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
  onAbortTransfer?: (transferId: number) => void;
  onEdit?: (message: WebMessage) => void;
  onDelete?: (message: WebMessage, opener: HTMLButtonElement) => void;
  autoplay: AutoplayMode;
}) {
  // A continuation hides the header and shows its time only on hover, in the
  // reserved left gutter. Group metadata is projected reactively from the full
  // feed so prepended history can still change the boundary row's grouping.
  const continuation = () => props.group.continuation;
  const canEdit = () =>
    props.message.local &&
    props.message.message_id !== 0 &&
    props.message.file_id === null &&
    props.group.newestOffset <= EDIT_ACTION_WINDOW &&
    !!props.onEdit;
  const canDelete = () =>
    props.message.local &&
    props.message.message_id !== 0 &&
    props.group.newestOffset <= DELETE_ACTION_WINDOW &&
    !!props.onDelete;
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
      onClick={(event) => {
        if (!props.group.collapsed) return;
        const active = document.activeElement;
        if (
          event.detail > 0 &&
          active instanceof HTMLElement &&
          event.currentTarget.contains(active)
        ) {
          active.blur();
        }
        props.onToggleGroup(props.group.key);
      }}
    >
      {/* The time always lives in the left gutter so it sits in one consistent
       * column: shown on a group's first row, revealed on hover for the rest. */}
      <span
        class="message-time-gutter"
        title={formatExactTime(props.message.timestamp_ms)}
        aria-label={formatExactTime(props.message.timestamp_ms)}
        tabIndex={0}
      >
        {formatTime(props.message.timestamp_ms)}
      </span>
      <Show when={!continuation()}>
        <div class="message-meta">
          <span class="message-sender">{props.message.sender}</span>
          <Show when={props.message.edited}>
            <span class="message-edited">(edited)</span>
          </Show>
          <Show when={props.group.collapsed}>
            <span class="message-group-summary">
              {props.group.messageCount}{" "}
              {props.group.messageCount === 1 ? "message" : "messages"} collapsed
            </span>
          </Show>
        </div>
      </Show>
      <button
        class="message-group-toggle"
        type="button"
        aria-expanded={!props.group.collapsed}
        aria-label={groupLabel()}
        title={
          props.group.collapsed
            ? "Expand collapsed messages"
            : "Collapse this message and those below"
        }
        onClick={(event) => {
          event.stopPropagation();
          if (event.detail > 0) event.currentTarget.blur();
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
          <TransferProgressBar
            progress={props.message.progress!}
            onAbort={
              props.onAbortTransfer && props.message.file_id != null
                ? () => props.onAbortTransfer!(props.message.file_id!)
                : undefined
            }
          />
        </Show>
        <Show when={!props.message.attachment && props.message.terminal}>
          <div class="message-transfer-terminal">
            {props.message.terminal!.reason
              ? `${props.message.terminal!.verb}: ${props.message.terminal!.reason}`
              : props.message.terminal!.verb}
          </div>
        </Show>
        <Show
          when={
            props.message.ref_code ||
            canEdit() ||
            canDelete()
          }
        >
          <div class="message-actions">
            <Show when={canEdit()}>
              <button
                class="message-action"
                type="button"
                aria-label="Edit message"
                title="Edit"
                onClick={() => props.onEdit!(props.message)}
              >
                <Icon name="pencil" />
              </button>
            </Show>
            <Show when={canDelete()}>
              <button
                class="message-action is-destructive"
                type="button"
                aria-label="Delete message"
                title="Delete"
                onClick={(event) =>
                  props.onDelete!(props.message, event.currentTarget)
                }
              >
                <Icon name="trash-2" />
              </button>
            </Show>
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
            <Show when={props.message.ref_code}>
              <button
                class="message-action"
                type="button"
                aria-label={refCopied() ? "Copied reference" : "Copy reference"}
                title={refCopied() ? "Copied" : "Copy reference"}
                onClick={copyRef}
              >
                <Icon name={refCopied() ? "check" : "at-sign"} />
              </button>
            </Show>
          </div>
        </Show>
      </Show>
    </div>
  );
}

function DeleteConfirmation(props: {
  message: WebMessage;
  onCancel: () => void;
  onConfirm: () => void;
}) {
  let cancelButton: HTMLButtonElement | undefined;
  let deleteButton: HTMLButtonElement | undefined;

  onMount(() => {
    cancelButton?.focus();
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        event.preventDefault();
        props.onCancel();
        return;
      }
      if (event.key !== "Tab" || !cancelButton || !deleteButton) return;
      if (event.shiftKey && document.activeElement === cancelButton) {
        event.preventDefault();
        deleteButton.focus();
      } else if (!event.shiftKey && document.activeElement === deleteButton) {
        event.preventDefault();
        cancelButton.focus();
      }
    };
    document.addEventListener("keydown", onKeyDown);
    onCleanup(() => document.removeEventListener("keydown", onKeyDown));
  });

  return (
    <div
      class="confirm-backdrop"
      onMouseDown={(event) => {
        if (event.target === event.currentTarget) props.onCancel();
      }}
    >
      <section
        class="confirm-dialog"
        role="alertdialog"
        aria-modal="true"
        aria-labelledby="delete-confirm-title"
        aria-describedby="delete-confirm-description"
      >
        <h2 id="delete-confirm-title">Delete message?</h2>
        <p id="delete-confirm-description">
          This will permanently delete {props.message.sender}'s message from
          the room.
        </p>
        <div class="confirm-actions">
          <button
            class="confirm-button"
            type="button"
            ref={cancelButton}
            onClick={props.onCancel}
          >
            Cancel
          </button>
          <button
            class="confirm-button is-destructive"
            type="button"
            ref={deleteButton}
            onClick={props.onConfirm}
          >
            Delete
          </button>
        </div>
      </section>
    </div>
  );
}

export default function App() {
  const standalone = standalonePreviewFromLocation();
  const scrollDebugActive = scrollDebugEnabled();
  if (standalone) {
    const key = previewKey(standalone.item);
    document.title = standalone.item.name;
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
  const [collapsedSections, setCollapsedSections] =
    createSignal<CollapsedSections>(new Map());
  const messageList = createMemo(() =>
    buildMessageList(messages(), collapsedSections())
  );
  const [refToast, setRefToast] = createSignal<string | null>(null);
  const [deleteError, setDeleteError] = createSignal<string | null>(null);
  const [refHover, setRefHover] = createSignal<RefHoverState | null>(null);
  const [connected, setConnected] = createSignal(false);
  const [connectionErrorVisible, setConnectionErrorVisible] =
    createSignal(false);
  // Drives virtua's `shift`: while true a data change is treated as a prepend so
  // scroll position is anchored from the end (reverse infinite scroll).
  const [prepend, setPrepend] = createSignal(false);
  const [newMessageCount, setNewMessageCount] = createSignal(0);

  // Screen shares this browser can watch, and the stream ids currently playing.
  const [shares, setShares] = createSignal<ShareInfo[]>([]);
  const [playing, setPlaying] = createSignal<number[]>([]);
  const [shareStates, setShareStates] = createSignal<Record<number, string>>({});
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
  const [compactPreview, setCompactPreview] = createSignal(false);
  const [autoplay, setAutoplay] =
    createSignal<AutoplayMode>("disabled");
  const [viewer, setViewer] = createSignal<ViewerMode>("panel");
  let previewOpener: HTMLElement | undefined;
  let deleteOpener: HTMLButtonElement | undefined;
  let deleteErrorTimer: number | undefined;
  let previewMedia: MediaQueryList | undefined;

  const activePreview = () => {
    const key = activePreviewKey();
    return key
      ? previewHistory().find((item) => previewKey(item) === key) ?? null
      : null;
  };

  function openPreview(item: PreviewItem, opener: HTMLElement) {
    if (viewer() === "tab") {
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
  const [draft, setDraft] = createSignal(loadSessionDraft());
  // Files dragged onto the composer, held until the message is submitted.
  const [queued, setQueued] = createSignal<File[]>([]);
  const [editing, setEditing] = createSignal<PendingWebEdit | null>(null);
  const [pendingDelete, setPendingDelete] = createSignal<WebMessage | null>(null);
  const [dragActive, setDragActive] = createSignal(false);
  const [submitting, setSubmitting] = createSignal(false);
  const [composeError, setComposeError] = createSignal<string | null>(null);
  const [maxUploadBytes, setMaxUploadBytes] = createSignal(Number.MAX_SAFE_INTEGER);
  const [staging, setStaging] = createSignal<{
    uploadId: number;
    file: File;
    sent: number;
    cancelled: boolean;
  } | null>(null);
  // A per-connection counter naming each upload so its chunk frames route to the
  // right server-side file.
  let nextUploadId = 1;
  let nextRequestId = 1;
  const pendingRequests = new Map<
    number,
    { resolve: () => void; reject: (error: Error) => void; timer: number }
  >();

  createEffect(() => {
    const value = draft();
    try {
      if (value) sessionStorage.setItem(DRAFT_STORAGE_KEY, value);
      else sessionStorage.removeItem(DRAFT_STORAGE_KEY);
    } catch {
      // Storage can be disabled; the in-memory draft remains authoritative.
    }
  });

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
    setShareStates((prev) => ({ ...prev, [streamId]: "stopped" }));
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
  let topPagingArmed = true;
  let prependSettling = false;
  let prependSettleFrame: number | undefined;

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
  let lastScrollDebugAt = 0;
  let lastTopBlockedDebugAt = 0;

  // Use virtua's measured geometry, not DOM scrollHeight: when tail items are
  // still unmeasured, totalSize is an estimate and scrollHeight can disagree.
  function atBottom(): boolean {
    if (!handle) return true;
    return (
      handle.scrollOffset >=
      handle.scrollSize - handle.viewportSize - BOTTOM_EPSILON
    );
  }

  function chatViewportSize(): number {
    return handle?.viewportSize || logEl?.clientHeight || 0;
  }

  function messageEstimateLayout(): MessageEstimateLayout {
    const contentWidth = logEl?.clientWidth
      ? Math.max(
          ESTIMATE_MIN_CONTENT_WIDTH,
          logEl.clientWidth - ESTIMATE_MESSAGE_HORIZONTAL_PADDING
        )
      : ESTIMATE_CHAT_CONTENT_WIDTH;
    const imageMaxHeight =
      typeof window === "undefined" || window.innerHeight <= 0
        ? ESTIMATE_IMAGE_MAX_HEIGHT
        : Math.max(72, window.innerHeight * ESTIMATE_IMAGE_VIEWPORT_RATIO);
    return { contentWidth, imageMaxHeight };
  }

  function topRequestThreshold(): number {
    const viewport = chatViewportSize();
    if (viewport <= 0) return TOP_PREFETCH_MIN;
    return clamp(
      viewport * TOP_PREFETCH_VIEWPORTS,
      TOP_PREFETCH_MIN,
      TOP_PREFETCH_MAX
    );
  }

  function topRearmThreshold(): number {
    return topRequestThreshold() + Math.max(chatViewportSize(), 400);
  }

  function debugScrollState(
    stage: string,
    fields: Record<string, unknown> = {}
  ) {
    if (!scrollDebugActive) return;

    const h = handle;
    const el = logEl;
    const estimateLayout = messageEstimateLayout();
    const list = messageList().visible;
    const topIndex =
      h && h.scrollOffset >= 0 ? h.findItemIndex(h.scrollOffset) : undefined;
    const topMessage =
      topIndex !== undefined && topIndex >= 0 ? list[topIndex] : undefined;

    debugScroll(stage, {
      domTop: el?.scrollTop,
      domScrollHeight: el?.scrollHeight,
      domClientHeight: el?.clientHeight,
      vOffset: h?.scrollOffset,
      vScrollSize: h?.scrollSize,
      vViewportSize: h?.viewportSize,
      topIndex,
      topMessage: debugMessageKey(topMessage),
      messages: messages().length,
      visible: list.length,
      firstMessage: debugMessageKey(messages()[0]),
      oldestSeq,
      hasMore,
      loadingOlder,
      topPagingArmed,
      prependSettling,
      topRequestThreshold: topRequestThreshold(),
      topRearmThreshold: topRearmThreshold(),
      estimateContentWidth: estimateLayout.contentWidth,
      estimateImageMaxHeight: estimateLayout.imageMaxHeight,
      prepend: prepend(),
      userDriving,
      suppress,
      following,
      refJumping,
      ...fields,
    });
  }

  function debugNow(): number {
    return typeof performance === "undefined" ? Date.now() : performance.now();
  }

  // The ONLY thing allowed to flip `following`. Bound to genuine input events.
  function markUser() {
    const wasDriving = userDriving;
    userDriving = true;
    if (!wasDriving) debugScrollState("user-start");
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

  function clearPrependSettleFrame() {
    if (prependSettleFrame !== undefined) {
      cancelAnimationFrame(prependSettleFrame);
      prependSettleFrame = undefined;
    }
  }

  function finishPrependSettling() {
    prependSettling = false;
    prependSettleFrame = undefined;
    const rearmThreshold = topRearmThreshold();
    if (handle && handle.scrollOffset > rearmThreshold) {
      topPagingArmed = true;
    }
    debugScrollState("prepend-settle-end", { rearmThreshold });
  }

  function holdPrependSettling() {
    prependSettling = true;
    clearPrependSettleFrame();
    debugScrollState("prepend-settle-start");
    prependSettleFrame = requestAnimationFrame(() => {
      debugScrollState("prepend-settle-raf1");
      prependSettleFrame = requestAnimationFrame(() => {
        debugScrollState("prepend-settle-raf2");
        finishPrependSettling();
      });
    });
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
    if (previewPanelResize || !handle || refJumping || !following) {
      debugScrollState("pin-skip", {
        reason: {
          previewPanelResize: !!previewPanelResize,
          noHandle: !handle,
          refJumping,
          detached: !following,
        },
      });
      return;
    }
    const last = messages().length - 1;
    if (last < 0) return;
    suppressProgrammaticScroll(250);
    const target = Math.max(0, handle.scrollSize - handle.viewportSize);
    debugScrollState("pin", { target, last });
    // Scroll to the virtual bottom so the configured end margin remains visible
    // after the newest message.
    handle.scrollTo(target);
    // Programmatic scrolls may emit zero scroll events when already at the
    // destination, so clearing `suppress` from onScrollEnd would deadlock it.
    // Always clear on a timer that outlives the measurement window.
  }

  function jumpToLatest() {
    following = true;
    pin();
    window.setTimeout(() => {
      if (atBottom()) setNewMessageCount(0);
    }, 300);
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

  function requestOlder(source: OlderRequestSource): boolean {
    if (!hasMore) {
      debugScrollState("request-older-skip", { source, reason: "no-more" });
      return false;
    }
    if (loadingOlder) {
      debugScrollState("request-older-skip", { source, reason: "loading" });
      return false;
    }
    if (!socket || socket.readyState !== WebSocket.OPEN) {
      debugScrollState("request-older-skip", { source, reason: "socket" });
      return false;
    }
    loadingOlder = true;
    const req: ClientRequest = {
      type: "load_older",
      before_seq: oldestSeq,
      limit: PAGE,
    };
    debugScrollState("request-older-send", {
      source,
      beforeSeq: req.before_seq,
      limit: req.limit,
    });
    socket.send(JSON.stringify(req));
    return true;
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

  // Message ids are the durable identity; `ts` rides along for display only.
  function findMessageIndex(_ts: number, mid: number): number {
    return messages().findIndex((m) => m.message_id === mid);
  }

  function toggleMessageGroup(key: string) {
    setCollapsedSections((current) => {
      const next = new Map(current);
      if (next.has(key)) {
        next.delete(key);
        return next;
      }

      const feed = messages();
      const selected = feed.findIndex(
        (message) => messageCollapseKey(message) === key
      );
      if (selected < 0) return current;

      let groupEnd = selected + 1;
      while (
        groupEnd < feed.length &&
        isMessageContinuation(feed[groupEnd]!, feed[groupEnd - 1])
      ) {
        groupEnd++;
      }

      let nextSection: string | null = null;
      for (let index = selected + 1; index < groupEnd; index++) {
        const candidate = messageCollapseKey(feed[index]!);
        if (current.has(candidate)) {
          nextSection = candidate;
          break;
        }
      }
      next.set(key, nextSection);
      return next;
    });
  }

  function showsDateSeparator(message: WebMessage): boolean {
    const visible = messageList().visible;
    const index = visible.indexOf(message);
    return (
      index === 0 ||
      dateKey(visible[index - 1]!.timestamp_ms) !== dateKey(message.timestamp_ms)
    );
  }

  function flashMessage(_ts: number, mid: number) {
    const row = logEl?.querySelector(`.message[data-mid="${mid}"]`);
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
    const collapseKeyToExpand = group?.collapsed ? group.key : null;
    if (collapseKeyToExpand) {
      setCollapsedSections((current) => {
        const next = new Map(current);
        next.delete(collapseKeyToExpand);
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
    if (collapseKeyToExpand) requestAnimationFrame(scroll);
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
      requestOlder("ref");
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
    requestOlder("ref");
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

  // Hover preview for `@@` reference pills. Delegated mouseover/mouseout on the
  // log container, mirroring onLogClick, because the anchors live inside
  // Rust-rendered fragment HTML. The card is pointer-inert, so only the pill
  // itself keeps the hover alive. A target outside the loaded window is fetched
  // from the web server's retained history with a `ref_preview` request; the
  // responses are cached (hits and misses) until the next room sync.
  let refHoverTimer: number | undefined;
  let refHoverAnchor: HTMLElement | undefined;
  let refHoverPendingKey: string | undefined;
  const refPreviewCache = new Map<string, WebMessage | null>();

  function invalidateMessageReference(messageId: number) {
    refPreviewCache.delete(refPreviewKey(0, messageId));
    if (refHover()?.message.message_id === messageId) hideRefHover();
  }

  function refPreviewKey(_ts: number, mid: number): string {
    return `${mid}`;
  }

  function hideRefHover() {
    if (refHoverTimer !== undefined) {
      clearTimeout(refHoverTimer);
      refHoverTimer = undefined;
    }
    refHoverAnchor = undefined;
    refHoverPendingKey = undefined;
    if (refHover()) setRefHover(null);
  }

  function showRefHover(anchor: HTMLElement) {
    // Ref pills carry only `data-mid` now; the ts is a harmless echo.
    const ts = Number(anchor.dataset.ts ?? "0");
    const mid = Number(anchor.dataset.mid);
    if (!Number.isFinite(ts) || !Number.isFinite(mid)) return;
    const index = findMessageIndex(ts, mid);
    if (index >= 0) {
      displayRefHover(anchor, messages()[index]!);
      return;
    }
    const key = refPreviewKey(ts, mid);
    if (refPreviewCache.has(key)) {
      const cached = refPreviewCache.get(key);
      if (cached) displayRefHover(anchor, cached);
      return;
    }
    refHoverPendingKey = key;
    sendJson({ type: "ref_preview", ts, mid });
  }

  function onRefPreview(ts: number, mid: number, message: WebMessage | null) {
    const key = refPreviewKey(ts, mid);
    refPreviewCache.set(key, message);
    if (key !== refHoverPendingKey) return;
    refHoverPendingKey = undefined;
    const anchor = refHoverAnchor;
    if (!anchor || !anchor.isConnected || !message) return;
    displayRefHover(anchor, message);
  }

  function displayRefHover(anchor: HTMLElement, message: WebMessage) {
    const rect = anchor.getBoundingClientRect();
    const left = clamp(
      rect.left,
      REF_HOVER_MARGIN,
      Math.max(
        REF_HOVER_MARGIN,
        window.innerWidth - REF_HOVER_MAX_WIDTH - REF_HOVER_MARGIN
      )
    );
    const fitsAbove =
      rect.top >= REF_HOVER_MAX_HEIGHT + REF_HOVER_GAP + REF_HOVER_MARGIN;
    const belowTop = clamp(
      rect.bottom + REF_HOVER_GAP,
      REF_HOVER_MARGIN,
      Math.max(
        REF_HOVER_MARGIN,
        window.innerHeight - REF_HOVER_MAX_HEIGHT - REF_HOVER_MARGIN
      )
    );
    setRefHover(
      fitsAbove
        ? {
            message,
            left,
            top: null,
            bottom: window.innerHeight - rect.top + REF_HOVER_GAP,
          }
        : { message, left, top: belowTop, bottom: null }
    );
  }

  function onLogMouseOver(event: MouseEvent) {
    const target = event.target;
    if (!(target instanceof Element)) return;
    const anchor = target.closest("a.msg-ref");
    if (!(anchor instanceof HTMLElement)) return;
    // mouseover re-fires when crossing descendant nodes; ignore repeats on the
    // pill already tracked so the show timer is not restarted.
    if (anchor === refHoverAnchor) return;
    hideRefHover();
    refHoverAnchor = anchor;
    refHoverTimer = window.setTimeout(() => {
      refHoverTimer = undefined;
      showRefHover(anchor);
    }, REF_HOVER_DELAY_MS);
  }

  function onLogMouseOut(event: MouseEvent) {
    if (!refHoverAnchor) return;
    const target = event.target;
    if (!(target instanceof Element)) return;
    if (target.closest("a.msg-ref") !== refHoverAnchor) return;
    // Still inside the same pill (moved onto a descendant node): not a leave.
    const related = event.relatedTarget;
    if (related instanceof Node && refHoverAnchor.contains(related)) return;
    hideRefHover();
  }

  function onLogFocusIn(event: FocusEvent) {
    const target = event.target;
    if (!(target instanceof Element)) return;
    const anchor = target.closest("a.msg-ref");
    if (!(anchor instanceof HTMLElement)) return;
    hideRefHover();
    refHoverAnchor = anchor;
    showRefHover(anchor);
  }

  function onLogFocusOut(event: FocusEvent) {
    if (!refHoverAnchor) return;
    const related = event.relatedTarget;
    if (related instanceof Node && refHoverAnchor.contains(related)) return;
    hideRefHover();
  }

  // Reference anchors live inside Rust-rendered fragment HTML. Media references
  // include backend-filled preview metadata; shift-click keeps the jump action.
  function onLogClick(event: MouseEvent) {
    hideRefHover();
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
    const ts = Number(anchor.dataset.ts ?? "0");
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
    if (!ScreenShareDecoder.supported()) {
      setShareError(streamId, "This browser does not support WebCodecs screen-share playback");
      setShareStates((prev) => ({ ...prev, [streamId]: "failed" }));
      return;
    }
    if (socket && socket.readyState === WebSocket.OPEN) {
      clearShareError(streamId);
      setShareStates((prev) => ({ ...prev, [streamId]: "connecting" }));
      setPlaying((prev) => (prev.includes(streamId) ? prev : [...prev, streamId]));
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

  function sendJson(req: ClientRequest): boolean {
    if (socket && socket.readyState === WebSocket.OPEN) {
      socket.send(JSON.stringify(req));
      return true;
    }
    return false;
  }

  function sendRequest(
    build: (requestId: number) => ClientRequest
  ): Promise<void> {
    if (!socket || socket.readyState !== WebSocket.OPEN) {
      return Promise.reject(new Error("Not connected to the local Chatt client"));
    }
    const requestId = nextRequestId++;
    return new Promise((resolve, reject) => {
      const timer = window.setTimeout(() => {
        pendingRequests.delete(requestId);
        reject(new Error("Chatt did not acknowledge the request; input was retained"));
      }, REQUEST_TIMEOUT_MS);
      pendingRequests.set(requestId, { resolve, reject, timer });
      try {
        socket!.send(JSON.stringify(build(requestId)));
      } catch (error) {
        clearTimeout(timer);
        pendingRequests.delete(requestId);
        reject(error instanceof Error ? error : new Error(String(error)));
      }
    });
  }

  // Cancel an outgoing upload or skip an incoming download, by the transfer id
  // the placeholder message carries as its `file_id`.
  function abortTransfer(transferId: number) {
    void sendRequest((requestId) => ({
      type: "abort_transfer",
      request_id: requestId,
      transfer_id: transferId,
    })).catch((error) => setComposeError(error.message));
  }

  function restoreParkedComposer(edit: PendingWebEdit) {
    batch(() => {
      setEditing(null);
      setDraft(edit.parkedDraft);
      setQueued(edit.parkedFiles);
      setDragActive(false);
    });
    queueMicrotask(resizeComposer);
  }

  function cancelEdit() {
    const edit = editing();
    if (!edit) return;
    restoreParkedComposer(edit);
  }

  function beginEdit(message: WebMessage) {
    const current = editing();
    const parkedDraft = current?.parkedDraft ?? draft();
    const parkedFiles = current?.parkedFiles ?? queued();
    batch(() => {
      setEditing({
        target: message.message_id,
        original: message.body,
        parkedDraft,
        parkedFiles,
      });
      setDraft(message.body);
      setQueued([]);
      setDragActive(false);
    });
    queueMicrotask(() => {
      resizeComposer();
      composerTextEl?.focus();
      composerTextEl?.setSelectionRange(message.body.length, message.body.length);
    });
  }

  function deleteMessage(message: WebMessage, opener: HTMLButtonElement) {
    deleteOpener = opener;
    setPendingDelete(message);
  }

  function closeDeleteConfirmation(restoreFocus = true) {
    setPendingDelete(null);
    const opener = deleteOpener;
    deleteOpener = undefined;
    if (restoreFocus) {
      queueMicrotask(() => opener?.isConnected && opener.focus());
    }
  }

  async function confirmDelete() {
    const message = pendingDelete();
    if (!message) return;
    try {
      await sendRequest((requestId) => ({
        type: "delete_message",
        request_id: requestId,
        target: message.message_id,
      }));
      closeDeleteConfirmation(false);
      if (editing()?.target === message.message_id) cancelEdit();
    } catch (error) {
      showDeleteError(error instanceof Error ? error.message : String(error));
    }
  }

  function showDeleteError(message: string) {
    setDeleteError(message);
    if (deleteErrorTimer !== undefined) clearTimeout(deleteErrorTimer);
    deleteErrorTimer = window.setTimeout(() => {
      deleteErrorTimer = undefined;
      setDeleteError(null);
    }, 5000);
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
    const uploadId = nextUploadId++;
    if (file.size > maxUploadBytes()) {
      throw new Error(
        `${file.name} is ${formatBytes(file.size)}; the upload limit is ${formatBytes(maxUploadBytes())}`
      );
    }
    const ws = openSocketOrThrow();
    debugUpload("start", {
      upload_id: uploadId,
      name: file.name,
      size: file.size,
      buffered_amount: ws.bufferedAmount,
    });
    try {
      await sendRequest((requestId) => ({
        type: "upload_start",
        request_id: requestId,
        upload_id: uploadId,
        name: file.name,
        size: file.size,
      }));
    } catch (error) {
      if (socket?.readyState === WebSocket.OPEN) {
        void sendRequest((requestId) => ({
          type: "upload_cancel",
          request_id: requestId,
          upload_id: uploadId,
        })).catch(() => {});
      }
      throw error;
    }
    setStaging({ uploadId, file, sent: 0, cancelled: false });
    for (let offset = 0; offset < file.size; offset += UPLOAD_CHUNK_BYTES) {
      if (staging()?.cancelled) {
        throw new Error(`Staging ${file.name} was cancelled`);
      }
      const end = Math.min(file.size, offset + UPLOAD_CHUNK_BYTES);
      const chunk = new Uint8Array(await file.slice(offset, end).arrayBuffer());
      const frame = new Uint8Array(4 + chunk.length);
      new DataView(frame.buffer).setUint32(0, uploadId, true);
      frame.set(chunk, 4);
      const current = openSocketOrThrow();
      current.send(frame);
      setStaging((active) =>
        active?.uploadId === uploadId ? { ...active, sent: end } : active
      );
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
    await sendRequest((requestId) => ({
      type: "upload_finish",
      request_id: requestId,
      upload_id: uploadId,
    }));
    debugUpload("finish", {
      upload_id: uploadId,
      name: file.name,
      size: file.size,
      buffered_amount: current.bufferedAmount,
    });
    setStaging(null);
  }

  async function sendQueuedFiles(files: File[]) {
    for (const file of files) {
      await sendFile(file);
      setQueued((current) => current.filter((candidate) => candidate !== file));
    }
  }

  async function cancelStaging() {
    const active = staging();
    if (!active || active.cancelled) return;
    setStaging({ ...active, cancelled: true });
    try {
      await sendRequest((requestId) => ({
        type: "upload_cancel",
        request_id: requestId,
        upload_id: active.uploadId,
      }));
      setQueued((current) => current.filter((file) => file !== active.file));
    } catch (error) {
      setComposeError(error instanceof Error ? error.message : String(error));
    }
  }

  async function submitCompose() {
    if (!connected() || submitting()) return;
    setComposeError(null);
    const edit = editing();
    if (edit) {
      const body = draft();
      if (!body.trim()) return;
      if (body === edit.original) {
        restoreParkedComposer(edit);
        return;
      }
      setSubmitting(true);
      try {
        await sendRequest((requestId) => ({
          type: "edit_message",
          request_id: requestId,
          target: edit.target,
          body,
        }));
        restoreParkedComposer(edit);
      } catch (error) {
        setComposeError(error instanceof Error ? error.message : String(error));
      } finally {
        setSubmitting(false);
      }
      return;
    }

    const body = draft().trim();
    const files = queued();
    if (!body && files.length === 0) return;
    setSubmitting(true);
    try {
      if (body) {
        await sendRequest((requestId) => ({
          type: "send_message",
          request_id: requestId,
          body,
        }));
        setDraft("");
        queueMicrotask(resizeComposer);
      }
      if (files.length > 0) await sendQueuedFiles(files);
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      setComposeError(message);
      setStaging(null);
      debugUpload("error", { error: message });
    } finally {
      setSubmitting(false);
    }
  }

  function onComposeKeyDown(event: KeyboardEvent) {
    if (event.key === "Escape" && editing()) {
      event.preventDefault();
      cancelEdit();
      return;
    }
    if (
      event.key === "Enter" &&
      !event.shiftKey &&
      !event.isComposing &&
      event.keyCode !== 229
    ) {
      event.preventDefault();
      submitCompose();
    }
  }

  function onComposeDragOver(event: DragEvent) {
    if (editing()) return;
    event.preventDefault();
    setDragActive(true);
  }

  function onComposeDragLeave(event: DragEvent) {
    event.preventDefault();
    setDragActive(false);
  }

  function queueFiles(files: File[]) {
    const accepted = files.filter((file) => file.size <= maxUploadBytes());
    const rejected = files.find((file) => file.size > maxUploadBytes());
    if (rejected) {
      setComposeError(
        `${rejected.name} is ${formatBytes(rejected.size)}; the upload limit is ${formatBytes(maxUploadBytes())}`
      );
    }
    if (accepted.length > 0) setQueued((prev) => [...prev, ...accepted]);
  }

  function onComposeDrop(event: DragEvent) {
    if (editing()) return;
    event.preventDefault();
    setDragActive(false);
    const files = event.dataTransfer
      ? Array.from(event.dataTransfer.files)
      : [];
    if (files.length > 0) queueFiles(files);
  }

  function openComposeFileDialog() {
    if (editing()) return;
    composerFileInputEl?.click();
  }

  function onComposeFileInput(
    event: Event & { currentTarget: HTMLInputElement }
  ) {
    if (editing()) return;
    const files = Array.from(event.currentTarget.files ?? []);
    if (files.length > 0) queueFiles(files);
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
    if (editing()) return;
    const files = pastedImageFiles(event.clipboardData);
    if (files.length === 0) return;
    event.preventDefault();
    queueFiles(files);
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
    hideRefHover();
    const now = debugNow();
    const requestThreshold = topRequestThreshold();
    const rearmThreshold = topRearmThreshold();
    const nearTop = offset < rearmThreshold;
    const scrollLogInterval = nearTop ? 100 : 500;
    if (scrollDebugActive && now - lastScrollDebugAt >= scrollLogInterval) {
      lastScrollDebugAt = now;
      debugScrollState("scroll", {
        offset,
        nearTop,
        atTopThreshold: offset < requestThreshold,
        requestThreshold,
        rearmThreshold,
      });
    }

    if (!prependSettling && offset > rearmThreshold && !topPagingArmed) {
      topPagingArmed = true;
      debugScrollState("top-rearmed", { offset, rearmThreshold });
    }

    // Page older history only on a genuine user-driven crossing into the top
    // zone. Virtua's prepend shift and resize compensation also emit scroll
    // events near the top, and treating those as demand can replay a page loop.
    const canRequestOlderFromScroll =
      hasMore &&
      !loadingOlder &&
      topPagingArmed &&
      userDriving &&
      !suppress &&
      !refJumping &&
      !prependSettling &&
      offset < requestThreshold;
    if (canRequestOlderFromScroll) {
      if (requestOlder("scroll")) topPagingArmed = false;
    } else if (
      scrollDebugActive &&
      hasMore &&
      offset < requestThreshold &&
      now - lastTopBlockedDebugAt >= 250
    ) {
      lastTopBlockedDebugAt = now;
      debugScrollState("top-request-blocked", {
        offset,
        requestThreshold,
        rearmThreshold,
        blockedBy: {
          disarmed: !topPagingArmed,
          noUser: !userDriving,
          suppress,
          refJumping,
          prependSettling,
        },
      });
    }

    // Rule 1: ignore our own pin() scroll and any scroll not under user control
    // (virtua resize-jump compensation). Only a real user scroll flips follow.
    if (suppress) return;
    if (!userDriving) return;
    following = atBottom();
    if (following) setNewMessageCount(0);
  }

  // virtua's onScrollEnd fires ~150ms after scrolling stops. Release user
  // control shortly after so a later spontaneous compensation scroll is not
  // mis-attributed to the user.
  function onScrollEnd() {
    debugScrollState("scroll-end");
    if (idleTimer) clearTimeout(idleTimer);
    idleTimer = window.setTimeout(() => {
      userDriving = false;
      debugScrollState("user-idle");
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
      if (socket !== ws) return;
      debugSocket("open", { url });
      setConnected(true);
      hideConnectionError();
    };
    ws.onmessage = (ev) => {
      if (socket !== ws) return;
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
          debugScrollState("sync-received", {
            count: feed.messages.length,
            frameOldestSeq: feed.oldest_seq,
            frameHasMore: feed.has_more,
          });
          preloadRecentImages(feed.messages);
          closeDeleteConfirmation(false);
          refPreviewCache.clear();
          setMessages(feed.messages);
          oldestSeq = feed.oldest_seq;
          hasMore = feed.has_more;
          loadingOlder = false;
          prependSettling = false;
          clearPrependSettleFrame();
          topPagingArmed = true;
          following = true;
          setNewMessageCount(0);
          pin();
        } else if (feed.kind === "older") {
          debugScrollState("older-received", {
            count: feed.messages.length,
            frameOldestSeq: feed.oldest_seq,
            frameHasMore: feed.has_more,
            loadingOlderBefore: loadingOlder,
            firstOlder: debugMessageKey(feed.messages[0]),
            lastOlder: debugMessageKey(feed.messages[feed.messages.length - 1]),
          });
          loadingOlder = false;
          if (feed.messages.length > 0) {
            preloadRecentImages(feed.messages);
            holdPrependSettling();
            setPrepend(true);
            setMessages((prev) => [...feed.messages, ...prev]);
            debugScrollState("older-applied", {
              count: feed.messages.length,
              frameOldestSeq: feed.oldest_seq,
              frameHasMore: feed.has_more,
            });
          }
          oldestSeq = feed.oldest_seq;
          hasMore = feed.has_more;
          debugScrollState("older-cursor-updated", {
            count: feed.messages.length,
            frameOldestSeq: feed.oldest_seq,
            frameHasMore: feed.has_more,
          });
          scheduleResumePendingJump();
        } else if (feed.kind === "ref_preview") {
          onRefPreview(feed.ts, feed.mid, feed.message);
        } else if (feed.kind === "delete") {
          invalidateMessageReference(feed.message_id);
          if (editing()?.target === feed.message_id) cancelEdit();
          if (pendingDelete()?.message_id === feed.message_id) {
            closeDeleteConfirmation(false);
          }
          setMessages((prev) =>
            prev.filter((message) => message.message_id !== feed.message_id)
          );
          pin();
        } else {
          // A live message. Upsert by the announcement timestamp and file id;
          // transfer ids are reused after server restarts, while the pair
          // identifies one file.
          const msg = feed.message;
          if (msg.edited) {
            invalidateMessageReference(msg.message_id);
            if (editing()?.target === msg.message_id) cancelEdit();
          }
          preloadImage(msg);
          if (msg.attachment?.kind === "video" && autoplay() !== "disabled") {
            msg.autoplay = autoplay();
          }
          let appended = false;
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
            } else if (msg.message_id !== 0) {
              const i = prev.findIndex(
                (m) => m.message_id === msg.message_id
              );
              if (i >= 0) {
                const next = prev.slice();
                next[i] = msg;
                return next;
              }
              // An edit of a target outside this tab's loaded window updates
              // the server-side backlog but must not appear as a new tail row.
              if (msg.edited) return prev;
            }
            appended = true;
            return [...prev, msg];
          });
          if (appended && !following) {
            setNewMessageCount((count) => count + 1);
          }
          pin();
        }
        return;
      }
      const env: ServerEnvelope = JSON.parse(ev.data);
      if (env.type === "share_available") {
        setShareStates((prev) =>
          env.stream_id in prev ? prev : { ...prev, [env.stream_id]: "available" }
        );
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
        // targets share_config to every requesting tab so one that joined after
        // the share started can bootstrap its decoder. Treat the config as
        // a fresh bootstrap point even if a decoder already exists: the server is
        // about to fast-start from a keyframe, and any stale decode queue would
        // otherwise keep the canvas pinned on old frames.
        const canvas = canvases.get(env.stream_id);
        if (canvas) {
          clearShareError(env.stream_id);
          decoders.get(env.stream_id)?.close();
          const decoder = new ScreenShareDecoder(canvas, {
            waiting: () =>
              setShareStates((prev) => ({ ...prev, [env.stream_id]: "waiting-for-keyframe" })),
            playing: () =>
              setShareStates((prev) => ({ ...prev, [env.stream_id]: "playing" })),
            failed: (message) => {
              setShareError(env.stream_id, message);
              setShareStates((prev) => ({ ...prev, [env.stream_id]: "failed" }));
              setPlaying((prev) => prev.filter((id) => id !== env.stream_id));
            },
          });
          decoders.set(env.stream_id, decoder);
          void decoder.configure(env.codec, new Uint8Array(env.extradata));
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
            progress: {
              transferred: env.transferred,
              total: env.total,
              direction: env.direction,
            },
          };
          return next;
        });
      } else if (env.type === "file_terminal") {
        // The transfer ended without landing: replace any progress bar with a
        // persistent terminal label on the still-placeholder message.
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
            progress: undefined,
            terminal: { verb: env.verb, reason: env.reason },
          };
          return next;
        });
      } else if (env.type === "config") {
        if (env.readonly) {
          cancelEdit();
          closeDeleteConfirmation(false);
        }
        setReadonly(env.readonly);
        setAutoplay(env.autoplay);
        setViewer(env.viewer);
        setMaxUploadBytes(env.max_upload_bytes);
        document.title = `Chatt | ${env.room_name}`;
      } else if (env.type === "room") {
        document.title = `Chatt | ${env.name}`;
      } else if (env.type === "request_result") {
        const pending = pendingRequests.get(env.request_id);
        if (pending) {
          pendingRequests.delete(env.request_id);
          clearTimeout(pending.timer);
          if (env.accepted) pending.resolve();
          else pending.reject(new Error(env.message ?? `${env.operation} was rejected`));
        }
      } else if (env.type === "action_error") {
        setComposeError(`${env.operation.replace(/_/g, " ")} failed: ${env.message}`);
      } else if (env.type === "share_error") {
        setShareError(env.stream_id, env.message);
        setShareStates((prev) => ({ ...prev, [env.stream_id]: "failed" }));
        setPlaying((prev) => prev.filter((id) => id !== env.stream_id));
      } else if (env.type === "delete_error") {
        if (pendingDelete()?.message_id === env.target) {
          closeDeleteConfirmation(false);
        }
        showDeleteError(env.message);
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
      if (socket !== ws) return;
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
      closeDeleteConfirmation(false);
      loadingOlder = false;
      topPagingArmed = true;
      setStaging(null);
      for (const pending of pendingRequests.values()) {
        clearTimeout(pending.timer);
        pending.reject(new Error("Connection lost before Chatt accepted the request"));
      }
      pendingRequests.clear();
      for (const decoder of decoders.values()) decoder.close();
      decoders.clear();
      setPlaying([]);
      scheduleConnectionError();
      reconnectTimer = window.setTimeout(connect, 1000);
    };
    ws.onerror = (event) => {
      if (socket !== ws) return;
      console.warn("[chatt:ws] error", event);
      debugSocket("error");
      ws.close();
    };
    socket = ws;
  }

  onMount(() => {
    previewMedia = window.matchMedia("(max-width: 820px)");
    const updateCompactPreview = () => setCompactPreview(previewMedia!.matches);
    updateCompactPreview();
    previewMedia.onchange = updateCompactPreview;
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
    if (previewMedia) {
      previewMedia.onchange = null;
    }
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
    clearPrependSettleFrame();
    if (refToastTimer) clearTimeout(refToastTimer);
    if (deleteErrorTimer !== undefined) clearTimeout(deleteErrorTimer);
    if (refHoverTimer !== undefined) clearTimeout(refHoverTimer);
    for (const decoder of decoders.values()) decoder.close();
    for (const pending of pendingRequests.values()) clearTimeout(pending.timer);
    pendingRequests.clear();
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
      <Show when={deleteError()}>
        <div class="action-error" role="alert">
          <span>Delete failed: {deleteError()}</span>
          <button
            class="action-error-dismiss"
            type="button"
            aria-label="Dismiss deletion error"
            onClick={() => setDeleteError(null)}
          >
            <Icon name="x" />
          </button>
        </div>
      </Show>
      <Show when={pendingDelete()}>
        {(message) => (
          <DeleteConfirmation
            message={message()}
            onCancel={closeDeleteConfirmation}
            onConfirm={confirmDelete}
          />
        )}
      </Show>
      <Show when={refHover()}>
        {(hover) => <RefHoverCard hover={hover()} />}
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
          inert={compactPreview() && !!activePreview()}
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
                states={shareStates()}
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
            onMouseOver={onLogMouseOver}
            onMouseOut={onLogMouseOut}
            onFocusIn={onLogFocusIn}
            onFocusOut={onLogFocusOut}
          >
            <div class="chat-log-content" ref={contentEl}>
              <Virtualizer
                ref={(h) => (handle = h)}
                scrollRef={logEl}
                data={messageList().visible}
                itemSize={(message) => {
                  const group = messageList().groups.get(message)!;
                  return estimateMessageRowSize(
                    message,
                    group,
                    messageEstimateLayout()
                  );
                }}
                endMargin={CHAT_END_MARGIN_PX}
                shift={prepend()}
                onScroll={onScroll}
                onScrollEnd={onScrollEnd}
              >
                {(message) => (
                  <div class="message-item">
                    <Show when={showsDateSeparator(message)}>
                      <div class="date-separator" role="separator" aria-label={formatDateLabel(message.timestamp_ms)}>
                        <span>{formatDateLabel(message.timestamp_ms)}</span>
                      </div>
                    </Show>
                    <MessageRow
                      message={message}
                      group={messageList().groups.get(message)!}
                      onToggleGroup={toggleMessageGroup}
                      onOpenPreview={openPreview}
                      onQuoteRef={readonly() ? undefined : quoteRef}
                      onAbortTransfer={readonly() ? undefined : abortTransfer}
                      onEdit={readonly() ? undefined : beginEdit}
                      onDelete={readonly() ? undefined : deleteMessage}
                      autoplay={message.autoplay ?? "disabled"}
                    />
                  </div>
                )}
              </Virtualizer>
            </div>
            <Show when={newMessageCount() > 0}>
              <button class="new-messages" type="button" onClick={jumpToLatest}>
                {newMessageCount()} new {newMessageCount() === 1 ? "message" : "messages"}
              </button>
            </Show>
          </div>
          <Show when={!readonly()}>
            <section
              class="composer"
              classList={{
                "is-drag-active": dragActive(),
                "is-editing": !!editing(),
              }}
              onDragOver={onComposeDragOver}
              onDragLeave={onComposeDragLeave}
              onDrop={onComposeDrop}
            >
              <Show when={editing()}>
                <div class="composer-editing" role="status">
                  <span>Editing message</span>
                  <button
                    class="composer-edit-cancel"
                    type="button"
                    aria-label="Cancel message edit"
                    title="Cancel edit"
                    onClick={cancelEdit}
                  >
                    <Icon name="x" />
                  </button>
                </div>
              </Show>
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
              <Show when={staging()}>
                {(active) => (
                  <div class="composer-staging" role="status" aria-live="polite">
                    <span>
                      Staging {active().file.name}: {formatBytes(active().sent)} / {formatBytes(active().file.size)}
                    </span>
                    <progress value={active().sent} max={Math.max(1, active().file.size)} />
                    <button type="button" onClick={cancelStaging} disabled={active().cancelled}>
                      {active().cancelled ? "Cancelling…" : "Cancel"}
                    </button>
                  </div>
                )}
              </Show>
              <Show when={composeError()}>
                <div class="composer-error" role="alert">
                  {composeError()} Input and unaccepted files were retained.
                </div>
              </Show>
              <Show when={!connected()}>
                <div class="composer-offline" role="status">Offline — your draft is retained; sending is disabled.</div>
              </Show>
              <div class="composer-input-row">
                <Show when={!editing()}>
                  <button
                    class="composer-attach"
                    type="button"
                    aria-label="Attach files"
                    title="Attach files"
                    onClick={openComposeFileDialog}
                    disabled={submitting()}
                  >
                    <Icon name="plus" />
                  </button>
                </Show>
                <label class="visually-hidden" for="composer-message">Message</label>
                <textarea
                  id="composer-message"
                  class="composer-text"
                  ref={composerTextEl}
                  rows={1}
                  placeholder={editing() ? "Edit message…" : "Write a message…"}
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
                  modal={compactPreview()}
                />
              </aside>
            </>
          )}
        </Show>
      </div>
    </div>
  );
}
