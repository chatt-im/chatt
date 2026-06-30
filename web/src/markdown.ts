// Renders chat message bodies as markdown.
//
// Bodies are authored by other room members, so the output is dropped into the
// DOM via innerHTML and MUST NOT be able to execute. `marked` does not sanitize:
// it emits raw HTML verbatim and only validates URL *encoding*, not the scheme.
// So this renderer escapes every raw-HTML token to literal text and restricts
// link/image targets to a safe scheme allowlist; everything else is markdown
// formatting, which is structurally safe.

import { Marked, type RendererObject } from "marked";

const HTML_ESCAPES: Record<string, string> = {
  "&": "&amp;",
  "<": "&lt;",
  ">": "&gt;",
  '"': "&quot;",
  "'": "&#39;",
};

function escapeHtml(text: string): string {
  return text.replace(/[&<>"']/g, (c) => HTML_ESCAPES[c]);
}

// `http(s)`, `mailto`, and scheme-less (relative, anchor, `/files/...`) targets
// are allowed; anything with another scheme (`javascript:`, `data:`, `vbscript:`)
// is rejected. Control bytes and spaces are stripped first so a `java\tscript:`
// style payload cannot smuggle a scheme past the test.
const SAFE_SCHEME = /^(?:https?|mailto):/i;
const HAS_SCHEME = /^[a-z][a-z0-9+.-]*:/i;

function isSafeUrl(href: string): boolean {
  const cleaned = (href ?? "").replace(/[\x00-\x20]/g, "");
  if (HAS_SCHEME.test(cleaned)) return SAFE_SCHEME.test(cleaned);
  return true;
}

// Returning `false` from a `use({ renderer })` override falls back to marked's
// default rendering, so safe links/images render exactly as upstream does.
const renderer: RendererObject = {
  html({ text }) {
    return escapeHtml(text);
  },
  link(token) {
    if (isSafeUrl(token.href)) return false;
    return this.parser.parseInline(token.tokens);
  },
  image(token) {
    if (isSafeUrl(token.href)) return false;
    return escapeHtml(token.text ?? "");
  },
};

// `breaks` maps a single newline to <br>, matching chat expectations where each
// line a sender typed is its own line.
const md = new Marked({ gfm: true, breaks: true });
md.use({ renderer });

export function renderMarkdown(body: string): string {
  return md.parse(body) as string;
}
