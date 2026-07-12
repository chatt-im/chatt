# extui TUI Rendering Guide

Reference for building terminal interfaces in extask using the extui library.

## Core Concept: Rect Splitting

All layout is done by carving a `Rect` into smaller regions. `Rect` has four fields:
`x: u16, y: u16, w: u16, h: u16`.

### take\_\* (mutating)

`take_top`, `take_bottom`, `take_left`, `take_right` **mutate** `self` (shrinking it) and
**return** the taken portion:

```rust
let mut area = Rect { x: 0, y: 0, w: 80, h: 24 };
let status_bar = area.take_bottom(1);   // area is now 80x23, status_bar is 80x1
let sidebar    = area.take_right(0.4);  // area shrinks, sidebar is 40% of former width
let header     = area.take_top(2);      // area shrinks by 2 rows from top
// area is now whatever remains
```

### Split rules

The argument to any split/take method is an `impl SplitRule`:

| Type  | Meaning                    | Example                |
| ----- | -------------------------- | ---------------------- |
| `i32` | Absolute cells (positive)  | `area.take_top(1)`     |
| `f64` | Ratio of current dimension | `area.take_right(0.4)` |

### v_split / h_split (non-mutating)

Return a tuple without mutating the original:

```rust
let (top, bottom) = area.v_split(1);    // split vertically at row 1
let (left, right) = area.h_split(0.5);  // split horizontally at 50%
```

### Helpers

```rust
area.is_empty()  // true if w == 0 || h == 0
area.left()      // x
area.right()     // x + w
area.top()       // y
area.bottom()    // y + h
```

## Core Concept: DisplayRect

A `DisplayRect` is a `Rect` paired with `RenderProperties` (style, alignment, offsets). It
tracks a **cursor** that advances as you draw, enabling fluent left-to-right (or right-to-left)
rendering without manual coordinate math.

### Creating a DisplayRect

From any `Rect`:

```rust
// With default properties (left-aligned, no style):
let dr = area.display();

// With an initial property:
let dr = area.with(AnsiColor::Red1.as_fg());
```

Both consume the `Rect` by value and return a `DisplayRect`.

### Drawing Methods

All drawing methods consume `self` by value and return a new `DisplayRect` with the cursor
advanced. This is the key insight: **every call returns an updated cursor position**.

#### `.text(buf, &str) -> DisplayRect`

Draws a string at the current cursor position, advances the cursor past the drawn text:

```rust
area.display()
    .text(buf, "Hello")     // cursor at 0, draws "Hello", cursor now at 5
    .text(buf, " World");   // cursor at 5, draws " World", cursor now at 11
```

#### `.fmt(buf, format_args!(...)) -> DisplayRect`

Same as `.text` but accepts `format_args!` for formatted output:

```rust
area.display()
    .fmt(buf, format_args!(" {:02}:{:02} ", hours, minutes));
```

#### `.skip(amount: u16) -> DisplayRect`

Advances the cursor without drawing anything:

```rust
area.display()
    .text(buf, "Name")
    .skip(2)                 // 2-cell gap
    .text(buf, "Value");     // renders with gap between
```

#### `.fill(buf) -> DisplayRect`

Fills the entire rect with the current background style. Does not move the cursor.
Typically called first to set a background:

```rust
area.with(AnsiColor::Grey[3].as_bg())
    .fill(buf)               // entire rect gets background
    .text(buf, "over it");   // text drawn on top
```

### `.with(property) -> DisplayRect`

Applies a `RenderProperty` **without drawing**. Returns a new `DisplayRect` with the
property set but the cursor unchanged. This is how you change style or alignment mid-chain:

```rust
area.display()
    .with(AnsiColor::Blue1.as_fg())
    .text(buf, "blue text")
    .with(AnsiColor::Red1.as_fg())      // switch style, cursor stays
    .text(buf, " red text");
```

`.with()` accepts anything implementing `RenderProperty`:

| Type               | Effect                                    |
| ------------------ | ----------------------------------------- |
| `Style`            | Sets the full style (fg + bg + modifiers) |
| `HAlign`           | Changes horizontal alignment              |
| `VAlign`           | Changes vertical alignment                |
| `Ellipsis(bool)`   | Enables/disables truncation with `...`    |
| `RenderProperties` | Replaces all properties at once           |

### Horizontal Alignment

Alignment controls how `.text` / `.fmt` position content and which direction the cursor
moves.

**`HAlign::Left`** (default): Text renders left-to-right. The `offset` field tracks how
far the cursor has advanced from the left edge. Each `.text` call appends after the
previous.

