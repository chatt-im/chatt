// WebCodecs decode for the screen-share live path.
//
// The chatt client forwards each decrypted frame body over the WebSocket as a
// binary message: a 17-byte header (`[u32 size_incl_header][i64 ts_ms][u8
// is_key][u32 stream_id]`) followed by the length-prefixed video bitstream.
//
// All codec knowledge lives in the Rust client: it derives the codec string and
// the `extra_data` descriptor (`avcC`/`hvcC`) from the stream's parameter sets
// and converts each access unit to the length-prefixed form the decoder expects.
// This decoder is therefore codec-agnostic. It configures from the codec string
// and descriptor in the `share_config` envelope and feeds each frame body
// straight through, with no in-browser NAL parsing.

const VIDEO_FRAME_HEADER_LEN = 17;

export interface VideoFrame {
  tsMs: number;
  isKey: boolean;
  streamId: number;
  data: Uint8Array;
}

/// Parses one binary frame message: its 17-byte header then the bitstream body.
/// Returns `null` when the buffer is too short or its size field is malformed.
export function parseFrame(buffer: ArrayBuffer): VideoFrame | null {
  if (buffer.byteLength < VIDEO_FRAME_HEADER_LEN) return null;
  const view = new DataView(buffer);
  const size = view.getUint32(0, true);
  if (size < VIDEO_FRAME_HEADER_LEN || size > buffer.byteLength) return null;
  const tsMs = Number(view.getBigInt64(4, true));
  const isKey = view.getUint8(12) === 1;
  const streamId = view.getUint32(13, true);
  const data = new Uint8Array(buffer, VIDEO_FRAME_HEADER_LEN, size - VIDEO_FRAME_HEADER_LEN);
  return { tsMs, isKey, streamId, data };
}

/// Drives a `VideoDecoder` for one screen share, drawing decoded frames to a
/// canvas. It configures from the codec string and `extra_data` descriptor the
/// client supplies, then feeds each frame body straight to the decoder.
export class ScreenShareDecoder {
  private decoder: VideoDecoder | null = null;
  // Skip delta frames until the first keyframe so the decoder starts cleanly.
  private sawKey = false;

  constructor(private canvas: HTMLCanvasElement) {}

  /// True when WebCodecs is available in this browser.
  static supported(): boolean {
    return typeof VideoDecoder !== "undefined";
  }

  /// Configures the decoder for `codec`, with `description` the `extra_data`
  /// descriptor (empty when the stream needs none). Any prior decoder is closed.
  configure(codec: string, description: Uint8Array) {
    this.close();
    const ctx = this.canvas.getContext("2d");
    this.decoder = new VideoDecoder({
      output: (decoded) => {
        if (ctx) {
          if (this.canvas.width !== decoded.displayWidth) this.canvas.width = decoded.displayWidth;
          if (this.canvas.height !== decoded.displayHeight)
            this.canvas.height = decoded.displayHeight;
          ctx.drawImage(decoded, 0, 0);
        }
        decoded.close();
      },
      error: (error) => console.error("video decoder error", error),
    });
    const config: VideoDecoderConfig = { codec, optimizeForLatency: true };
    if (description.length > 0) config.description = description;
    this.decoder.configure(config);
    this.sawKey = false;
  }

  decode(frame: VideoFrame) {
    if (!this.decoder || this.decoder.state === "closed") return;
    if (!this.sawKey) {
      if (!frame.isKey) return;
      this.sawKey = true;
    }
    this.decoder.decode(
      new EncodedVideoChunk({
        type: frame.isKey ? "key" : "delta",
        timestamp: frame.tsMs * 1000,
        data: frame.data,
      }),
    );
  }

  close() {
    if (this.decoder && this.decoder.state !== "closed") {
      try {
        this.decoder.close();
      } catch {
        // A decoder that never decoded throws on close; ignore.
      }
    }
    this.decoder = null;
    this.sawKey = false;
  }
}
