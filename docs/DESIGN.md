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
- Host panel (dashboard, above the toolbar): see below.
- Main: sortable table. Sticky header. Row actions appear on hover/focus
  (`start` / `stop` / `restart` as small text buttons).
- Detail view: two panes — metadata on the left (narrow), logs on the right
  (wide). On small screens they stack.
- Log viewer: monospace, line numbers or timestamps toggle, stderr tinted,
  filter box, "follow" toggle that turns off automatically when the user
  scrolls up.

## Host panel

An htop-style block at the top of the dashboard, inside one bordered
`--surface` box. Collapsible (header button, `m`); the state is remembered
in `localStorage` (`cuthulu.sys.open`), as is the process sort
(`cuthulu.sys.mem`).

```
▾ host  my-box
    0 [||||||      31.0%]    1 [||||        22.0%]   Load  1.12 0.98 0.80
    2 [||           9.5%]    3 [|||||||     40.1%]   Tasks 312, 1708 threads; 2 running
  Mem [||||||||||||||||||           5.8G/15.5G]      Up    3d 4h
  Swp [|                             0.1G/2.0G]      Net   wlp2s0  ↓ 526K/s  ↑  34K/s
                                                     IP    192.168.1.57      local
                                                           100.115.90.103    tailscale
                                                     Disk  read 106K/s  write 819K/s
```

- **Meters are text**: `[`, pipes, spaces, the value right-aligned at the
  end, `]` — fixed character widths (26ch per CPU meter), like htop. The bar
  scales to the room left of the widest value, so its scale never shifts.
  The value is always shown; color is never the only signal. Meters carry
  `role="meter"` with the value as `aria-valuetext`.
- Pipes are colored by **zone**, using the existing tokens: each pipe takes
  `--ok` below the warn threshold, `--warn` up to the error threshold, then
  `--err`, so a fuller bar runs green → yellow → red and its tip shows the
  level. Thresholds: CPU 70 / 90 %, Mem 75 / 90 %, Swp 50 / 80 %; the
  1-minute load number uses load ÷ cores at 70 / 100 %. Labels and brackets
  are `--muted`.
- CPU meters use enough columns to stay about four rows tall (2 columns for
  ≤ 8 cores, up to 8); Mem/Swp span the CPU block's width. Below 600px one
  column.
- Info column: a label / value list (`dt` in `--muted`). Rates are padded
  to a fixed width so the line does not jitter; the network tooltip gives
  Mbit/s. `↓` / `↑` are plain text glyphs.
- Addresses: one per row, the default-route interface's first, each with a
  `--muted` kind label in an aligned column (`local`, `public`, `cgnat`,
  `tailscale`, `wireguard`, `zerotier`, `vpn`); the interface is in the
  tooltip.
- Process table (only with `CUTHULU_SYSTEM_PROCESSES=true`): top 10,
  sortable by `cpu%` or `mem%` (header buttons, the active one marked `▾`
  and `aria-sort`). Command in `--muted`, ellipsized, full text in the
  tooltip. CPU% is per core (can exceed 100). Below 600px it drops user and
  res.
- No new color tokens.

## Keyboard shortcuts

| Key        | Action |
|------------|--------|
| `/`        | focus filter |
| `j` / `k`  | move selection down / up |
| `Enter`    | open selected service |
| `l`        | open logs of selected service |
| `s`        | start / stop selected (stop asks for confirmation) |
| `r`        | restart selected |
| `m`        | collapse / expand the host panel (dashboard) |
| `t`        | toggle theme |
| `Esc`      | back / close / clear filter |
| `?`        | show shortcuts |

## The eye

The brand mark is a minimal eye: a circle with a pupil, drawn in `--accent`,
used as favicon and logo. The pupil can subtly indicate global health
(accent when all is well, `--err` when something is down). One idea, used
sparingly — no tentacles all over the UI.