**`HAlign::Right`**: Text renders right-to-left from the right edge. The `right_offset`
field tracks how far the cursor has consumed from the right. Each `.text` call prepends
before the previous. `.skip` also moves inward from the right.

**`HAlign::Center`**: Text is centered in the available space. After rendering, the cursor
is set to the full width (consuming everything), so subsequent draws typically need a new
alignment.

Both offsets are respected simultaneously -- left content won't overwrite right content:

```rust
// Status bar pattern: left content, then right content
let mut r = area.with(mode_style)
    .fmt(buf, format_args!(" {} ", label));      // draws from left

r = r.with(section_style)
    .fmt(buf, format_args!(" {group} "));        // continues left

r = r.with(HAlign::Right);                       // switch direction

r = r.with(mode_style)
    .fmt(buf, format_args!(" {pos} "));          // draws from right edge

r = r.with(section_style)
    .fmt(buf, format_args!(" {sort} "));         // draws left of previous right
```

### Ellipsis Truncation

When enabled, text that overflows the available width is truncated with a trailing `...`:

```rust
area.with(Ellipsis(true))
    .text(buf, very_long_string);  // "Some long te..."
```

Without ellipsis, text is silently truncated at the boundary.

## Style and AnsiColor

### Style

`Style` is a packed `u32` combining foreground AnsiColor, background AnsiColor, and modifiers.

```rust
Style::DEFAULT                              // no AnsiColors, no modifiers
AnsiColor::Red1.as_fg()                         // red foreground only
AnsiColor::Blue1.as_bg()                        // blue background only
AnsiColor::Red1.with_bg(AnsiColor::Black)           // red fg + black bg
AnsiColor::Black.with_fg(AnsiColor::Red1)           // same: black bg + red fg (note: method is on bg AnsiColor)
Style::DEFAULT.with_fg(AnsiColor::Red1)         // explicit builder
    .with_bg(AnsiColor::Black)
    .with_modifier(Modifier::BOLD)
```

Key methods on `Style` (all `const`, all non-mutating -- return new `Style`):

- `.with_fg(AnsiColor) -> Style`
- `.with_bg(AnsiColor) -> Style`
- `.with_modifier(Modifier) -> Style`
- `.without_fg() -> Style`
- `.without_bg() -> Style`

### AnsiColor

`AnsiColor(u8)` wraps an ANSI 256-AnsiColor index. **No 24-bit true AnsiColor** -- only the 256-AnsiColor
palette.

Four ways to get a `AnsiColor`:

```rust
AnsiColor::SpringGreen    // named constant (see full list below)
AnsiColor::Grey[15]       // grey ramp array (0..=30)
AnsiColor(42)             // raw 256-AnsiColor index
AnsiColor::Black          // alias for AnsiColor(16)
```

Convenience methods on `AnsiColor` for building styles:

- `.as_fg() -> Style` -- style with just this foreground
- `.as_bg() -> Style` -- style with just this background
- `.with_fg(AnsiColor) -> Style` -- `self` as bg, argument as fg
- `.with_bg(AnsiColor) -> Style` -- `self` as fg, argument as bg

### The Grey Ramp

`AnsiColor::Grey` is a `[AnsiColor; 31]` array providing a perceptual brightness ramp from black
to white. This is the primary tool for building UI contrast hierarchies.

```
Index:  0    1    2    3    4    5    6    7    8    9
ANSI:  16  232  233  234  235  236  237  238  239  240
       ^black                                      ^dark grey

Index: 10   11   12   13   14   15   16   17   18   19
ANSI:  59  241  242  243  244  102  245  246  247  139
                                     ^mid grey

Index: 20   21   22   23   24   25   26   27   28   29   30
ANSI: 248  145  249  250  251  252  188  253  254  255  231
                          ^light grey                    ^white
```

Usage in extask by role:

| Role             | Typical Grey indices | Purpose                        |
| ---------------- | -------------------- | ------------------------------ |
| Backgrounds      | 0-6                  | Near-black for dark theme base |
| Dim/muted text   | 6-12                 | Secondary info, borders        |
| Mid-emphasis     | 15-20                | Tags, metadata, descriptions   |
| Primary text     | 22-26                | Task names, key content        |
| Bright/highlight | 28-30                | Titles, emphasized text        |

### Named AnsiColor Constants

All AnsiColors extask currently uses, grouped by purpose:

**Status badge AnsiColors** (dark variant for badge bg, light variant for cursor bg):

