// Mirrors the Rust DTOs in `src/web_server.rs`. Keep the two in sync.

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

export interface WebMessage {
  id: number;
  sender: string;
  body: string;
  timestamp_ms: number;
  attachment: WebAttachment | null;
  // The file transfer id for a file message, else null. A `message` frame with a
  // file_id matching one already held replaces it in place (the announcement
  // placeholder gains its inline attachment) instead of appending a second row.
  file_id: number | null;
}

// One JSON object per WebSocket text frame.
//
// `oldest_seq` is the server-assigned sequence number of the first message in a
// window. `has_more` is true when still-older history can be paged in. The
// browser requests older history with a `load_older` frame (see ClientRequest).
export type ServerEnvelope =
  | { type: "sync"; messages: WebMessage[]; oldest_seq: number; has_more: boolean }
  | { type: "message"; message: WebMessage }
  | { type: "older"; messages: WebMessage[]; oldest_seq: number; has_more: boolean }
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
  | { type: "share_ended"; stream_id: number };

// A screen share this browser can watch.
export interface ShareInfo {
  stream_id: number;
  sender: string;
  codec: string;
}

// Frames the browser sends: paging requests and screen-share playback control.
export type ClientRequest =
  | { type: "load_older"; before_seq: number; limit: number }
  | { type: "play_share"; stream_id: number }
  | { type: "stop_share"; stream_id: number };
