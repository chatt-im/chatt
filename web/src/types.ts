// Mirrors the Rust DTOs. Chat messages arrive as binary feed frames (decoded in
// `feed.ts`, encoded in `src/web_wire.rs`); share control stays JSON text.

export type MediaKind = "image" | "video" | "audio" | "file";

export interface WebAttachment {
  // Served file name. The URL is `/files/${name}`.
  name: string;
  kind: MediaKind;
  // Intrinsic pixel size, present for images with a readable header. The view
  // reserves the box from these so a decoding image never grows the layout.
  width: number | null;
  height: number | null;
}

// One piece of a message body. Prose renders as markdown; a code block renders
// from its precomputed highlight spans (see `highlight.ts`). A code fragment's
// `text` is UTF-8 bytes, because the spans are byte offsets into it.
export type Fragment =
  | { kind: "text"; text: string }
  | { kind: "code"; lang: string; text: Uint8Array; spans: Uint8Array };

export interface WebMessage {
  id: number;
  sender: string;
  timestamp_ms: number;
  attachment: WebAttachment | null;
  // The file transfer id for a file message, else null. A message with both
  // file_id and timestamp_ms matching one already held replaces it in place;
  // transfer ids alone are reused after server restarts.
  file_id: number | null;
  // The body pre-split into prose and code fragments.
  fragments: Fragment[];
}

// One JSON object per WebSocket text frame. Chat sync/message/older frames are
// binary now, so only screen-share control travels as JSON.
export type ServerEnvelope =
  // A room member started sharing their screen. The browser shows a play button.
  | {
      type: "share_available";
      stream_id: number;
      sender: string;
      codec: string;
      width: number;
      height: number;
      extradata: number[];
    }
  // Playback started for a share; configure the decoder with this codec and the
  // `extra_data` descriptor (avcC/hvcC).
  | { type: "share_config"; stream_id: number; codec: string; extradata: number[] }
  // A share ended; tear down its decoder.
  | { type: "share_ended"; stream_id: number }
  // A play request failed; show the message on the share's row.
  | { type: "share_error"; stream_id: number; message: string };

// A screen share this browser can watch.
export interface ShareInfo {
  stream_id: number;
  sender: string;
  codec: string;
  width: number;
  height: number;
}

// Frames the browser sends: paging requests and screen-share playback control.
export type ClientRequest =
  | { type: "load_older"; before_seq: number; limit: number }
  | { type: "play_share"; stream_id: number }
  | { type: "stop_share"; stream_id: number };