| Status      | Dark (badge bg)                  | Light (cursor bg)               |
| ----------- | -------------------------------- | ------------------------------- |
| In Progress | `AnsiColor(221)` (gold)          | `AnsiColor(229)` (wheat)        |
| In Review   | `AnsiColor::DarkOliveGreen`(113) | `AnsiColor::LightSeaGreen`(193) |
| Blocked     | `AnsiColor::NeonRed` (203)       | `AnsiColor::MistyRose`(224)     |
| Done        | `AnsiColor::SpringGreen` (42)    | `AnsiColor::Honeydew`(194)      |
| On Hold     | `AnsiColor::Violet` (140)        | `AnsiColor::Thistle`(225)       |
| Missed      | `AnsiColor::Salmon` (209)        | `AnsiColor(223)`                |
| Default     | `Grey[17]`                       | `Grey[22]`                      |

**Mode accent AnsiColors** (used in status bar mode label):

| Mode                | AnsiColor                         |
| ------------------- | --------------------------------- |
| Normal              | `AnsiColor::LightSkyBlue1` (153)  |
| Visual              | `AnsiColor::Violet` (140)         |
| Input               | `AnsiColor::SpringGreen` (42)     |
| Search              | `AnsiColor::PaleTurquoise1` (159) |
| Select              | `AnsiColor::DeepSkyBlue1` (39)    |
| Tag Select          | `AnsiColor::Gold3` (142)          |
| Blocker Search      | `AnsiColor::NeonRed` (203)        |
| Spelling Correction | `AnsiColor::Orange1` (214)        |

**Status bar constants** (defined in `theme.rs`):

```rust
pub const STATUS_SECTION_BG: AnsiColor = AnsiColor::Grey[6];
pub const STATUS_FILL_BG: AnsiColor = AnsiColor::Grey[3];
pub const STATUS_SECTION_FG: AnsiColor = AnsiColor::Grey[22];
pub const STATUS_MODE_FG: AnsiColor = AnsiColor::Black;
```

**Time tracking AnsiColors:**

```rust
AnsiColor(221).with_fg(AnsiColor::Black)           // active timer (gold bg)
AnsiColor::Grey[9].with_fg(AnsiColor(217))         // over estimate (pink on dark)
AnsiColor::SpringGreen.with_fg(AnsiColor::Grey[4]) // done task time
AnsiColor::Grey[9].with_fg(AnsiColor::Grey[25])    // normal time display
```

### Building Contrast Hierarchies

extask uses a consistent pattern: define 3 style tiers (primary, mid, dim) and swap them
based on cursor state. This gives every element appropriate emphasis in all contexts:

```rust
// Default styles
let mut name_sty  = AnsiColor::Grey[25].as_fg();  // bright -- primary content
let mut mid_sty   = AnsiColor::Grey[17].as_fg();  // medium -- tags, metadata
let mut under_sty = AnsiColor::Grey[12].as_fg();  // dim    -- IDs, secondary info

// Under cursor: darken text, add AnsiColored background
if under_cursor {
    name_sty  = AnsiColor::Grey[0].as_fg().with_bg(*selected_AnsiColor);
    mid_sty   = AnsiColor::Grey[6].as_fg().with_bg(*selected_AnsiColor);
    under_sty = AnsiColor::Grey[8].as_fg().with_bg(*selected_AnsiColor);
}

// Visual selection: slightly brighter than default
if in_visual {
    name_sty  = AnsiColor::Grey[26].as_fg();
    mid_sty   = AnsiColor::Grey[20].as_fg();
    under_sty = AnsiColor::Grey[16].as_fg();
}
```

The pattern: primary text is ~Grey[25], secondary ~Grey[17], tertiary ~Grey[12]. Under
cursor, everything shifts dark (0-8) against the status AnsiColor background. In visual
selection, everything shifts slightly brighter (+1-3 indices).

### Due Date AnsiColor Convention

```rust
// Overdue: muted warning
(" past due ", AnsiColor::Grey[12].with_fg(AnsiColor::Grey[25]))

// Due today: bright, attention-grabbing
(" Today ", AnsiColor::Grey[13].with_fg(AnsiColor::Grey[29]))

// Due in N days: subtle
(format!(" in {d}d "), AnsiColor::Grey[7].with_fg(AnsiColor::Grey[22]))
```

Higher bg Grey index = more urgency. Brighter fg = more attention needed.

### Choosing AnsiColors

Do not pick arbitrary named AnsiColors. Stick to the established palette:

- **Grey ramp** (`AnsiColor::Grey[0..30]`) for all neutral text and backgrounds. This is the
  primary tool. Pick indices from the role table above.
- **Status AnsiColors** from the status badge table for anything status-related.
- **Mode accent AnsiColors** from the mode table for mode-related UI.
- **`AnsiColor(N)` raw indices** only when matching an existing usage (e.g. `AnsiColor(221)` for
  active timer gold).

