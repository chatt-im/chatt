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
const MAX_DECODE_QUEUE = 2;
const MAX_PENDING_FRAMES = 90;

export interface VideoFrame {
  tsMs: number;
  isKey: boolean;
  streamId: number;
  data: Uint8Array;
}

export interface DecoderEvents {
  waiting?: () => void;
  playing?: () => void;
  failed?: (message: string) => void;
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
  private pending: VideoFrame[] = [];
  private skippedBeforeKey = 0;
  private warnedWaitingForKey = false;
  // Timestamp of the last frame accepted for decode, to flag duplicated or
  // reordered frames (a delta decoded twice, or on the wrong reference,
  // corrupts the picture until the next keyframe).
  private lastAcceptedTsMs = Number.NEGATIVE_INFINITY;
  // Highest chunk timestamp (microseconds) handed to the decoder, so the
  // output callback can tell an intermediate catch-up frame from the newest.
  private newestSubmittedUs = Number.NEGATIVE_INFINITY;

  private terminal = false;
  private generation = 0;

  constructor(private canvas: HTMLCanvasElement, private events: DecoderEvents = {}) {}

  /// True when WebCodecs is available in this browser.
  static supported(): boolean {
    return typeof VideoDecoder !== "undefined";
  }

  /// Configures the decoder for `codec`, with `description` the `extra_data`
  /// descriptor (empty when the stream needs none). Any prior decoder is closed.
  async configure(codec: string, description: Uint8Array): Promise<void> {
    this.close();
    const generation = this.generation;
    if (!ScreenShareDecoder.supported()) {
      this.fail("This browser does not support WebCodecs screen-share playback");
      return;
    }
    this.codec = codec;
    this.description = description;
    this.preferSoftware = false;
    console.info("[screenshare] configure", { codec, descriptorBytes: description.length });
    const config: VideoDecoderConfig = { codec, optimizeForLatency: true };
    if (description.length > 0) config.description = description;
    try {
      const support = await VideoDecoder.isConfigSupported(config);
      if (generation !== this.generation) return;
      if (!support.supported) {
        this.fail(`The browser cannot decode ${codec}`);
        return;
      }
    } catch (error) {
      if (generation !== this.generation) return;
      this.fail(error instanceof Error ? error.message : String(error));
      return;
    }
    this.terminal = false;
    this.events.waiting?.();
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
        this.events.playing?.();
        this.replay.length = 0;
        // While catching up (fast-start burst, or decode falling behind), more
        // frames are already queued behind this one. Drawing each of them would
        // flash the whole backlog across the canvas and canvas draws are slow,
        // so skip intermediate frames and draw only the newest one. The newest
        // submitted frame itself always draws, so the canvas never sticks on a
        // stale frame when the stream then goes quiet.
        const catchingUp =
          this.pending.length > 0 || decoded.timestamp < this.newestSubmittedUs;
        if (ctx && !catchingUp) {
          if (this.canvas.width !== decoded.displayWidth) this.canvas.width = decoded.displayWidth;
          if (this.canvas.height !== decoded.displayHeight)
            this.canvas.height = decoded.displayHeight;
          ctx.drawImage(decoded, 0, 0);
        }
        decoded.close();
        this.pump();
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
      this.fail(error instanceof Error ? error.message : String(error));
    }
    this.sawKey = false;
    this.decoded = false;
    this.pending.length = 0;
    this.skippedBeforeKey = 0;
    this.warnedWaitingForKey = false;
    this.lastAcceptedTsMs = Number.NEGATIVE_INFINITY;
    this.newestSubmittedUs = Number.NEGATIVE_INFINITY;
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
      pending: this.pending.length,
      message: error.message,
    });
    if (this.decoded || this.preferSoftware) {
      console.error("[screenshare] decode failed (terminal)", error);
      this.fail(error.message || "Screen-share decoding failed");
      return;
    }
    console.warn("[screenshare] hardware decode failed, falling back to software");
    this.preferSoftware = true;
    const chunks = this.replay;
    // The frames still queued behind the failed decoder are part of the same
    // GOP; dropping them would leave a gap after the replayed chunks and the
    // stream would decode corrupt until the next keyframe. Capture them before
    // start() clears the queue and requeue them after the replay.
    const pending = this.pending;
    this.replay = [];
    this.pending = [];
    this.start();
    console.info(
      "[screenshare] replaying in software",
      { chunks: chunks.length, pending: pending.length },
    );
    for (const chunk of chunks) {
      if (chunk.type === "key") this.sawKey = true;
      this.newestSubmittedUs = Math.max(this.newestSubmittedUs, chunk.timestamp);
      try {
        this.decoder?.decode(chunk);
      } catch (replayError) {
        console.error("[screenshare] replay decode threw", replayError);
      }
    }
    this.pending = pending;
    this.pump();
  }

  decode(frame: VideoFrame) {
    if (this.terminal) return;
    if (!this.decoder || this.decoder.state === "closed") {
      console.warn("[screenshare] decode skipped, decoder not ready", {
        hasDecoder: !!this.decoder,
        state: this.decoder?.state,
      });
      return;
    }
    if (this.pending.length >= MAX_PENDING_FRAMES) {
      console.warn("[screenshare] pending frame limit reached; waiting for a fresh keyframe", {
        pending: this.pending.length,
        streamId: frame.streamId,
      });
      this.closeDecoderOnly();
      this.start();
      this.events.waiting?.();
      if (!frame.isKey) return;
      this.sawKey = true;
    }
    if (!this.sawKey) {
      if (!frame.isKey) {
        this.skippedBeforeKey += 1;
        if (this.skippedBeforeKey === 1 || this.skippedBeforeKey % 120 === 0) {
          this.events.waiting?.();
          console.warn("[screenshare] waiting for keyframe", {
            skipped: this.skippedBeforeKey,
            streamId: frame.streamId,
            tsMs: frame.tsMs,
          });
        }
        return;
      }
      this.sawKey = true;
      this.skippedBeforeKey = 0;
      console.info("[screenshare] first keyframe received", {
        streamId: frame.streamId,
        tsMs: frame.tsMs,
        bytes: frame.data.byteLength,
      });
    }
    if (!frame.isKey && frame.tsMs <= this.lastAcceptedTsMs) {
      console.warn("[screenshare] non-monotonic delta frame", {
        streamId: frame.streamId,
        tsMs: frame.tsMs,
        lastTsMs: this.lastAcceptedTsMs,
      });
    }
    this.lastAcceptedTsMs = frame.tsMs;
    if (frame.isKey && this.shouldRestartAtKeyframe()) {
      console.warn("[screenshare] restarting decoder at fresh keyframe", {
        streamId: frame.streamId,
        tsMs: frame.tsMs,
        pending: this.pending.length,
        decodeQueue: this.decoder?.decodeQueueSize ?? 0,
      });
      this.restartAtKeyframe(frame);
      return;
    }
    this.pending.push(frame);
    this.warnIfWaitingForCatchup();
    this.pump();
  }

  private shouldRestartAtKeyframe(): boolean {
    if (!this.decoded || !this.decoder || this.decoder.state === "closed") return false;
    return (
      this.pending.length > MAX_PENDING_FRAMES ||
      this.decoder.decodeQueueSize > MAX_DECODE_QUEUE
    );
  }

  private restartAtKeyframe(frame: VideoFrame) {
    this.closeDecoderOnly();
    this.start();
    this.sawKey = true;
    this.pending.push(frame);
    this.pump();
  }

  private warnIfWaitingForCatchup() {
    if (this.warnedWaitingForKey || this.pending.length <= MAX_PENDING_FRAMES) return;
    this.warnedWaitingForKey = true;
    console.warn("[screenshare] decoder is behind; waiting for a keyframe to catch up", {
      pending: this.pending.length,
      decodeQueue: this.decoder?.decodeQueueSize ?? 0,
    });
  }

  private pump() {
    if (!this.decoder || this.decoder.state === "closed") return;
    while (this.pending.length > 0 && this.decoder.decodeQueueSize < MAX_DECODE_QUEUE) {
      const frame = this.pending.shift();
      if (!frame) return;
      this.decodeNow(frame);
      if (!this.decoder) return;
    }
  }

  private decodeNow(frame: VideoFrame) {
    if (!this.decoder || this.decoder.state === "closed") return;
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
    this.newestSubmittedUs = Math.max(this.newestSubmittedUs, chunk.timestamp);
    try {
      this.decoder.decode(chunk);
    } catch (error) {
      console.error("[screenshare] decode threw", { type: chunk.type, error });
      this.fail(error instanceof Error ? error.message : String(error));
    }
  }

  close() {
    this.generation += 1;
    this.closeDecoderOnly();
    this.sawKey = false;
    this.decoded = false;
    this.replay.length = 0;
    this.pending.length = 0;
    this.skippedBeforeKey = 0;
    this.warnedWaitingForKey = false;
  }

  private fail(message: string) {
    this.terminal = true;
    this.closeDecoderOnly();
    this.replay.length = 0;
    this.pending.length = 0;
    this.events.failed?.(message);
  }

  private closeDecoderOnly() {
    if (this.decoder && this.decoder.state !== "closed") {
      try {
        this.decoder.close();
      } catch {
        // A decoder that never decoded throws on close; ignore.
      }
    }
    this.decoder = null;
  }
}
