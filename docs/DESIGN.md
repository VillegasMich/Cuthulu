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
| `--hover`        | `#1c201d`  | `#efefea`  | row hover / selection background |
| `--border`       | `#262b28`  | `#dcdcd5`  | 1px rules (decorative) |
| `--line`         | `#60675f`  | `#858a83`  | outlines of buttons, inputs, `kbd` |
| `--text`         | `#d4d7d2`  | `#1c1e1c`  | body text |
| `--muted`        | `#969d96`  | `#5a5f5a`  | secondary text, stopped |
| `--accent`       | `#7fd962`  | `#2a7530`  | focus, links, the eye, section headings |
| `--ok`           | `#5fb85f`  | `#2a7530`  | running / healthy |
| `--warn`         | `#d4a72c`  | `#8a5f00`  | restarting / starting |
| `--err`          | `#ec5f57`  | `#c62828`  | exited non-zero / unhealthy / dead |
| `--log-stderr`   | `#e58a84`  | `#b3261e`  | stderr lines |
| `--key`          | `#5fadb7`  | `#1d6a75`  | key labels: detail `dt`, host-panel labels |
| `--project`      | `#c495d3`  | `#883a8a`  | project / compose group |
| `--tag`          | `#7eaee6`  | `#2259a0`  | image tag or digest (`:0.2.0`, `@sha256:…`) |
| `--ansi-black`   | `#5a615b`  | `#1c1e1c`  | ANSI color 0 (SGR 30 / 40) |
| `--ansi-red`     | `#ec5f57`  | `#c62828`  | ANSI color 1 (SGR 31 / 41) |
| `--ansi-green`   | `#5fb85f`  | `#2a7530`  | ANSI color 2 (SGR 32 / 42) |
| `--ansi-yellow`  | `#d4a72c`  | `#8a5f00`  | ANSI color 3 (SGR 33 / 43) |
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

Contrast (WCAG 2.x): every text token is at least 4.5:1 against `--bg`,
`--surface` and `--hover` in both themes (`--muted` ≈ 6:1,
`--key` / `--project` / `--tag` 5.4–8:1); `--line` is at least 3:1 against
`--bg` and `--surface`, so controls stay identifiable. `--border` is for
decorative rules only and may stay faint. Check new tokens against all three
backgrounds before adding them.

The data colors (`--key`, `--project`, `--tag`) are a small terminal palette
— cyan keys like htop, magenta project, blue tag — used only where they help
scanning: on the dashboard (project column, image tag, host-panel labels) and
in the detail metadata. Everything else stays neutral; stopped rows stay
`--muted` throughout.

Theme selection: follow `prefers-color-scheme` by default; a toggle (`t`)
stores an explicit choice in `localStorage` and sets `data-theme` on `<html>`.

## Layout

```
┌──────────────────────────────────────────────────────────────────────┐
│ [←] (◉) cuthulu   3 running · 1 stopped (hidden) [bell] ● docker [☾]│
├──────────────────────────────────────────────────────────────────────┤
│ [/ filter…]   [ ] show stopped                                       │
│ STATE    NAME                     IMAGE                      UPTIME  │
│ ● run    auto-git-commit-tool     villegasmich/auto-git…:0.2.0  20m  │
│ ● run    claude-session-starter   villegasmich/claude-ses…:0.1.0 20m │
│ ● run    producer-tag-on-merge    villegasmich/producer-t…:0.1.0 20m │
└──────────────────────────────────────────────────────────────────────┘
```

