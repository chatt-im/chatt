import { For, Show } from "solid-js";
import type { ShareInfo } from "./types";

// Presents available screen shares with play/stop controls and the canvas the
// decoder draws to. Decode and frame feeding live in App; this is presentational.
export default function ScreenShare(props: {
  shares: ShareInfo[];
  playing: number | null;
  onPlay: (streamId: number) => void;
  onStop: (streamId: number) => void;
  canvasRef: (el: HTMLCanvasElement) => void;
}) {
  return (
    <Show when={props.shares.length > 0 || props.playing !== null}>
      <div class="screenshare">
        <For each={props.shares}>
          {(share) => (
            <div class="screenshare-row">
              <span class="screenshare-sender">{share.sender} is sharing a screen</span>
              <Show
                when={props.playing === share.stream_id}
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
          )}
        </For>
        <canvas
          class="screenshare-canvas"
          classList={{ "is-playing": props.playing !== null }}
          ref={props.canvasRef}
        />
      </div>
    </Show>
  );
}
