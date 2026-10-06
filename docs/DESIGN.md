# Design Guide

Cuthulu should feel like a tool a developer built for themselves: dense,
fast, monospace, keyboard-driven. Think `htop`, `lazydocker`, a good terminal
theme — not a SaaS landing page.

## Principles

1. **Information density over decoration.** A table row per service, not a
   card per service.
2. **Text first.** Status is a word plus a small colored marker, never color
   alone.
3. **Quiet by default, loud when broken.** Healthy services are visually
   calm; stopped/unhealthy ones stand out.
4. **Keyboard is first class.** Everything reachable without a mouse.
5. **Instant.** No spinners for things that take <200ms, no entrance animations.

## Avoid (the "AI-generated" look)

- Gradients, glassmorphism, blurred backgrounds, glow effects
- Purple/indigo-on-white default palettes
- Large rounded cards with soft drop shadows, `rounded-2xl` everywhere
- Emoji as icons, sparkles, hero sections, marketing copy
- Oversized padding and font sizes that fit 5 rows on a screen
- Generic icon packs for everything — prefer text labels

## Typography

- One monospace family for the whole UI: **JetBrains Mono** (vendored,
  OFL licence), fallback `ui-monospace, "SF Mono", Menlo, Consolas, monospace`.
- Base size 13–14px, line-height 1.45. Headings differ by weight, not size jumps.
- Numbers use tabular figures.

## Color tokens

All colors are CSS custom properties on `:root`; themes only redefine tokens.

| Token            | Dark       | Light      | Use |
|------------------|------------|------------|-----|
| `--bg`           | `#0f1110`  | `#f7f7f4`  | page background |
| `--surface`      | `#161917`  | `#ffffff`  | panels, table header |
| `--border`       | `#262b28`  | `#dcdcd5`  | 1px rules |
| `--text`         | `#d4d7d2`  | `#1c1e1c`  | body text |
| `--muted`        | `#7d847e`  | `#6b706b`  | secondary text, stopped |
| `--accent`       | `#7fd962`  | `#2f7d32`  | focus, links, the eye |
| `--ok`           | `#5fb85f`  | `#2e7d32`  | running / healthy |
| `--warn`         | `#d4a72c`  | `#9a6b00`  | restarting / starting |
| `--err`          | `#e5534b`  | `#c62828`  | exited non-zero / unhealthy / dead |
| `--log-stderr`   | `#e58a84`  | `#b3261e`  | stderr lines |
| `--ansi-black`   | `#5a615b`  | `#1c1e1c`  | ANSI color 0 (SGR 30 / 40) |
| `--ansi-red`     | `#e5534b`  | `#c62828`  | ANSI color 1 (SGR 31 / 41) |
| `--ansi-green`   | `#5fb85f`  | `#2e7d32`  | ANSI color 2 (SGR 32 / 42) |
| `--ansi-yellow`  | `#d4a72c`  | `#9a6b00`  | ANSI color 3 (SGR 33 / 43) |
| `--ansi-blue`    | `#5f9bd8`  | `#1f5fa8`  | ANSI color 4 (SGR 34 / 44) |
| `--ansi-magenta` | `#b781c6`  | `#8e3a8e`  | ANSI color 5 (SGR 35 / 45) |
| `--ansi-cyan`    | `#4fb0b0`  | `#00727a`  | ANSI color 6 (SGR 36 / 46) |
| `--ansi-white`   | `#b8bcb6`  | `#6b706b`  | ANSI color 7 (SGR 37 / 47) |
| `--ansi-bright-black` | `#7d847e`  | `#5c615c`  | ANSI color 8 (SGR 90 / 100) |
| `--ansi-bright-red` | `#f07c74`  | `#b3261e`  | ANSI color 9 (SGR 91 / 101) |
| `--ansi-bright-green` | `#7fd962`  | `#1b6e20`  | ANSI color 10 (SGR 92 / 102) |
| `--ansi-bright-yellow` | `#e8c35a`  | `#7d5700`  | ANSI color 11 (SGR 93 / 103) |
| `--ansi-bright-blue` | `#8ab6eb`  | `#174f8f`  | ANSI color 12 (SGR 94 / 104) |
| `--ansi-bright-magenta` | `#d0a0de`  | `#7a2e7a`  | ANSI color 13 (SGR 95 / 105) |
| `--ansi-bright-cyan` | `#72cfcf`  | `#005e64`  | ANSI color 14 (SGR 96 / 106) |
| `--ansi-bright-white` | `#eceee9`  | `#4a4e4a`  | ANSI color 15 (SGR 97 / 107) |