- Top bar: back arrow, logo/eye, counts, notifications, connection state,
  healthchecks.io, Tailscale, GitHub, theme toggle.
  - Back (`←`) goes to the previous cuthulu page in this tab's history, or to
    `/` when there is none. On the dashboard without history it is hidden but
    keeps its slot, so the eye never shifts between pages.
  - The theme toggle shows the *current* theme: a sun in light, a moon in dark
    (pure CSS, so it also follows `prefers-color-scheme` live).
  - Notifications (`.btn.icon`, before the connection state): a bell, struck
    through when service alerts are off. Opens the notifications dialog.
  - Tailscale (left of GitHub, only when Tailscale is available): a
    link styled as `.btn.icon` with a three-node network glyph; opens this
    machine in the Tailscale admin console in a new tab. The tooltip names
    host, IP and tailnet. Hidden (not reserved) when unavailable. No brand
    logo.
  - healthchecks.io (left of Tailscale, only when `CUTHULU_HEALTHCHECK_URL`
    is set): a `.btn.icon` link with a pulse-line glyph; opens the
    healthchecks.io dashboard in a new tab. The tooltip states the last
    ping (`healthchecks.io: last ping ok 2m ago`, `… failed 30s ago: <error>`,
    `… no ping yet`, `… pings off`); the icon is `--err` (class `bad`) when
    the last ping failed or was skipped, `--muted` otherwise. No brand logo.
  - GitHub (between Tailscale and the theme toggle, always shown): a link
    styled as `.btn.icon` with the GitHub mark; opens the project repository
    (`Cargo.toml` `repository`) in a new tab.
- Host panel (dashboard, above the toolbar): see below.
- Toolbar (dashboard): filter input and a `show stopped` checkbox. Stopped
  services are hidden by default (only `running` / `restarting` / `paused`
  rows show); the counts keep the full totals and mark the hidden part. The
  choice is stored in `localStorage` (`cuthulu.index.stopped`).