When adding a new UI element, find the closest existing element in the tables above and
reuse its AnsiColors. New accent AnsiColors should only be introduced for genuinely new semantic
categories, and should be chosen to not clash with existing status/mode AnsiColors.

### Modifiers

```rust
Modifier::BOLD
Modifier::DIM
Modifier::ITALIC
Modifier::UNDERLINED
Modifier::REVERSED      // swap fg/bg
Modifier::CROSSED_OUT   // strikethrough
```

Combine with `|`:

```rust
style | Modifier::BOLD | Modifier::ITALIC
```

### Palette Styles

For advanced styling (AnsiColored underlines, etc.) that can't be expressed with fg/bg/modifier:

```rust
// Register raw VT escape bytes at startup:
buf.set_palette(0, b"\x1b[4:3m\x1b[58;5;196m");  // curly underline, red

// Reference in rendering:
let ERROR_STYLE: Style = Style::palette(0);
area.with(ERROR_STYLE).text(buf, "error");
```

## Buffer

The rendering target. Maintains two frames and diffs them to minimize terminal output.

### Key Methods

```rust
let mut buf = Buffer::new(width, height);

buf.width()   -> u16
buf.height()  -> u16
buf.rect()    -> Rect        // full buffer area: Rect { x:0, y:0, w, h }

// Render to terminal (computes diff, writes only changes):
buf.render(&mut terminal);

// Force full redraw next frame:
buf.reset();

// Handle terminal resize:
buf.resize(new_w, new_h);

// Queue a scroll optimization:
buf.scroll(delta: i16);     // positive = up, negative = down
```

### Retained mode and damage declarations

By default (`Swap::Blank`) the work buffer is cleared after every render, so a
frame must repaint everything it wants visible. `buf.set_swap(Swap::Retained)`
seeds the next frame's work buffer with the rendered frame instead, allowing
partial overdraw: untouched cells persist on screen and emit nothing.

With retained mode, `buf.damage(rect)` declares which regions were drawn since
the last render. Damage is tracked per *line*: the first declaration switches
the frame to a row bitmap and the diff scans only marked rows (columns are
ignored). Declaring only empty rects means "nothing was drawn" and the diff
scans nothing, while a render with no declarations at all falls back to the
full scan, so callers that never declare are unaffected. Whole-grid rewrites —
`blank()` or a queued scroll — force the frame back to a full scan that later
declarations cannot narrow. This is a promise, not a hint: every cell written
must be covered by a declared rect or stale content stays on screen (debug
builds assert on undeclared writes).

Two gotchas on a retained grid:

- `.fill()` is a style merge (`set_style`): it does not erase glyphs the way a
  blank frame does. Clear a section with `buf.clear_rect(rect, style)` before
  redrawing it.
- When switching back to immediate-mode frames mid-stream (e.g. a screen that
  relies on the blank grid), call `buf.blank()` to discard the retained seed.

Chatt uses this on the room screen only: `DirtySections` masks flow from the
core to the render thread, `draw_room_screen` redraws and declares only dirty
sections, and every other screen stays on classic `Swap::Blank` frames.

### Low-level drawing (avoid when possible)

These exist but should be avoided in favor of the `DisplayRect` API, which eliminates
manual coordinate arithmetic:

```rust
// Draws text at absolute coordinates. Returns (end_x, end_y).
buf.set_stringn(x, y, text, max_width, style) -> (u16, u16)

// Applies style to every cell in a rect:
buf.set_style(area, style)
```

The problem with `set_stringn` is that you must manually track x/y positions, handle
alignment yourself, and coordinate math is error-prone. Use `DisplayRect` chaining instead.

## Block (Borders)

```rust
let block = Block {
    title: Some(" Panel "),
    borders: Block::LEFT,             // only left border
    border_style: AnsiColor::Grey[10].as_fg(),
    ..Default::default()
};

// Render the border, get inner area:
let inner = block.inner(area);
block.render(area, buf.current());

// Then render content inside `inner`
```

Border bitmask constants: `Block::LEFT`, `Block::RIGHT`, `Block::TOP`, `Block::BOTTOM`,
`Block::ALL`.

Default `border_type` is `BorderType::Thin` (unicode box drawing).

## LazyList (Scrollable Lists)

Renders a scrollable list of fixed-height items using a callback:

```rust
let list = LazyList::new(
    total_items,       // usize
    item_height,       // usize (rows per item)
    scroll_offset,     // usize
    |index, area, buf| {
        // Render item `index` into `area`
        let (top, bot) = area.v_split(1);
        top.with(name_style).text(buf, name);
        bot.with(dim_style).text(buf, subtitle);
    },
);

// Optional: add a border
let list = list.block(Block { borders: Block::LEFT, ..Default::default() });

list.render(area, buf);
```

