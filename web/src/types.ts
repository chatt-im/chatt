// Mirrors the Rust DTOs. Chat messages arrive as binary feed frames (decoded in
// `feed.ts`, encoded in `src/web_wire.rs`); share control stays JSON text.

export type MediaKind = "image" | "video" | "audio" | "file";
export type AutoplayMode = "disabled" | "muted" | "with-audio";
// Which side of a transfer this view is on: "incoming" is a download this
// client is receiving, "outgoing" is an upload it is sending.
export type TransferDirection = "incoming" | "outgoing";

export interface WebAttachment {
  // Served file name. The URL is `/files/${name}`.
  name: string;
  kind: MediaKind;
  // Intrinsic pixel size, present for images with a readable header. The view
  // reserves the box from these so a decoding image never grows the layout.
  width: number | null;
  height: number | null;
}

// One piece of a message body. Prose is safe HTML rendered by Rust from the
// canonical Markdown subset token stream; a code block renders from its
// precomputed highlight spans (see `highlight.ts`). A code fragment's `text` is
// UTF-8 bytes, because the spans are byte offsets into it. Quote boundaries are
// explicit so nested quote rules can join and split by Markdown scope.
export type Fragment =
  | { kind: "text"; html: string }
  | {
      kind: "code";
      lang: string;
      text: Uint8Array;
      spans: Uint8Array;
    }
  | { kind: "quote_start" }
  | { kind: "quote_end" };

export interface WebMessage {
  id: number;
  sender: string;
  timestamp_ms: number;
  attachment: WebAttachment | null;
  // The file transfer id for a file message, else null. A message with both
  // file_id and timestamp_ms matching one already held replaces it in place;
  // transfer ids alone are reused after server restarts.
  file_id: number | null;
  // The chat message id (distinct from `id`, which collapses to the transfer id
  // for file messages). With timestamp_ms it is the key `@@` references target.
  // Zero when unknown.
  message_id: number;
  // Precomputed `@@` reference code (without the prefix) for copying/quoting a
  // reference to this message. Empty when the message is not referenceable.
  ref_code: string;
  // Live progress for an in-flight file, set from `file_progress` envelopes.
  // `direction` picks the verb and the abort button label: an "incoming"
  // download offers [skip], an "outgoing" upload offers [cancel]. Cleared when
  // the enriched attachment replaces the placeholder.
  progress?: { transferred: number; total: number; direction: TransferDirection };
  // Persistent terminal state for a file that ended without landing, set from a
  // `file_terminal` envelope. Replaces `progress`; `verb` is skipped/cancelled/
  // failed and `reason` fills the `verb: reason` label (null for a bare verb).
  terminal?: { verb: string; reason: string | null };
  // Client-only playback intent attached to a newly received video. History
  // sync messages omit it so reconnecting does not autoplay old media.
  autoplay?: AutoplayMode;
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
  | { type: "share_error"; stream_id: number; message: string }
  // Live receive progress for an in-flight file, merged into the placeholder
  // message matched by `file_id` and `timestamp_ms`.
  | {
      type: "file_progress";
      file_id: number;
      timestamp_ms: number;
      transferred: number;
      total: number;
      direction: TransferDirection;
    }
  // A file transfer ended without landing; merged into the placeholder matched by
  // `file_id` and `timestamp_ms`, swapping its progress bar for a terminal label.
  | {
      type: "file_terminal";
      file_id: number;
      timestamp_ms: number;
      verb: string;
      reason: string | null;
    }
  // Sent once on connect with browser-only behavior settings.
  | {
      type: "config";
      readonly: boolean;
      autoplay: AutoplayMode;
      viewer_in_seperate_browser_tab: boolean;
    };

// A screen share this browser can watch.
export interface ShareInfo {
  stream_id: number;
  sender: string;
  codec: string;
  width: number;
  height: number;
}

// Frames the browser sends: paging requests, screen-share playback control, and
// (when not read-only) composing chat messages and file uploads. File bytes
// travel as separate binary frames between `upload_start` and `upload_finish`,
// each prefixed with the little-endian upload id.
export type ClientRequest =
  | { type: "load_older"; before_seq: number; limit: number }
  | { type: "play_share"; stream_id: number }
  | { type: "stop_share"; stream_id: number }
  | { type: "send_message"; body: string }
  | { type: "upload_start"; upload_id: number; name: string; size: number }
  | { type: "upload_finish"; upload_id: number }
  | { type: "abort_transfer"; transfer_id: number };
