import { expect, test } from "bun:test";
import { decodeFeed } from "../src/feed";

function u8(bytes: number[], value: number) {
  bytes.push(value);
}

function u32(bytes: number[], value: number) {
  bytes.push(value, value >>> 8, value >>> 16, value >>> 24);
}

function u64(bytes: number[], value: number) {
  u32(bytes, value >>> 0);
  u32(bytes, Math.floor(value / 0x1_0000_0000));
}

function string(bytes: number[], value: string) {
  const encoded = new TextEncoder().encode(value);
  u32(bytes, encoded.length);
  bytes.push(...encoded);
}

test("decodes durable attachment identity from a message frame", () => {
  const bytes = [0, 0, 0, 0, 2];
  u64(bytes, 37);
  u64(bytes, 8_000);
  u64(bytes, 91);
  string(bytes, "");
  string(bytes, "Alice");
  string(bytes, "sent file");
  u8(bytes, 0);
  u8(bytes, 0);
  u8(bytes, 0);
  u8(bytes, 1);
  string(bytes, "clip.mp4");
  u8(bytes, 1);
  u64(bytes, 37);
  u64(bytes, 8_000);
  u8(bytes, 0);
  u8(bytes, 1);
  u64(bytes, 37);
  u32(bytes, 0);

  const frame = decodeFeed(Uint8Array.from(bytes).buffer);

  expect(frame?.kind).toBe("message");
  if (!frame || frame.kind !== "message") throw new Error("expected message frame");
  expect(frame.message.attachment).toEqual({
    file_id: 37,
    timestamp_ms: 8_000,
    name: "clip.mp4",
    kind: "video",
    width: null,
    height: null,
  });
});
