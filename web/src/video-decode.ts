// WebCodecs decode for the screen-share live path.
//
// The chatt client forwards each decrypted frame body over the WebSocket as a
// binary message: a 13-byte header (`[u32 size_incl_header][i64 ts_ms][u8 is_key]`)
// followed by H.264 Annex-B. Browsers vary in whether they accept Annex-B
// directly, so this configures the decoder in avcC mode: it extracts the SPS and
// PPS from the first keyframe, builds an `AVCDecoderConfigurationRecord`
// (`description`), derives the codec string from the SPS, and feeds every access
// unit as length-prefixed NALs. This matches what mature WebCodecs H.264 players
// do and avoids the "encoding is not supported" configure failure that Annex-B
// mode hits on some browsers.

const VIDEO_FRAME_HEADER_LEN = 13;

const NAL_SEI = 6;
const NAL_SPS = 7;
const NAL_PPS = 8;
const NAL_AUD = 9;

export interface VideoFrame {
  tsMs: number;
  isKey: boolean;
  data: Uint8Array;
}

/// Parses one binary frame message: its 13-byte header then the H.264 body.
/// Returns `null` when the buffer is too short or its size field is malformed.
export function parseFrame(buffer: ArrayBuffer): VideoFrame | null {
  if (buffer.byteLength < VIDEO_FRAME_HEADER_LEN) return null;
  const view = new DataView(buffer);
  const size = view.getUint32(0, true);
  if (size < VIDEO_FRAME_HEADER_LEN || size > buffer.byteLength) return null;
  const tsMs = Number(view.getBigInt64(4, true));
  const isKey = view.getUint8(12) === 1;
  const data = new Uint8Array(buffer, VIDEO_FRAME_HEADER_LEN, size - VIDEO_FRAME_HEADER_LEN);
  return { tsMs, isKey, data };
}

/// Splits an Annex-B access unit into NAL bodies (without start codes), trimming
/// the extra leading zero of a 4-byte start code, mirroring the Rust splitter.
function splitNals(data: Uint8Array): Uint8Array[] {
  const starts: number[] = [];
  let i = 0;
  while (i + 3 <= data.length) {
    if (data[i] === 0 && data[i + 1] === 0 && data[i + 2] === 1) {
      starts.push(i + 3);
      i += 3;
    } else {
      i += 1;
    }
  }
  const nals: Uint8Array[] = [];
  for (let p = 0; p < starts.length; p++) {
    const begin = starts[p];
    let end: number;
    if (p + 1 < starts.length) {
      const nextCode = starts[p + 1] - 3;
      end = nextCode > 0 && data[nextCode - 1] === 0 ? nextCode - 1 : nextCode;
    } else {
      end = data.length;
    }
    if (end > begin) nals.push(data.subarray(begin, end));
  }
  return nals;
}

// Profile_idc values whose avcC record carries the High-profile extension bytes
// (chroma_format, bit depths, SPS-ext count). Firefox validates these strictly.
const HIGH_PROFILE_IDCS = [100, 110, 122, 144, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135];

/// Builds an `AVCDecoderConfigurationRecord` (avcC) from one SPS and PPS NAL.
///
/// For High-family profiles (profile_idc 100, 110, 122, 144, ...) ISO 14496-15
/// requires four trailing bytes after the parameter sets. Omitting them makes the
/// record invalid; Chrome tolerates it but Firefox rejects the config. 4:2:0
/// 8-bit is assumed (`chroma_format_idc = 1`, bit depths = 8), which matches the
/// `yuv420p`/`nv12` capture; other chroma/bit-depth would need an SPS parse.
function buildAvcc(sps: Uint8Array, pps: Uint8Array): Uint8Array {
  const out: number[] = [];
  out.push(1); // configurationVersion
  out.push(sps[1]); // AVCProfileIndication (profile_idc)
  out.push(sps[2]); // profile_compatibility (constraint flags)
  out.push(sps[3]); // AVCLevelIndication (level_idc)
  out.push(0xff); // 6 reserved bits set + lengthSizeMinusOne = 3 (4-byte lengths)
  out.push(0xe1); // 3 reserved bits set + numOfSequenceParameterSets = 1
  out.push((sps.length >> 8) & 0xff, sps.length & 0xff);
  out.push(...sps);
  out.push(1); // numOfPictureParameterSets
  out.push((pps.length >> 8) & 0xff, pps.length & 0xff);
  out.push(...pps);
  if (HIGH_PROFILE_IDCS.includes(sps[1])) {
    out.push(0xfc | 1); // 6 reserved bits + chroma_format_idc = 1 (4:2:0)
    out.push(0xf8 | 0); // 5 reserved bits + bit_depth_luma_minus8 = 0
    out.push(0xf8 | 0); // 5 reserved bits + bit_depth_chroma_minus8 = 0
    out.push(0); // numOfSequenceParameterSetExt
  }
  return new Uint8Array(out);
}

