import { createEffect, createResource, Show, Suspense } from "solid-js";
import CodeList from "./CodeList";
import { decodeFileBuffer, type FileHighlight } from "./highlight";

type FileLoadResult = { highlight: FileHighlight } | { error: string };

// Fetches and decodes a file's highlight buffer from `/highlight/<name>`.
// A 413 means the file is too large to preview, 415 means it is not UTF-8 text,
// and any other non-200 is an error.
async function loadFile(name: string): Promise<FileLoadResult> {
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

// The expanded, line-numbered, syntax-highlighted view of a text file. Panel
// chrome is owned by PreviewPanel so code and image previews share one history.
// Only visible lines build HTML, keeping very large files responsive.
export default function FileViewer(props: {
  name: string;
  searchOpen: boolean;
  onCloseSearch: () => void;
  onTextLoaded: (text: string | null) => void;
}) {
  const [state] = createResource(() => props.name, loadFile);
  createEffect(() => {
    const result = state();
    props.onTextLoaded(
      result && "highlight" in result ? result.highlight.text : null,
    );
  });
  const error = () => {
    const result = state();
    return result && "error" in result ? result.error : undefined;
  };
  const highlight = () => {
    const result = state();
    return result && "highlight" in result ? result.highlight : undefined;
  };

  return (
    <div class="file-viewer">
      <Suspense fallback={<div class="file-viewer-status">loading…</div>}>
        <Show
          when={error()}
          fallback={
            <Show when={highlight()} keyed>
              {(loadedHighlight) => (
                <CodeList
                  highlight={loadedHighlight}
                  searchOpen={props.searchOpen}
                  onCloseSearch={props.onCloseSearch}
                />
              )}
            </Show>
          }
        >
          {(message) => <div class="file-viewer-status">{message()}</div>}
        </Show>
      </Suspense>
    </div>
  );
}