The callback is only invoked for visible items (those within the scroll window).

## Complete Layout Example

Annotated version of extask's main `ui()` function pattern:

```rust
fn ui(buf: &mut Buffer, app: &mut App, mode: &mut dyn AppMode) {
    let mut r = Rect { x: 0, y: 0, w: buf.width(), h: buf.height() };

    // Mode gets first crack at carving out space (e.g. for input overlays)
    mode.render(buf, &mut windows, app, &mut r);

    // Status bar: 1 row from bottom
    let status_area = r.take_bottom(1);
    // Fill background, then chain left and right content:
    status_area.with(bg_style).fill(buf);
    let mut s = status_area.with(mode_style)
        .fmt(buf, format_args!(" {} ", mode_label));
    s = s.with(section_style)
        .fmt(buf, format_args!(" {} ", group));
    s = s.with(HAlign::Right);
    s = s.with(mode_style)
        .fmt(buf, format_args!(" {} ", position));

    // Keybindings panel: fixed width from right
    if show_bindings {
        let bindings_area = r.take_right(38);
        // render bindings list into bindings_area
    }

    // Messages panel: fixed height from bottom
    if show_messages {
        let msg_area = r.take_bottom(11);
        // render messages into msg_area
    }

    // Details panel: 40% of remaining from right
    if show_details {
        let details = r.take_right(0.4);
        // render task details into details
    }

    // Everything left is the main task list
    task_list::render(buf, app, r);
}
```

Key observations:

- Each `take_*` shrinks `r`, so the order determines priority. Status bar and panels are
  carved out first; the task list gets whatever remains.
- Ratios like `0.4` adapt to terminal size automatically.
- No absolute coordinates are computed by hand.

## Item Rendering Pattern

Inside a `LazyList` callback, each item gets a `Rect`. Split it vertically for multi-line
items, then chain draws within each line:

```rust
|i, area, buf| {
    let (top, bot) = area.v_split(1);

    // Top line: status badge + name (left), time + due (right)
    let cell = top.with(status_style).text(buf, " In Progress ").skip(1);
    cell.with(name_style).text(buf, task_name);

    top.take_right(7).with(time_style)
        .fmt(buf, format_args!(" {:02}:{:02} ", h, m));

    // Bottom line: ID on left, tags in middle, group on right
    raster_left(buf, &id_str, &mut bot.take_left(status_width), dim_style);
    let mut tag_r = bot.with(tag_style).skip(1);
    for tag in tags {
        tag_r = tag_r.text(buf, tag.as_str()).text(buf, " / ");
    }
}
```

Note how `top.take_right(7)` carves from the right of the top line for right-aligned
content (time display), while the left side is drawn with chaining. This is how left and
right content coexist on the same line without coordinate math.

## Common Pitfalls

**Don't use `set_stringn` for layout.** It requires manual x/y tracking and is error-prone.
Use `DisplayRect` chaining. The only time `set_stringn` is justified is inside legacy code
or when writing to an absolute position is truly necessary (like scrollbar glyph placement).

**Don't forget `take_*` mutates.** If you need the original rect after taking, clone or
split first.

**Return values matter.** Every `.text()`, `.fmt()`, `.skip()`, `.with()` returns an updated
`DisplayRect`. If you discard it, the cursor position is lost. Always chain or rebind:

```rust
// Wrong: cursor position from text() is discarded
area.display().text(buf, "a");
area.display().text(buf, "b");  // overwrites "a" at same position

// Right: chain to advance cursor
area.display().text(buf, "a").text(buf, "b");  // "ab"
```

**`fill` doesn't move the cursor.** It paints the background of the entire rect. Chain draws
after it:

```rust
area.with(bg).fill(buf).text(buf, "over the background");
```

**`.with()` replaces the style, doesn't merge.** Each `.with(Style)` call sets the full
style. To keep a background while changing foreground, construct the full style:

```rust
// Wrong: loses the background
r.with(bg_style).fill(buf).with(fg_only_style).text(buf, "oops");

// Right: style includes both fg and bg
r.with(bg_style).fill(buf).with(AnsiColor::Red1.with_bg(bg_AnsiColor)).text(buf, "good");
```

**`set_style` is for bulk style application.** It applies a style to every cell in a rect
(merging with existing content). Useful for selection highlighting after content is drawn:

```rust
// Draw content first, then apply highlight
render_task_line(area, buf);
if is_selected {
    buf.set_style(area, highlight_bg);
}
```