The `--ansi-*` palette renders colors emitted by services in their logs. It
reuses the status colors where they overlap and is tuned for contrast on
`--bg` rather than fidelity: on the light theme "white" and "bright" colors
are darker, so nothing turns invisible. No neon.

Theme selection: follow `prefers-color-scheme` by default; a toggle (`t`)
stores an explicit choice in `localStorage` and sets `data-theme` on `<html>`.

## Layout

```
┌──────────────────────────────────────────────────────────────────────┐
│ (◉) cuthulu   3 running · 0 stopped     [/ filter…]   [all▾]  [t]   │
├──────────────────────────────────────────────────────────────────────┤
│ STATE    NAME                     IMAGE                      UPTIME  │
│ ● run    auto-git-commit-tool     villegasmich/auto-git…:0.2.0  20m  │
│ ● run    claude-session-starter   villegasmich/claude-ses…:0.1.0 20m │
│ ● run    producer-tag-on-merge    villegasmich/producer-t…:0.1.0 20m │
│ ○ exit 1 some-old-thing           postgres:16                    —   │
└──────────────────────────────────────────────────────────────────────┘
```

- Top bar: logo/eye, counts, filter input, state filter, theme toggle.
- Main: sortable table. Sticky header. Row actions appear on hover/focus
  (`start` / `stop` / `restart` as small text buttons).
- Detail view: two panes — metadata on the left (narrow), logs on the right
  (wide). On small screens they stack.
- Log viewer: monospace, line numbers or timestamps toggle, stderr tinted,
  filter box, "follow" toggle that turns off automatically when the user
  scrolls up. ANSI colors from the service are rendered with the `--ansi-*`
  palette (bold, dim, italic, underline too); a background without a
  foreground shows text in `--bg`, like a badge. Lines without ANSI color
  get their level keyword colored: error/fatal `--err`, warn `--warn`,
  info `--ok`, debug/trace `--muted` — the keyword only, never the whole
  line. Colored spans override the stderr tint; the rest of an stderr line
  keeps it. A `color` checkbox turns all of it off (remembered).
- TODO list (detail view, left pane, below the metadata): heading
  `todo 2/5` (done/total, muted count), one line per item: `[ ]` / `[x]`
  text toggle (`--muted`, `--ok` when done), the text, and a small `del`
  button that appears on hover/focus like row actions. Done items are
  `--muted` and struck through. An input + `add` button below; Enter adds.
  The list scrolls past 40vh. No new color tokens.

## Keyboard shortcuts

| Key        | Action |
|------------|--------|
| `/`        | focus filter |
| `j` / `k`  | move selection down / up |
| `Enter`    | open selected service |
| `l`        | open logs of selected service |
| `s`        | start / stop selected (stop asks for confirmation) |
| `r`        | restart selected |
| `t`        | toggle theme |
| `Esc`      | back / close / clear filter |
| `?`        | show shortcuts |

## The eye

The brand mark is a minimal eye: a circle with a pupil, drawn in `--accent`,
used as favicon and logo. The pupil can subtly indicate global health
(accent when all is well, `--err` when something is down). One idea, used
sparingly — no tentacles all over the UI.
