import { createEffect, createSignal } from "solid-js";
import type { AutoplayMode } from "./types";

export default function VideoPlayer(props: {
  class: string;
  responsiveSizing?: boolean;
  src: string;
  autoplay: AutoplayMode;
  // Locally derived display size, when the completed file fit the probe's work
  // budgets. Attributes reserve the ratio before the browser fetches metadata.
  width?: number | null;
  height?: number | null;
}) {
  let videoEl: HTMLVideoElement | undefined;
  const [metadataSource, setMetadataSource] = createSignal<string>();

  const hasProbedSize = () => props.width != null && props.height != null;
  const hasNaturalSize = () =>
    hasProbedSize() || metadataSource() === props.src;

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
      classList={{
        "media-video-sized": props.responsiveSizing && hasNaturalSize(),
        "media-video-unsized": props.responsiveSizing && !hasNaturalSize(),
      }}
      src={props.src}
      width={props.width ?? undefined}
      height={props.height ?? undefined}
      onLoadedMetadata={() => {
        if (props.responsiveSizing) setMetadataSource(props.src);
      }}
      controls
      preload="metadata"
      autoplay={props.autoplay !== "disabled"}
      muted={props.autoplay === "muted"}
    />
  );
}
