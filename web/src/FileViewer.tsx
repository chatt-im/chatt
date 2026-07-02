import { createResource, Show, Suspense } from "solid-js";
import CodeList from "./CodeList";
import { decodeFileBuffer, type FileHighlight } from "./highlight";

// Fetches and decodes a file's highlight buffer from `/highlight/<name>`.
// A 413 means the file is too large to preview, 415 means it is not UTF-8 text,
// and any other non-200 is an error.
async function loadFile(
  name: string,
): Promise<{ highlight: FileHighlight } | { error: string }> {
  try {
    const response = await fetch(`/highlight/${encodeURIComponent(name)}`);
    if (response.status === 413) return { error: "file too large to preview" };
    if (response.status === 415) return { error: "not a text file" };
    if (!response.ok) return { error: `failed to load (${response.status})` };
    // The endpoint always returns the binary buffer; any other type means the
    // response is not ours (e.g. a dev-server SPA fallback).
    const contentType = response.headers.get("Content-Type") ?? "";
    if (!contentType.includes("octet-stream")) {
      return { error: "unexpected response" };
    }
    return { highlight: decodeFileBuffer(await response.arrayBuffer()) };
  } catch (error) {
    return { error: error instanceof Error ? error.message : "failed to load" };
  }
}

// The expanded, line-numbered, syntax-highlighted view of a text file. Only the
// visible lines build HTML, so a file with tens of thousands of lines stays
// responsive. Highlighting is done by Rust; this view just paints spans.
export default function FileViewer(props: { name: string; onClose: () => void }) {
  const [state] = createResource(() => props.name, loadFile);

  return (
    <div class="file-viewer">
      <div class="file-viewer-head">
        <span class="file-viewer-name">{props.name}</span>
        <a class="file-viewer-download" href={`/files/${encodeURIComponent(props.name)}`} download={props.name}>
          download
        </a>
        <button class="file-viewer-close" type="button" onClick={props.onClose}>
          close
        </button>
      </div>
      <Suspense fallback={<div class="file-viewer-status">loading…</div>}>
        {(() => {
          const result = state();
          if (result && "error" in result) {
            return <div class="file-viewer-status">{result.error}</div>;
          }
          const highlight = result?.highlight;
          return (
            <Show when={highlight}>
              <CodeList highlight={highlight!} />
            </Show>
          );
        })()}
      </Suspense>
    </div>
  );
}
