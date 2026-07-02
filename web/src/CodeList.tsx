import { onCleanup, onMount } from "solid-js";
import type { FileHighlight } from "./highlight";

// Rows kept beyond the viewport on each side when the window is refilled.
// The slack is what lets most scroll frames skip DOM work entirely.
const OVERSCAN = 20;

// Refill once the viewport gets this close (in rows) to the window's edge.
// Until then a scroll frame writes nothing: the browser composites the native
// scroll and the main thread stays idle. This batches row recycling into one
// style/layout/paint pass per ~(OVERSCAN - margin) rows scrolled instead of
// one per frame.
const REFILL_MARGIN = 2;

function clamp(value: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, value));
}

// One highlight run inside a pooled row: a span that is created once and then
// only mutated (class, text-node value, display), never rebuilt.
interface RunSlot {
  el: HTMLSpanElement;
  text: Text;
  visible: boolean;
}

// A pooled row. `noText` is the line number's text node; `runs` is the slot
// pool for the line's highlight runs, grown on demand and hidden when a
// shorter line recycles the row.
interface Row {
  el: HTMLDivElement;
  noText: Text;
  code: HTMLElement;
  runs: RunSlot[];
  line: number;
}

// A windowed list specialized for source lines, which all share one fixed row
// height (`white-space: pre`, constant line-height, no wrapping). Scrolling is
// native — the spacer gives the container the file's full height and the
// browser owns the scrollbar — but the rows bypass the framework entirely: in
// steady state a scroll frame creates no DOM nodes at all. Rows are recycled
// by writing text-node values, class names, and transforms in place, each
// write guarded by a comparison so an unchanged value costs no invalidation.
//
// The pool is kept in DOM order equal to line order: advancing the window by
// k lines rewrites the k leaving rows and moves them to the other end of the
// child list. Native selection highlights a DOM range, so keeping document
// order equal to visual order makes selecting across lines behave like a
// static page; a moved row drops out of any selection range exactly as an
// unmounted row did under the previous virtualizer, while selections within
// stationary rows survive scrolling. Row positions are absolute `translateY`
// transforms, computed as a single multiplication so they never accumulate
// float error.
//
// The spacer height `lineCount * rowHeight` meets the browser's max element
// height (~17.9M px in Firefox) only far beyond the server's highlight size
// cap.
export default function CodeList(props: { highlight: FileHighlight }) {
  // A new file remounts this component (FileViewer re-creates it when its
  // resource changes), so the highlight can be captured once.
  const highlight = props.highlight;
  const lineCount = highlight.lineCount;

  let scrollEl!: HTMLDivElement;
  let spacerEl!: HTMLDivElement;

  onMount(() => {
    let rowHeight = 0;
    // Cached from the ResizeObserver so scroll frames never read layout
    // geometry after writing to the DOM (a forced synchronous layout).
    let viewportH = 0;
    const rows: Row[] = [];

    // The container's scrollWidth follows the longest *mounted* line, so
    // without intervention the horizontal scrollbar appears and disappears as
    // long lines scroll through the window — each toggle costs a layout and a
    // viewport resize. Ratchet the spacer's min-width up to the widest
    // content seen so the scrollable width only ever grows. Callers read
    // scrollWidth at frame start, before any writes, so the read is free.
    let ratchetedWidth = 0;
    const ratchetWidth = () => {
      const width = scrollEl.scrollWidth;
      // Only genuine overflow ratchets: without the clientWidth guard a wide
      // viewport would pin min-width to its own width and leave phantom
      // horizontal scroll behind when the panel is later narrowed.
      if (width > ratchetedWidth && width > scrollEl.clientWidth) {
        ratchetedWidth = width;
        spacerEl.style.minWidth = `${width}px`;
      }
    };

    const makeRow = (): Row => {
      const el = document.createElement("div");
      el.className = "code-line";
      const no = document.createElement("span");
      no.className = "code-line-no";
      const noText = document.createTextNode("");
      no.appendChild(noText);
      const code = document.createElement("code");
      code.className = "code-line-text";
      el.append(no, code);
      spacerEl.appendChild(el);
      return { el, noText, code, runs: [], line: -1 };
    };

    const assign = (row: Row, line: number) => {
      row.line = line;
      row.noText.data = String(line + 1);
      let used = 0;
      highlight.forEachLineRun(line, (text, cls) => {
        let slot = row.runs[used];
        if (slot === undefined) {
          const el = document.createElement("span");
          const textNode = document.createTextNode("");
          el.appendChild(textNode);
          row.code.appendChild(el);
          slot = { el, text: textNode, visible: true };
          row.runs.push(slot);
        }
        const className = cls === 0 ? "" : `hl-${cls}`;
        if (slot.el.className !== className) slot.el.className = className;
        if (slot.text.data !== text) slot.text.data = text;
        if (!slot.visible) {
          slot.el.style.display = "";
          slot.visible = true;
        }
        used++;
      });
      for (let i = used; i < row.runs.length; i++) {
        const slot = row.runs[i]!;
        if (slot.visible) {
          slot.el.style.display = "none";
          slot.visible = false;
        }
      }
      row.el.style.transform = `translateY(${line * rowHeight}px)`;
    };

    // The line shown by rows[0]; rows[i] always shows firstLine + i. -1 marks
    // the pool contents invalid (initial mount, pool resize).
    let firstLine = -1;

    // A full-window rewrite reuses text nodes in place, which leaves a native
    // selection attached to content it was never made on. Mimic the previous
    // virtualizer's remount semantics by dropping a selection that touches
    // the viewer; selections elsewhere on the page are left alone.
    const clearIntersectingSelection = () => {
      const selection = document.getSelection();
      if (
        selection &&
        selection.rangeCount > 0 &&
        !selection.isCollapsed &&
        selection.containsNode(spacerEl, true)
      ) {
        selection.removeAllRanges();
      }
    };

    // Recomputes the window from the live scroll position. A small advance
    // rewrites only the rows that left the window and moves them to the other
    // end of the child list, keeping DOM order equal to line order.
    const update = () => {
      if (rowHeight <= 0 || lineCount === 0) return;
      let n = Math.min(
        lineCount,
        Math.ceil(viewportH / rowHeight) + 2 * OVERSCAN,
      );
      // Keep a slightly oversized pool rather than shrinking: a scrollbar
      // appearing or a small viewport change would otherwise resize the pool
      // and force a full-window reassign.
      if (n < rows.length && rows.length - n <= 4) {
        n = rows.length;
      }
      if (n !== rows.length) {
        while (rows.length < n) rows.push(makeRow());
        while (rows.length > n) rows.pop()!.el.remove();
        firstLine = -1;
      }
      const visFirst = Math.floor(scrollEl.scrollTop / rowHeight);
      const visLast = Math.min(
        lineCount - 1,
        Math.floor((scrollEl.scrollTop + viewportH) / rowHeight),
      );
      if (firstLine >= 0) {
        // Skip the frame while the viewport sits comfortably inside the
        // window. An edge of the file counts as covered: the window cannot
        // extend past it.
        const coveredAbove =
          firstLine === 0 || firstLine <= visFirst - REFILL_MARGIN;
        const coveredBelow =
          firstLine + n >= lineCount ||
          firstLine + n - 1 >= visLast + REFILL_MARGIN;
        if (coveredAbove && coveredBelow) return;
      }
      const start = clamp(visFirst - OVERSCAN, 0, Math.max(0, lineCount - n));
      if (start === firstLine) return;
      // Moving a row re-inserts its whole span subtree, which costs a style
      // recalc of every node, but writes only the rows that actually changed;
      // measured, that wins until the delta approaches the pool size, where
      // rewriting every row in place avoids the re-inserts. DOM order stays
      // ascending on both paths: in-place because rows[i] takes start + i.
      if (firstLine < 0 || Math.abs(start - firstLine) > n >> 1) {
        // A selection would be left attached to rewritten text; drop it like
        // the previous virtualizer's remount did.
        clearIntersectingSelection();
        for (let i = 0; i < n; i++) {
          assign(rows[i]!, start + i);
        }
      } else if (start > firstLine) {
        // Scrolled down: the top rows take the lines past the old window and
        // move to the back.
        const moved = rows.splice(0, start - firstLine);
        for (let i = 0; i < moved.length; i++) {
          const row = moved[i]!;
          assign(row, firstLine + n + i);
          spacerEl.appendChild(row.el);
        }
        rows.push(...moved);
      } else {
        // Scrolled up: the bottom rows take the lines above the old window
        // and move to the front.
        const moved = rows.splice(rows.length - (firstLine - start));
        const ref = rows[0]!.el;
        for (let i = 0; i < moved.length; i++) {
          const row = moved[i]!;
          assign(row, start + i);
          spacerEl.insertBefore(row.el, ref);
        }
        rows.unshift(...moved);
      }
      firstLine = start;
    };

    const applyRowHeight = (height: number) => {
      rowHeight = height;
      spacerEl.style.height = `${lineCount * rowHeight}px`;
      // Content stays valid; only positions depend on the row height.
      for (const row of rows) {
        if (row.line >= 0) {
          row.el.style.transform = `translateY(${row.line * rowHeight}px)`;
        }
      }
      update();
    };

    const measureRow = () => {
      const row = rows[0];
      if (!row) return;
      const height = row.el.getBoundingClientRect().height;
      if (height > 0 && Math.abs(height - rowHeight) > 0.01) {
        applyRowHeight(height);
      }
    };

    let raf = 0;
    const onScroll = () => {
      if (raf) return;
      raf = requestAnimationFrame(() => {
        raf = 0;
        // Reads first (layout is clean at frame start), then writes.
        ratchetWidth();
        update();
      });
    };
    scrollEl.addEventListener("scroll", onScroll, { passive: true });
    onCleanup(() => {
      scrollEl.removeEventListener("scroll", onScroll);
      if (raf) cancelAnimationFrame(raf);
    });

    // Seed the row height from the resolved line-height so the first window
    // (rendered before paint) is already essentially exact, then confirm
    // against a real laid-out row.
    viewportH = scrollEl.clientHeight;
    applyRowHeight(parseFloat(getComputedStyle(scrollEl).lineHeight) || 0);
    measureRow();

    // One observer for the viewport; rows never need measuring again. The
    // viewport height comes from the observer entry, so steady scrolling
    // never queries layout geometry. The browser clamps scrollTop itself
    // when the spacer shrinks (zoom, panel resize), and `update` reads the
    // live value.
    const observer = new ResizeObserver((entries) => {
      const rect = entries[entries.length - 1]?.contentRect;
      // A horizontal split drag changes only the viewport width. Source rows
      // never wrap, so none of the list geometry depends on that dimension;
      // avoid forcing overflow and row geometry reads on every drag frame.
      if (!rect || Math.abs(rect.height - viewportH) < 0.01) return;
      viewportH = rect.height;
      measureRow();
      ratchetWidth();
      update();
    });
    observer.observe(scrollEl);
    onCleanup(() => observer.disconnect());

    // The mono stack is local fonts, but if a fallback loads late its metrics
    // change the row height; re-measure once fonts settle.
    let disposed = false;
    onCleanup(() => {
      disposed = true;
    });
    document.fonts?.ready.then(() => {
      if (!disposed) measureRow();
    });
  });

  return (
    <div class="file-viewer-body" ref={scrollEl}>
      <div class="code-list" ref={spacerEl} />
    </div>
  );
}
