import { expect, test } from "bun:test";
import { estimateMediaBoxHeight } from "../src/media-layout";

const layout = { contentWidth: 720, mediaMaxHeight: 460 };

test("estimates border-box media geometry without upscaling", () => {
  expect(estimateMediaBoxHeight(1920, 1080, layout)).toBe(411);
  expect(estimateMediaBoxHeight(320, 180, layout)).toBe(192);
  expect(estimateMediaBoxHeight(1920, 20, layout)).toBe(20);
});

test("caps portrait and tall media at the CSS viewport limit", () => {
  expect(estimateMediaBoxHeight(1080, 1920, layout)).toBe(460);
  expect(estimateMediaBoxHeight(640, 1000, layout)).toBe(460);
});

test("rejects missing or invalid media geometry", () => {
  expect(estimateMediaBoxHeight(null, 1080, layout)).toBeNull();
  expect(estimateMediaBoxHeight(1920, 0, layout)).toBeNull();
});

test("video sizing keeps the custom stylesheet override contract", async () => {
  const css = await Bun.file(new URL("../src/styles.css", import.meta.url)).text();
  const root = css.match(/:root\s*\{([\s\S]*?)\}/)?.[1] ?? "";
  expect(root).not.toMatch(/--media-video-height\s*:/);
  expect(css).toContain(".media-video-sized {");
  expect(css).toContain("height: var(--media-video-height, auto);");
  expect(css).toContain("max-height: var(--media-video-height, 50vh);");
  expect(css).toContain(".media-video-unsized {");
  expect(css).not.toMatch(/\.media-video\.[\w-]+\s*\{/);
});
