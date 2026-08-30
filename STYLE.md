# STYLE.md

Visual specification for Vestro interfaces — site, dashboard, checkouts.

The direction is **cold room**: clinical, near-white, thin strokes, a lot of air, a cool
undertone in the greys, labels that read as sample tags. Two principles govern it:

1. Colour is a signal, never a surface.
2. Separation is by tone change first, hairline second, accent outline only where content
   is live.

There are no gradients, shadows, filled colour panels, or corner radii.

## Colour

| Token     | Hex       | Role                                                     |
|-----------|-----------|----------------------------------------------------------|
| `paper`   | `#F1F4F5` | Page.                                                    |
| `surface` | `#FAFCFC` | Panel on the page — lighter, never darker.               |
| `sunken`  | `#E7ECEE` | Recess inside a panel; also the hairline colour.         |
| `ink`     | `#15181A` | Headings, primary values.                                |
| `muted`   | `#6B767D` | Body copy, 14px and above.                               |
| `label`   | `#5E686E` | Small uppercase labels, functional text below 12px.      |
| `faint`   | `#9CA6AC` | Decorative glyphs, placeholder data, disabled states.    |
| `accent`  | `#4E7C8C` | Live regions, focus, primary action, in-progress states. |
| `ok`      | `#4C7D69` | Good terminal state.                                     |
| `warn`    | `#9C8250` | Incomplete or requiring attention.                       |
| `stop`    | `#96635E` | Failed or reversed.                                      |

``` js
colors: {
  paper:'#F1F4F5', surface:'#FAFCFC', sunken:'#E7ECEE',
  ink:'#15181A', muted:'#6B767D', label:'#5E686E', faint:'#9CA6AC',
  accent:'#4E7C8C', ok:'#4C7D69', warn:'#9C8250', stop:'#96635E'
}
```

The same values exist as CSS variables (`--paper` etc.) for surfaces not on Tailwind.

There is no dark mode. Inverting these tokens does not hold; a dark variant would be a
separate direction.

## Status words

The only saturated ink on a page is a state word. A given state uses the same word and the
same colour across every surface.

- `muted` — nothing yet: open, pending, watching, unpaid, queued
- `accent` — in motion: detected, received, sweeping, retrying
- `ok` — done: confirmed, finalized, finished, swept, delivered
- `warn` — incomplete: partial, underpaid, overpaid, late, reorg
- `stop` — failed: expired, cancelled, orphaned, refunded

Status renders as coloured text, never a filled pill: uppercase, 9px, 0.22em tracking. The
word carries the meaning and the colour reinforces it. A 4px dot may accompany a status but
does not replace the word.

## Type

Jost sets human copy; IBM Plex Mono sets machine values — addresses, hashes, IDs, amounts,
timestamps, event names. The split is literal.

|         |                                                                                            |
|---------|--------------------------------------------------------------------------------------------|
| Display | 34–48px Jost 200, tight tracking, 1.12. One per page.                                      |
| Body    | 15px Jost 300, 1.65, max ~62ch.                                                            |
| UI text | 13px Jost 300.                                                                             |
| Data    | 11–13px mono 300, tabular figures in columns.                                              |
| Label   | 8.5–9px Jost 300, uppercase, 0.22em (0.3em for wordmark and page eyebrow), colour `label`. |

Weights above Jost 400 are unused; the lightness is the direction. Uppercase tracking does
not go below 0.2em. Labels run one or two words — anything requiring a verb is not a label.

## Layout

A 4px baseline, with section rhythm at 32 / 56 / 80. Container `max-w-6xl`, page padding
24 / 40. Radius is 0 throughout; status dots are the only round element. Space precedes a
line wherever two blocks need separating.

**Live region.** One `ring-1 ring-accent/25` per view, on the element that is changing —
the watched payment panel, or an event log while its invoice is open. The ring is removed
once an invoice reaches a terminal state. This outline is the single bold element in the
design and is not available for static cards.

## Components

- **Panel** — `bg-surface`, 24px padding, no border. Data blocks inside use `bg-sunken/60`.
- **Data row** — key left (mono, `faint` or `label`), value right (mono, `ink`), numbers right-aligned.
- **Link** — `ink`, no underline, `accent` on hover. One trailing `→` link per view.
- **Button** — `ring-1 ring-accent`, transparent, `accent` text, filling on hover; the only element that becomes a solid field. Secondary: `ink` text, `ring-sunken`.
- **Input** — `bg-sunken/50`, no border, bottom hairline turning `accent` on focus. Label above.
- **Table** — no vertical rules or striping. Header in label style over a `sunken` band, rows split by hairlines.
- **Warning** — 2px left rule in `warn` or `stop`, 16px padding, text in `muted`. No banner or icon.
- **Empty state** — a label and one sentence naming the available action.

## Motion

Ambient motion is limited to two effects: a breathing dot on a live region (opacity .25→.9,
3.2s) and a row reveal on new events (opacity plus 2px rise, 500ms). Everything else is a
150ms colour transition on hover and focus. Space is reserved ahead of async content so
nothing reflows. Under `prefers-reduced-motion`, the end state renders immediately.

## Per-network

Checkouts diverge per token handler; the chassis does not. Layout, type, greys, and status
vocabulary are identical across surfaces. Network identity enters through the accent colour
and wordmark alone — no branded gradients or backgrounds.

A network accent derives from the brand hue at roughly 30% saturation and 42% lightness,
verified at 4.5:1 on `surface`. Default and EVM are `#4E7C8C`, Solana `#6E6A93`. Site and
dashboard always use the default.

## Contrast

`label` on `paper` is 4.9:1 and is the floor for text. `muted` is 4.0:1 and is restricted to
14px and above. `faint` falls below the floor by design and carries no information. Focus is
`outline: 1px accent` at 4px offset on every interactive element.
