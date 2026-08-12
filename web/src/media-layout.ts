export interface MediaEstimateLayout {
  contentWidth: number;
  mediaMaxHeight: number;
}

const MEDIA_BORDER_PX = 12;

// Mirrors the shipped border-box image/video rules. Intrinsic content is never
// enlarged; a width cap scales the content inside its borders, and max-height
// caps the resulting outer box.
export function estimateMediaBoxHeight(
  width: number | null | undefined,
  height: number | null | undefined,
  layout: MediaEstimateLayout
): number | null {
  if ((width ?? 0) <= 0 || (height ?? 0) <= 0) return null;
  const usableWidth = Math.max(0, layout.contentWidth - MEDIA_BORDER_PX);
  const scale = Math.min(1, usableWidth / width!);
  const outerHeight = height! * scale + MEDIA_BORDER_PX;
  return Math.ceil(Math.min(outerHeight, layout.mediaMaxHeight));
}
