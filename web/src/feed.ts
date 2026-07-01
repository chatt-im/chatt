// Decoder for the binary chat feed frames (see `src/web_wire.rs`).
//
// Feed frames are binary WebSocket messages that begin with a four-byte zero
// sentinel and a kind byte. A video frame never begins with a zero u32 (its
// first field is a length), so `decodeFeed` returns null for one, letting the
// caller treat it as video. All integers are little-endian.

import type { WebMessage, Fragment, MediaKind } from "./types";

const KIND_SYNC = 1;
const KIND_MESSAGE = 2;

const FRAG_TEXT = 0;

const MEDIA_KINDS: MediaKind[] = ["image", "video", "audio", "file"];

const decoder = new TextDecoder();

export type FeedFrame =
  | { kind: "sync"; messages: WebMessage[]; oldest_seq: number; has_more: boolean }
  | { kind: "older"; messages: WebMessage[]; oldest_seq: number; has_more: boolean }
  | { kind: "message"; message: WebMessage };

// Decodes a feed frame, or returns null when the buffer is not one (a video
// frame), so the caller falls back to the video path.
export function decodeFeed(buffer: ArrayBuffer): FeedFrame | null {
  const view = new DataView(buffer);
  if (buffer.byteLength < 5 || view.getUint32(0, true) !== 0) return null;
  const reader = new Reader(view, new Uint8Array(buffer), 5);
  const kind = view.getUint8(4);
  if (kind === KIND_MESSAGE) {
    return { kind: "message", message: reader.message() };
  }
  const oldest_seq = reader.u53();
  const has_more = reader.u8() === 1;
  const count = reader.u32();
  const messages: WebMessage[] = [];
  for (let i = 0; i < count; i++) messages.push(reader.message());
  return { kind: kind === KIND_SYNC ? "sync" : "older", messages, oldest_seq, has_more };
}

class Reader {
  constructor(
    private view: DataView,
    private bytes: Uint8Array,
    private pos: number,
  ) {}

  u8(): number {
    return this.view.getUint8(this.pos++);
  }

  u32(): number {
    const value = this.view.getUint32(this.pos, true);
    this.pos += 4;
    return value;
  }

  // Reads a u64 as a JS number. Sequence numbers, ids, and timestamps stay well
  // inside 2^53 for this app.
  u53(): number {
    const lo = this.view.getUint32(this.pos, true);
    const hi = this.view.getUint32(this.pos + 4, true);
    this.pos += 8;
    return hi * 0x1_0000_0000 + lo;
  }

  slice(): Uint8Array {
    const len = this.u32();
    const out = this.bytes.subarray(this.pos, this.pos + len);
    this.pos += len;
    return out;
  }

  string(): string {
    return decoder.decode(this.slice());
  }

  message(): WebMessage {
    const id = this.u53();
    const timestamp_ms = this.u53();
    const sender = this.string();
    let attachment: WebMessage["attachment"] = null;
    if (this.u8() === 1) {
      const name = this.string();
      const kind = MEDIA_KINDS[this.u8()] ?? "file";
      let width: number | null = null;
      let height: number | null = null;
      if (this.u8() === 1) {
        width = this.u32();
        height = this.u32();
      }
      attachment = { name, kind, width, height };
    }
    const file_id = this.u8() === 1 ? this.u53() : null;
    const fragmentCount = this.u32();
    const fragments: Fragment[] = [];
    for (let i = 0; i < fragmentCount; i++) {
      if (this.u8() === FRAG_TEXT) {
        fragments.push({ kind: "text", text: this.string() });
      } else {
        // Keep the code bytes rather than a string: highlight spans are byte
        // offsets, so per-run byte slicing is what renders correctly.
        const lang = this.string();
        const text = this.slice().slice();
        const spans = this.slice().slice();
        fragments.push({ kind: "code", lang, text, spans });
      }
    }
    return { id, sender, timestamp_ms, attachment, file_id, fragments };
  }
}
