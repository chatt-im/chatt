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
import type { AutoplayMode } from "./types";

export type PreviewItem =
  | { kind: "file"; name: string }
  | { kind: "video"; name: string }
  | { kind: "audio"; name: string }
  | {
      kind: "image";
      name: string;
      width: number | null;
      height: number | null;
    };

export function previewKey(item: PreviewItem): string {
  return `${item.kind}:${item.name}`;
}

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
}) {
  let tabsEl: HTMLDivElement | undefined;
  let tabsResizeObserver: ResizeObserver | undefined;
  const tabButtons = new Map<string, HTMLButtonElement>();
  const [hasOverflowBefore, setHasOverflowBefore] = createSignal(false);
  const [hasOverflowAfter, setHasOverflowAfter] = createSignal(false);

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

  onMount(() => {
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

  onCleanup(() => {
    tabsResizeObserver?.disconnect();
    tabButtons.clear();
  });

  const imageUsesStandaloneToolbar = () =>
    props.standalone && props.active.kind === "image";

  return (
    <div
      class="preview-panel"
      classList={{ "is-standalone": props.standalone }}
    >
      <Show when={!imageUsesStandaloneToolbar()}>
        <div class="preview-panel-head">
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
            <a
              class="preview-panel-download"
              href={`/files/${encodeURIComponent(props.active.name)}`}
              download={props.active.name}
              aria-label={`Download ${props.active.name}`}
              title="Download"
            >
              <Icon name="download" />
            </a>
            <button
              class="preview-panel-close"
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
              <FileViewer name={item.name} />
            )
          }
        </Show>
      </div>
    </div>
  );
}