- Main: sortable table. Sticky header. Row actions appear on hover/focus
  (`start` / `stop` / `restart` as small text buttons). Column widths are
  adjustable with header splitters (see [Splitters](#splitters)).
- Detail view: two panes — metadata on the left (narrow, 360px by default),
  logs on the right (wide), with a splitter between them. On small screens
  they stack.
  - Metadata is a key/value list: keys in `--key`, values in `--text`, a
    1px `--border` rule under each row (not the last). Project in
    `--project`, image tag in `--tag`; in ports the host address and `/proto`
    are `--muted`, so the port numbers stand out.
  - Sections below it (`todo`, `env`, `labels`) start with a `--border` rule
    and a bold `--accent` heading; their counts stay `--muted`.
- Log viewer: monospace, line numbers or timestamps toggle, stderr tinted,
  filter box, "follow" toggle that turns off automatically when the user
  scrolls up. ANSI colors from the service are rendered with the `--ansi-*`
  palette (bold, dim, italic, underline too); a background without a
  foreground shows text in `--bg`, like a badge. Lines without ANSI color
  get their level keyword colored: error/fatal `--err`, warn `--warn`,
  info `--ok`, debug/trace `--muted` — the keyword only, never the whole
  line. Colored spans override the stderr tint; the rest of an stderr line
  keeps it. A `color` checkbox turns all of it off (remembered).
- Bell (notify) toggle: services rows get a 12px bell at the far right of
  the actions cell — `--accent` when the service is watched (always
  visible), otherwise `--muted` and shown on hover/focus like the row
  actions. With alerts switched off globally, watched bells turn `--muted`.
  The detail page has a `notify` text button with the bell after the
  actions, `.on` (accent border) while watched. `b` toggles it. Read-only:
  watched rows keep a static bell, the detail page says `notify: on|off`.
  Cuthulu's own row has none.
- Notifications dialog (same box as the help dialog): a `kv` list —
  `alerts` (checkbox on/off), `watched` (names), `email` / `healthcheck`
  (`configured` in `--ok`, or `off · <the env var to set>` in `--muted`;
  never addresses or URLs), `browser` (permission state, `allow` button) —
  then `send test` with a per-channel result line (`sent` `--ok`,
  `failed: …` `--err`). Read-only shows the same, without controls.
- Browser alert: a desktop notification `<name> is down` / `<state> ·
  cuthulu` (`<state> · stopped from the dashboard` after a stop clicked
  there); without permission the same text in the error flash.
- "Restarts by itself": a service stopped from Cuthulu that something else
  started again gets a `↻` (`--warn`, tooltip explains) before its name in
  the table, ` · restarts by itself` (`--warn`) on the detail state line,
  and an error flash suggesting to stop it at its source. No new color
  tokens for any of this.
- TODO list (detail view, left pane, below the metadata): heading
  `todo 2/5` (done/total, muted count; styled like the other sections), one line per item: `[ ]` / `[x]`
  text toggle (`--muted`, `--ok` when done), the text, and a small `del`
  button that appears on hover/focus like row actions. Done items are
  `--muted` and struck through. An input + `add` button below; Enter adds.
  The list scrolls past 40vh. No new color tokens.

## Splitters

Pane borders that can be dragged, so clipped text and boxes can be given more
room. One component (`.split`, `splitter()` in `app.js`), used in three places:

| Where | Moves | Bounds | Stored as |
|-------|-------|--------|-----------|
| Detail view, between info and logs | info column width | 220px … 70% | `cuthulu.split.meta` (px) |
| Dashboard table, right edge of `state` … `ports` headers | the border between two columns | 56px per column, 120px for `name` | `cuthulu.split.cols` (% of the table, per column) |
| Host panel, bottom border | panel body height (a max-height; it scrolls) | 2 text lines … full height | `cuthulu.split.sys` (px) |

- **Look:** no new chrome. The line is the existing 1px `--border` rule —
  the logs box's left border, the host panel's bottom border, or a 1px
  `--border` line at each resizable header's right edge — and turns
  `--accent` on hover, while dragging, and on keyboard focus. The hit area is
  wider than the line (the 16px gap on the detail view, 7px elsewhere);
  the cursor is `col-resize` / `row-resize`.
- **Columns** trade width with their neighbour only: dragging a border
  moves just that border. `name` has no width of its own and takes what is
  left. Widths are a fixed table layout above 900px.
- **Host panel** heights snap to whole text lines, so no row is cut in half.
  Dragging back to full height forgets the limit.
- **Keyboard:** each splitter is focusable (`role="separator"`,
  `aria-orientation`, `aria-valuenow/min/max` in px). Arrow keys along its
  axis step 16px (one line for the host panel), 4× with shift; `Home` /
  `End` or a double-click restore the default.
- **Persistence:** per browser in `localStorage`; `theme.js` applies stored
  sizes as CSS custom properties (`--split-meta`, `--split-sys`, `--col-*`)
  before first paint, so nothing jumps on load. Without storage, sizes last
  for the page.
- **Narrow screens** (≤ 900px), where the detail panes stack and the table
  drops columns, hide the detail and column splitters and ignore stored
  column widths.

## Icons

Text labels win by default. The few icons (the eye, back arrow, sun/moon,
bell, the Tailscale network glyph, the healthchecks.io pulse line) are hand-written inline SVGs: 14px
in a 24-unit viewBox, `fill: none`, `stroke: currentColor`, round
caps/joins, no fills except the pupil. The one exception is the GitHub
mark, the official filled silhouette (16-unit viewBox, still 14px) with
`.ico.fill` (`fill: currentColor`, no stroke) so it follows the theme like
the rest. Icon
buttons (`.btn.icon`) keep the box of a text `.btn`, are `--muted` until
hover, and always carry `aria-label` + `title`.

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
  1-minute load number uses load ÷ cores at 70 / 100 %. Labels are `--key`
  (cyan, as in htop), brackets `--muted`.
- CPU meters use enough columns to stay about four rows tall (2 columns for
  ≤ 8 cores, up to 8); Mem/Swp span the CPU block's width. Below 600px one
  column.
- Info column: a label / value list (`dt` in `--key`). Rates are padded
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
| `a`        | show / hide stopped services |
| `b`        | notify when the selected service goes down (bell) |
| `t`        | toggle theme |
| `Esc`      | back (same as the back arrow) / close / clear filter / clear selection |
| `?`        | show shortcuts |
| `←` `→` / `↑` `↓` | resize, when a splitter has focus (`Home` / `End`: reset) |

## The eye

The brand mark is a minimal eye: a circle with a pupil, drawn in `--accent`,
used as favicon and logo. The pupil can subtly indicate global health
(accent when all is well, `--err` when something is down). One idea, used
sparingly — no tentacles all over the UI.
