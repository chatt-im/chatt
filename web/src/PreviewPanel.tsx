import {
  createEffect,
  createSignal,
  For,
  onCleanup,
  onMount,
  Show,
} from "solid-js";
import FileViewer from "./FileViewer";
import Icon from "./Icon";
import ImageViewer from "./ImageViewer";
import VideoPlayer from "./VideoPlayer";
import type { IconName } from "./icons";
import { previewKey, type PreviewItem } from "./preview";
import type { AutoplayMode } from "./types";

function fileUrl(name: string): string {
  return `/files/${encodeURIComponent(name)}`;
}

function previewIcon(item: PreviewItem): IconName {
  switch (item.kind) {
    case "image":
      return "image";
    case "file":
      return "file-text";
    case "video":
    case "audio":
      return "play";
  }
}

export default function PreviewPanel(props: {
  history: PreviewItem[];
  active: PreviewItem;
  activeKey: string;
  onSelect: (key: string) => void;
  onClose: () => void;
  onCloseTab: (key: string) => void;
  autoplay: AutoplayMode;
  standalone?: boolean;
  modal?: boolean;
}) {
  let panelEl: HTMLDivElement | undefined;
  let tabsEl: HTMLDivElement | undefined;
  let tabsResizeObserver: ResizeObserver | undefined;
  const tabButtons = new Map<string, HTMLButtonElement>();
  const [hasOverflowBefore, setHasOverflowBefore] = createSignal(false);
  const [hasOverflowAfter, setHasOverflowAfter] = createSignal(false);
  const [searchOpen, setSearchOpen] = createSignal(false);
  const [copied, setCopied] = createSignal(false);
  const [activeFileText, setActiveFileText] = createSignal<string | null>(null);
  let copiedReset: number | undefined;

  function updateOverflowEdges() {
    if (!tabsEl) return;
    const maxScroll = Math.max(0, tabsEl.scrollWidth - tabsEl.clientWidth);
    setHasOverflowBefore(tabsEl.scrollLeft > 1);
    setHasOverflowAfter(tabsEl.scrollLeft < maxScroll - 1);
  }

  function revealTab(button: HTMLButtonElement, behavior: ScrollBehavior = "smooth") {
    if (!tabsEl) return;
    const left = button.offsetLeft;
    const right = left + button.offsetWidth;
    const visibleLeft = tabsEl.scrollLeft;
    const visibleRight = visibleLeft + tabsEl.clientWidth;
    if (left < visibleLeft) tabsEl.scrollTo({ left, behavior });
    else if (right > visibleRight) {
      tabsEl.scrollTo({ left: right - tabsEl.clientWidth, behavior });
    }
  }

  function selectTab(key: string, focus = false) {
    props.onSelect(key);
    const button = tabButtons.get(key);
    if (!button) return;
    revealTab(button);
    if (focus) button.focus({ preventScroll: true });
  }

  function onTabKeyDown(event: KeyboardEvent, index: number) {
    let nextIndex: number;
    switch (event.key) {
      case "ArrowLeft":
        nextIndex = index > 0 ? index - 1 : props.history.length - 1;
        break;
      case "ArrowRight":
        nextIndex = index < props.history.length - 1 ? index + 1 : 0;
        break;
      case "Home":
        nextIndex = 0;
        break;
      case "End":
        nextIndex = props.history.length - 1;
        break;
      default:
        return;
    }
    event.preventDefault();
    const item = props.history[nextIndex];
    if (item) selectTab(previewKey(item), true);
  }

  function onTabsWheel(event: WheelEvent) {
    if (!tabsEl || Math.abs(event.deltaX) >= Math.abs(event.deltaY)) return;
    const maxScroll = tabsEl.scrollWidth - tabsEl.clientWidth;
    if (maxScroll <= 0) return;
    event.preventDefault();
    tabsEl.scrollLeft += event.deltaY;
    updateOverflowEdges();
  }

  function onPanelKeyDown(event: KeyboardEvent) {
    if (event.key === "Escape" && !props.standalone) {
      event.preventDefault();
      props.onClose();
      return;
    }
    if (event.key !== "Tab" || !props.modal || !panelEl) return;
    const focusable = Array.from(
      panelEl.querySelectorAll<HTMLElement>(
        'button:not([disabled]), a[href], input:not([disabled]), [tabindex]:not([tabindex="-1"])'
      )
    );
    if (focusable.length === 0) return;
    const first = focusable[0]!;
    const last = focusable[focusable.length - 1]!;
    if (event.shiftKey && document.activeElement === first) {
      event.preventDefault();
      last.focus();
    } else if (!event.shiftKey && document.activeElement === last) {
      event.preventDefault();
      first.focus();
    }
  }

  onMount(() => {
    if (props.modal) queueMicrotask(() => panelEl?.focus());
    if (!tabsEl) return;
    tabsResizeObserver = new ResizeObserver(updateOverflowEdges);
    tabsResizeObserver.observe(tabsEl);
    updateOverflowEdges();
  });

  // Direct opens are promoted to index zero. Keep that newest tab visible;
  // history navigation leaves order untouched and only reveals the selected tab.
  createEffect(() => {
    const history = props.history;
    const activeKey = props.activeKey;
    const currentKeys = new Set(history.map(previewKey));
    for (const key of tabButtons.keys()) {
      if (!currentKeys.has(key)) tabButtons.delete(key);
    }
    queueMicrotask(() => {
      if (!tabsEl) return;
      const activeIndex = history.findIndex((item) => previewKey(item) === activeKey);
      if (activeIndex === 0) tabsEl.scrollTo({ left: 0, behavior: "smooth" });
      else {
        const button = tabButtons.get(activeKey);
        if (button) revealTab(button);
      }
      updateOverflowEdges();
    });
  });

  createEffect(() => {
    props.activeKey;
    setSearchOpen(false);
    setCopied(false);
    setActiveFileText(null);
  });

  async function copyActiveFile() {
    const text = activeFileText();
    if (text === null) return;
    try {
      await navigator.clipboard.writeText(text);
      setCopied(true);
      if (copiedReset !== undefined) window.clearTimeout(copiedReset);
      copiedReset = window.setTimeout(() => setCopied(false), 1500);
    } catch (error) {
      console.warn("[chatt:clipboard] file copy failed", error);
    }
  }

  onCleanup(() => {
    tabsResizeObserver?.disconnect();
    if (copiedReset !== undefined) window.clearTimeout(copiedReset);
    tabButtons.clear();
  });

  const imageUsesStandaloneToolbar = () =>
    props.standalone && props.active.kind === "image";

  return (
    <div
      class="preview-panel"
      classList={{ "is-standalone": props.standalone }}
      ref={panelEl}
      role={props.modal ? "dialog" : undefined}
      aria-modal={props.modal ? "true" : undefined}
      aria-label={props.modal ? `Preview ${props.active.name}` : undefined}
      tabIndex={props.modal ? -1 : undefined}
      onKeyDown={onPanelKeyDown}
    >
      <Show when={!imageUsesStandaloneToolbar()}>
        <div class="preview-panel-head">
          <Show
            when={props.active.kind === "file" && activeFileText() !== null}
          >
            <button
              class="preview-panel-action preview-panel-search"
              type="button"
              aria-label="Search file"
              aria-pressed={searchOpen()}
              title="Search file"
              onClick={() => setSearchOpen((open) => !open)}
            >
              <Icon name="search" />
            </button>
          </Show>
          <Show when={!props.standalone}>
            <div
              class="preview-tabs-frame"
              classList={{
                "has-overflow-before": hasOverflowBefore(),
                "has-overflow-after": hasOverflowAfter(),
              }}
            >
              <div
                class="preview-tabs"
                ref={tabsEl}
                role="tablist"
                aria-label="Preview history"
                onScroll={updateOverflowEdges}
                onWheel={onTabsWheel}
              >
                <For each={props.history}>
                  {(item, index) => {
                    const key = previewKey(item);
                    const selected = () => key === props.activeKey;
                    return (
                      <div
                        class="preview-tab"
                        classList={{ "is-active": selected() }}
                      >
                        <button
                          class="preview-tab-select"
                          ref={(element) => {
                            tabButtons.set(key, element);
                          }}
                          id={`preview-tab-${index()}`}
                          type="button"
                          role="tab"
                          aria-selected={selected()}
                          aria-controls="preview-panel-content"
                          tabIndex={selected() ? 0 : -1}
                          title={item.name}
                          onClick={() => selectTab(key)}
                          onKeyDown={(event) => onTabKeyDown(event, index())}
                        >
                          <Icon name={previewIcon(item)} />
                          <span class="preview-tab-label">{item.name}</span>
                        </button>
                        <button
                          class="preview-tab-close"
                          type="button"
                          aria-label={`Close ${item.name}`}
                          title="Close"
                          onClick={() => props.onCloseTab(key)}
                        >
                          <Icon name="x" />
                        </button>
                      </div>
                    );
                  }}
                </For>
              </div>
            </div>
          </Show>
          <div class="preview-panel-actions">
            <Show
              when={props.active.kind === "file" && activeFileText() !== null}
            >
              <button
                class="preview-panel-action"
                type="button"
                aria-label={`Copy ${props.active.name}`}
                title={copied() ? "Copied" : "Copy file"}
                onClick={copyActiveFile}
              >
                <Icon name={copied() ? "check" : "copy"} />
              </button>
            </Show>
            <a
              class="preview-panel-action"
              href={`/files/${encodeURIComponent(props.active.name)}`}
              download={props.active.name}
              aria-label={`Download ${props.active.name}`}
              title="Download"
            >
              <Icon name="download" />
            </a>
            <button
              class="preview-panel-action"
              type="button"
              aria-label="Close preview"
              title="Close"
              onClick={props.onClose}
            >
              <Icon name="x" />
            </button>
          </div>
        </div>
      </Show>
      <div
        class="preview-panel-content"
        id="preview-panel-content"
        role="tabpanel"
        aria-labelledby={
          props.standalone
            ? undefined
            : `preview-tab-${props.history.findIndex(
                (item) => previewKey(item) === props.activeKey,
              )}`
        }
      >
        <Show when={props.active} keyed>
          {(item) =>
            item.kind === "image" ? (
              <ImageViewer
                name={item.name}
                width={item.width}
                height={item.height}
                standaloneActions={
                  props.standalone ? { onClose: props.onClose } : undefined
                }
              />
            ) : item.kind === "video" ? (
              <VideoPlayer
                class="preview-media-video"
                src={fileUrl(item.name)}
                autoplay={props.autoplay}
              />
            ) : item.kind === "audio" ? (
              <div class="preview-media-audio-frame">
                <audio
                  class="preview-media-audio"
                  src={fileUrl(item.name)}
                  controls
                  preload="metadata"
                />
              </div>
            ) : (
              <FileViewer
                name={item.name}
                searchOpen={searchOpen()}
                onCloseSearch={() => setSearchOpen(false)}
                onTextLoaded={setActiveFileText}
              />
            )
          }
        </Show>
      </div>
    </div>
  );
}
