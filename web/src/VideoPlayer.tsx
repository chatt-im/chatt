import { createEffect } from "solid-js";
import type { AutoplayMode } from "./types";

export default function VideoPlayer(props: {
  class: string;
  src: string;
  autoplay: AutoplayMode;
}) {
  let videoEl: HTMLVideoElement | undefined;

  createEffect(() => {
    const mode = props.autoplay;
    if (!videoEl || mode === "disabled") return;
    videoEl.muted = mode === "muted";
    void videoEl.play().catch(() => {
      // Browsers commonly reject unmuted autoplay until the user interacts
      // with the page. Controls remain available when that happens.
    });
  });

  return (
    <video
      ref={videoEl}
      class={props.class}
      src={props.src}
      controls
      preload="metadata"
      autoplay={props.autoplay !== "disabled"}
      muted={props.autoplay === "muted"}
    />
  );
}
