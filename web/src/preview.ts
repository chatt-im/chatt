import type { AutoplayMode } from "./types";

type PreviewIdentity = {
  file_id: number;
  timestamp_ms: number;
};

export type PreviewItem = PreviewIdentity &
  (
    | { kind: "file"; name: string }
    | { kind: "video"; name: string }
    | { kind: "audio"; name: string }
    | {
        kind: "image";
        name: string;
        width: number | null;
        height: number | null;
      }
  );

export function previewKey(item: PreviewItem): string {
  return `${item.timestamp_ms}:${item.file_id}`;
}

export function promotePreviewHistory(
  current: PreviewItem[],
  item: PreviewItem,
  limit: number,
): PreviewItem[] {
  const key = previewKey(item);
  const existing = current.find((candidate) => previewKey(candidate) === key);
  const nextItem = existing ?? item;
  if (current[0] === nextItem) return current;
  return [
    nextItem,
    ...current.filter((candidate) => previewKey(candidate) !== key),
  ].slice(0, limit);
}

export function previewKind(
  value: string | undefined,
): PreviewItem["kind"] | null {
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

export function optionalPreviewNumber(value: string | undefined): number | null {
  if (!value) return null;
  const parsed = Number(value);
  return Number.isFinite(parsed) ? parsed : null;
}

export function previewIdentityNumber(value: string | undefined): number | null {
  if (!value) return null;
  const parsed = Number(value);
  return Number.isSafeInteger(parsed) && parsed >= 0 ? parsed : null;
}

export function standalonePreviewFromSearch(params: URLSearchParams): {
  item: PreviewItem;
  autoplay: AutoplayMode;
} | null {
  const kind = previewKind(params.get("preview") ?? undefined);
  const name = params.get("name");
  const file_id = previewIdentityNumber(params.get("file_id") ?? undefined);
  const timestamp_ms = previewIdentityNumber(
    params.get("timestamp_ms") ?? undefined,
  );
  if (!kind || !name || file_id === null || timestamp_ms === null) return null;

  const autoplayValue = params.get("autoplay");
  const autoplay: AutoplayMode =
    autoplayValue === "muted" || autoplayValue === "with-audio"
      ? autoplayValue
      : "disabled";
  if (kind !== "image") {
    return { item: { file_id, timestamp_ms, kind, name }, autoplay };
  }

  return {
    item: {
      file_id,
      timestamp_ms,
      kind,
      name,
      width: optionalPreviewNumber(params.get("width") ?? undefined),
      height: optionalPreviewNumber(params.get("height") ?? undefined),
    },
    autoplay,
  };
}

export function standalonePreviewUrl(
  item: PreviewItem,
  autoplay: AutoplayMode,
  baseUrl: string,
): string {
  const url = new URL("/", baseUrl);
  url.searchParams.set("preview", item.kind);
  url.searchParams.set("name", item.name);
  url.searchParams.set("file_id", String(item.file_id));
  url.searchParams.set("timestamp_ms", String(item.timestamp_ms));
  url.searchParams.set("autoplay", autoplay);
  if (item.kind === "image") {
    if (item.width !== null) url.searchParams.set("width", String(item.width));
    if (item.height !== null)
      url.searchParams.set("height", String(item.height));
  }
  return url.href;
}
