// Decoders for binary highlight buffers produced by the Rust backend.
//
// The backend runs the syntax highlighter and sends compact span buffers; the
// browser never highlights. A span buffer is a run list of (u32 byte length, u8
// class); the class byte indexes `chatt-message-format`'s `HlClass` enum and
// maps to a `hl-<n>` CSS color. Class 0 is plain text and gets no span.
//
// Runs and line splits fall on UTF-8 character boundaries, so slicing the
// source bytes per run and decoding each slice is always valid.

const decoder = new TextDecoder();

const HTML_ESCAPES: Record<string, string> = {
  "&": "&amp;",
  "<": "&lt;",
  ">": "&gt;",
};

function escapeHtml(text: string): string {
  return text.replace(/[&<>]/g, (c) => HTML_ESCAPES[c]!);
}

// Decodes an inline span buffer (a code fragment's `[version][(len,class)...]`)
// applied to `textBytes`, returning the highlighted inner HTML for a `<code>`.
export function renderInline(textBytes: Uint8Array, spans: Uint8Array): string {
  const view = new DataView(spans.buffer, spans.byteOffset, spans.byteLength);
  let pos = 1; // Skip the version byte.
  let offset = 0;
  let html = "";
  while (pos + 5 <= spans.byteLength) {
    const len = view.getUint32(pos, true);
    const cls = view.getUint8(pos + 4);
    pos += 5;
    const text = escapeHtml(decoder.decode(textBytes.subarray(offset, offset + len)));
    offset += len;
    html += cls === 0 ? text : `<span class="hl-${cls}">${text}</span>`;
  }
  return html;
}

// A whole file decoded for the line viewer. Lines render lazily so a file with
// tens of thousands of lines only builds DOM for the visible window.
export interface FileHighlight {
  readonly lineCount: number;
  readonly text: string;
  readonly lines: readonly string[];
  // Walks the highlight runs of line `index` (0-based) in order. `cls` is the
  // `HlClass` byte (0 = plain). The viewer feeds each run into a recycled
  // span slot, so no HTML string, escaping, parsing, or node creation happens
  // on the hot recycle path.
  forEachLineRun(index: number, fn: (text: string, cls: number) => void): void;
}

// Decodes a file highlight buffer from `/highlight/<name>`. Layout (see
// `highlight::encode_file`): version, u32 line count, u32 text length, the
// UTF-8 text, a u32 per-line record offset table, then the records.
export function decodeFileBuffer(buffer: ArrayBuffer): FileHighlight {
  const bytes = new Uint8Array(buffer);
  const view = new DataView(buffer);
  // Validate the header before trusting any length, so a wrong response (a dev
  // proxy serving index.html, a truncated body) fails cleanly instead of
  // allocating a line array from a garbage count.
  if (buffer.byteLength < 9 || view.getUint8(0) !== 1) {
    throw new Error("not a highlight buffer");
  }
  let pos = 1; // Skip version.
  const lineCount = view.getUint32(pos, true);
  pos += 4;
  const textLen = view.getUint32(pos, true);
  pos += 4;
  const textStart = pos;
  pos += textLen;
  const offsetsStart = pos;
  const recordsStart = offsetsStart + lineCount * 4;
  if (recordsStart > buffer.byteLength) {
    throw new Error("corrupt highlight buffer");
  }
  const text = decoder.decode(bytes.subarray(textStart, textStart + textLen));
  const lines = text.split("\n");
  if (lines.length > lineCount) lines.length = lineCount;
  while (lines.length < lineCount) lines.push("");

  // Line start offsets, derived from newlines with the same rule as the
  // encoder: each line's runs carry their own lengths, so only the start of the
  // line's bytes is needed to slice each run.
  const lineStart = new Uint32Array(lineCount);
  {
    let line = 0;
    let start = textStart;
    const textEnd = textStart + textLen;
    for (let i = textStart; i < textEnd && line < lineCount; i++) {
      if (bytes[i] === 0x0a) {
        lineStart[line] = start;
        line++;
        start = i + 1;
      }
    }
    if (line < lineCount) {
      lineStart[line] = start;
    }
  }

  return {
    lineCount,
    text,
    lines,
    forEachLineRun(index: number, fn: (text: string, cls: number) => void): void {
      if (index < 0 || index >= lineCount) return;
      const recordOffset = view.getUint32(offsetsStart + index * 4, true);
      let pos = recordsStart + recordOffset;
      const runCount = view.getUint32(pos, true);
      pos += 4;
      let offset = lineStart[index]!;
      for (let i = 0; i < runCount; i++) {
        const len = view.getUint32(pos, true);
        const cls = view.getUint8(pos + 4);
        pos += 5;
        fn(decoder.decode(bytes.subarray(offset, offset + len)), cls);
        offset += len;
      }
    },
  };
}
