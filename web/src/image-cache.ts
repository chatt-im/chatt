type CachedImageState = {
  status: "loaded" | "error";
  width: number;
  height: number;
};

const IMAGE_CACHE_LIMIT = 256;
const imageCache = new Map<string, CachedImageState>();

function rememberImageState(src: string, state: CachedImageState) {
  if (imageCache.has(src)) imageCache.delete(src);
  imageCache.set(src, state);

  while (imageCache.size > IMAGE_CACHE_LIMIT) {
    const oldest = imageCache.keys().next().value;
    if (oldest === undefined) break;
    imageCache.delete(oldest);
  }
}

export function cachedImageState(src: string): CachedImageState | undefined {
  const state = imageCache.get(src);
  if (state) rememberImageState(src, state);
  return state;
}

export function markImageLoaded(src: string, image: HTMLImageElement) {
  rememberImageState(src, {
    status: "loaded",
    width: image.naturalWidth,
    height: image.naturalHeight,
  });
}

export function markImageError(src: string) {
  rememberImageState(src, { status: "error", width: 0, height: 0 });
}
