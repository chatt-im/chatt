// Mirrors the Rust DTOs in `src/web_server.rs`. Keep the two in sync.

export type MediaKind = "image" | "video" | "audio" | "file";

export interface WebAttachment {
  // Served file name. The URL is `/files/${name}`.
  name: string;
  kind: MediaKind;
}

export interface WebMessage {
  id: number;
  sender: string;
  body: string;
  timestamp_ms: number;
  attachment: WebAttachment | null;
}

// One JSON object per WebSocket text frame.
export type ServerEnvelope =
  | { type: "sync"; messages: WebMessage[] }
  | { type: "message"; message: WebMessage };
