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
///
/// Firefox cannot always initialise a hardware H.264 decoder and, unlike Chrome,
/// does not fall back to software on its own: the hardware attempt errors before
/// the first frame decodes. So a hardware failure that happens before any frame
/// is drawn retries once with software decoding preferred, replaying the frames
/// buffered since the last keyframe so the picture recovers immediately.
export class ScreenShareDecoder {
  private decoder: VideoDecoder | null = null;
  // Skip delta frames until the first keyframe so the decoder starts cleanly.
  private sawKey = false;
  // Config retained so a hardware-decode failure can be retried in software.
  private codec = "";
  private description = new Uint8Array(0);
  private preferSoftware = false;
  // True once the decoder has drawn a frame. Until then the frames since the
  // last keyframe are retained so a hardware failure can replay them in software.
  private decoded = false;
  private replay: EncodedVideoChunk[] = [];

  constructor(private canvas: HTMLCanvasElement) {}

  /// True when WebCodecs is available in this browser.
  static supported(): boolean {
    return typeof VideoDecoder !== "undefined";
  }

  /// Configures the decoder for `codec`, with `description` the `extra_data`
  /// descriptor (empty when the stream needs none). Any prior decoder is closed.
  configure(codec: string, description: Uint8Array) {
    this.close();
    this.codec = codec;
    this.description = description;
    this.preferSoftware = false;
    console.info("[screenshare] configure", { codec, descriptorBytes: description.length });
    this.start();
  }

  // Creates the decoder for the current codec, descriptor, and acceleration
  // preference, resetting the keyframe and replay state for the fresh decoder.
  private start() {
    const ctx = this.canvas.getContext("2d");
    this.decoder = new VideoDecoder({
      output: (decoded) => {
        if (!this.decoded) {
          console.info("[screenshare] first frame decoded", {
            software: this.preferSoftware,
            width: decoded.displayWidth,
            height: decoded.displayHeight,
          });
        }
        this.decoded = true;
        this.replay.length = 0;
        if (ctx) {
          if (this.canvas.width !== decoded.displayWidth) this.canvas.width = decoded.displayWidth;
          if (this.canvas.height !== decoded.displayHeight)
            this.canvas.height = decoded.displayHeight;
          ctx.drawImage(decoded, 0, 0);
        }
        decoded.close();
      },
      error: (error) => this.onError(error),
    });
    const config: VideoDecoderConfig = { codec: this.codec, optimizeForLatency: true };
    if (this.description.length > 0) config.description = this.description;
    if (this.preferSoftware) config.hardwareAcceleration = "prefer-software";
    console.info("[screenshare] start decoder", {
      codec: config.codec,
      software: this.preferSoftware,
      hardwareAcceleration: config.hardwareAcceleration ?? "(default)",
    });
    try {
      this.decoder.configure(config);
    } catch (error) {
      console.error("[screenshare] configure threw", error);
    }
    this.sawKey = false;
    this.decoded = false;
  }

  // Retries in software when a hardware decoder fails before drawing a frame,
  // replaying the frames since the last keyframe. A later error, or one after a
  // software retry, is terminal and only logged.
  private onError(error: DOMException) {
    console.warn("[screenshare] decoder error", {
      decoded: this.decoded,
      alreadySoftware: this.preferSoftware,
      state: this.decoder?.state,
      buffered: this.replay.length,
      message: error.message,
    });
    if (this.decoded || this.preferSoftware) {
      console.error("[screenshare] decode failed (terminal)", error);
      return;
    }
    console.warn("[screenshare] hardware decode failed, falling back to software");
    this.preferSoftware = true;
    const chunks = this.replay;
    this.replay = [];
    this.start();
    console.info("[screenshare] replaying", chunks.length, "buffered chunks in software");
    for (const chunk of chunks) {
      if (chunk.type === "key") this.sawKey = true;
      try {
        this.decoder?.decode(chunk);
      } catch (replayError) {
        console.error("[screenshare] replay decode threw", replayError);
      }
    }
  }

  decode(frame: VideoFrame) {
    if (!this.decoder || this.decoder.state === "closed") {
      console.warn("[screenshare] decode skipped, decoder not ready", {
        hasDecoder: !!this.decoder,
        state: this.decoder?.state,
      });
      return;
    }
    if (!this.sawKey) {
      if (!frame.isKey) return;
      this.sawKey = true;
    }
    const chunk = new EncodedVideoChunk({
      type: frame.isKey ? "key" : "delta",
      timestamp: frame.tsMs * 1000,
      data: frame.data,
    });
    // Retain frames since the last keyframe until the first successful decode so
    // a hardware failure can replay them in software.
    if (!this.decoded) {
      if (frame.isKey) this.replay.length = 0;
      this.replay.push(chunk);
    }
    try {
      this.decoder.decode(chunk);
    } catch (error) {
      console.error("[screenshare] decode threw", { type: chunk.type, error });
    }
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
    this.decoded = false;
    this.replay.length = 0;
  }
}
