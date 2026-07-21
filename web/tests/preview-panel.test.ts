import { describe, expect, test } from "bun:test";
import {
  previewKey,
  promotePreviewHistory,
  standalonePreviewFromSearch,
  standalonePreviewUrl,
  type PreviewItem,
} from "../src/preview";

function video(
  file_id: number,
  timestamp_ms: number,
  name = "clip.mp4",
): PreviewItem {
  return { file_id, timestamp_ms, kind: "video", name };
}

describe("attachment preview identity", () => {
  test("distinguishes same-name uploads by transfer id", () => {
    expect(previewKey(video(10, 1_000))).not.toBe(previewKey(video(11, 1_000)));
  });

  test("distinguishes a reused transfer id by announcement timestamp", () => {
    expect(previewKey(video(10, 1_000))).not.toBe(previewKey(video(10, 2_000)));
  });

  test("promotes same-name uploads as independent preview tabs", () => {
    const first = video(10, 1_000);
    const second = video(11, 1_000);

    const history = promotePreviewHistory(
      promotePreviewHistory([], first, 8),
      second,
      8,
    );

    expect(history).toEqual([second, first]);
  });

  test("deduplicates the same upload independently of its display name", () => {
    const original = video(10, 1_000, "clip.mp4");
    const renamed = video(10, 1_000, "clip-1.mp4");

    const history = promotePreviewHistory([original], renamed, 8);

    expect(history).toEqual([original]);
  });

  test("standalone preview links round-trip the durable upload identity", () => {
    const item = video(10, 1_000);
    const url = standalonePreviewUrl(item, "muted", "http://localhost:8080/chat");

    const parsed = standalonePreviewFromSearch(new URL(url).searchParams);

    expect(parsed).toEqual({ item, autoplay: "muted" });
  });
});