/// Rewrites an access unit's NALs into the length-prefixed (avcC) bitstream the
/// decoder expects once a `description` is supplied. Each kept NAL gains a 4-byte
/// big-endian length prefix in place of its start code.
///
/// SPS, PPS, AUD, and SEI NALs are dropped: the parameter sets live in the
/// `description`, and Firefox's avcC decoder rejects an access unit that carries
/// them in band (Chrome tolerates it). This mirrors libra's `annex_b_to_avc`.
function toAvccChunk(nals: Uint8Array[]): Uint8Array {
  const kept = nals.filter((nal) => {
    const type = nal[0] & 0x1f;
    return type !== NAL_SEI && type !== NAL_SPS && type !== NAL_PPS && type !== NAL_AUD;
  });
  let size = 0;
  for (const nal of kept) size += 4 + nal.length;
  const out = new Uint8Array(size);
  let offset = 0;
  for (const nal of kept) {
    out[offset] = (nal.length >>> 24) & 0xff;
    out[offset + 1] = (nal.length >>> 16) & 0xff;
    out[offset + 2] = (nal.length >>> 8) & 0xff;
    out[offset + 3] = nal.length & 0xff;
    offset += 4;
    out.set(nal, offset);
    offset += nal.length;
  }
  return out;
}

function hex(byte: number): string {
  return byte.toString(16).padStart(2, "0").toUpperCase();
}

/// Drives a `VideoDecoder` for one screen share, drawing decoded frames to a
/// canvas. It configures lazily from the first keyframe's SPS/PPS in avcC mode
/// (the cross-browser path libra uses): the parameter sets go in the
/// `description` and frames are fed as length-prefixed VCL NALs.
export class ScreenShareDecoder {
  private decoder: VideoDecoder | null = null;
  private configured = false;

  constructor(private canvas: HTMLCanvasElement) {}

  /// True when WebCodecs is available in this browser.
  static supported(): boolean {
    return typeof VideoDecoder !== "undefined";
  }

  /// Discards any existing decoder so the next keyframe reconfigures cleanly.
  reset() {
    this.close();
  }

  decode(frame: VideoFrame) {
    const nals = splitNals(frame.data);
    if (!this.configured) {
      if (!frame.isKey) return;
      const sps = nals.find((nal) => (nal[0] & 0x1f) === NAL_SPS);
      const pps = nals.find((nal) => (nal[0] & 0x1f) === NAL_PPS);
      if (!sps || !pps) return;
      const codec = `avc1.${hex(sps[1])}${hex(sps[2])}${hex(sps[3])}`;
      const description = buildAvcc(sps, pps);
      console.log("screenshare codec", codec, "avcC", [...description].map(hex).join(""));
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
      this.decoder.configure({ codec, description, optimizeForLatency: true });
      this.configured = true;
    }

    if (!this.decoder || this.decoder.state === "closed") return;
    const data = toAvccChunk(nals);
    if (data.length === 0) return;
    this.decoder.decode(
      new EncodedVideoChunk({
        type: frame.isKey ? "key" : "delta",
        timestamp: frame.tsMs * 1000,
        data,
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
    this.configured = false;
  }
}
