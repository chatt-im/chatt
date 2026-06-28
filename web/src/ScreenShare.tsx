import { For, Show } from "solid-js";
import type { ShareInfo } from "./types";

// Presents available screen shares with play/stop controls and a per-share
// canvas the decoder draws to. Decode and frame feeding live in App; this is
// presentational. Each share has its own canvas so several can play at once.
export default function ScreenShare(props: {
  shares: ShareInfo[];
  playing: number[];
  onPlay: (streamId: number) => void;
  onStop: (streamId: number) => void;
  canvasRef: (streamId: number, el: HTMLCanvasElement) => void;
}) {
  const isPlaying = (streamId: number) => props.playing.includes(streamId);
  return (
    <Show when={props.shares.length > 0}>
      <div class="screenshare">
        <For each={props.shares}>
          {(share) => (
            <div class="screenshare-item">
              <div class="screenshare-row">
                <span class="screenshare-sender">{share.sender} is sharing a screen</span>
                <Show
                  when={isPlaying(share.stream_id)}
                  fallback={
                    <button class="screenshare-button" onClick={() => props.onPlay(share.stream_id)}>
                      play
                    </button>
                  }
                >
                  <button class="screenshare-button" onClick={() => props.onStop(share.stream_id)}>
                    stop
                  </button>
                </Show>
              </div>
              <canvas
                class="screenshare-canvas"
                classList={{ "is-playing": isPlaying(share.stream_id) }}
                ref={(el) => props.canvasRef(share.stream_id, el)}
              />
            </div>
          )}
        </For>
      </div>
    </Show>
  );
}
