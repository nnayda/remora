---
version: alpha
name: Remora
description: >-
  Design system for Remora — persistent remote coding-agent sessions from any
  device. A dark-first, dense developer tool built around a calm command-center
  aesthetic and one signature moment: the agent activity pulse.
colors:
  # --- RAW: NEUTRAL INK RAMP (dark surfaces) ---
  ink-1000: "#08090c"
  ink-950: "#0b0c10"
  ink-900: "#101117"
  ink-850: "#15161d"
  ink-800: "#1a1b22"
  ink-750: "#20212a"
  ink-700: "#282a34"
  ink-650: "#32343f"
  ink-600: "#3e404c"
  # --- RAW: PAPER RAMP (text on dark) ---
  paper-50: "#f4f5f8"
  paper-200: "#d6d9e1"
  paper-400: "#9ca0ae"
  paper-500: "#777c8b"
  paper-600: "#565a68"
  # --- RAW: NEUTRAL FOR LIGHT THEME ---
  cloud-0: "#ffffff"
  cloud-50: "#fafbfc"
  cloud-100: "#f4f5f7"
  cloud-150: "#edeef1"
  cloud-200: "#e3e5ea"
  cloud-300: "#d2d5dc"
  slate-900: "#14151a"
  slate-700: "#3c3f49"
  slate-500: "#5e6271"
  slate-400: "#82869a"
  # --- RAW: SIGNATURE ACCENT (marine blue) ---
  marine-300: "#86b2ff"
  marine-400: "#4d8dff"
  marine-500: "#1e6ff5"
  marine-600: "#155cd9"
  marine-700: "#1048ad"
  marine-pulse: "#6ea4ff"
  # --- RAW: STATUS (chrome only; the terminal owns its own ANSI) ---
  green-400: "#4ecb83"
  green-500: "#34b26c"
  amber-400: "#e8a33d"
  amber-500: "#d08a1e"
  red-400: "#f0556b"
  red-500: "#dc3c53"
  blue-400: "#54a0f0"
  blue-500: "#3a85de"
  # --- SEMANTIC: SURFACES (dark, default) ---
  bg-app: "{colors.ink-950}"
  bg-sidebar: "{colors.ink-900}"
  bg-panel: "{colors.ink-800}"
  bg-elevated: "{colors.ink-850}"
  bg-hover: "{colors.ink-750}"
  bg-active: "{colors.ink-700}"
  bg-input: "{colors.ink-850}"
  bg-overlay: "rgba(6, 7, 10, 0.62)"
  # --- SEMANTIC: BORDERS (hairline-first) ---
  border-hairline: "rgba(255, 255, 255, 0.07)"
  border-default: "rgba(255, 255, 255, 0.1)"
  border-strong: "rgba(255, 255, 255, 0.16)"
  # --- SEMANTIC: TEXT ---
  text-primary: "{colors.paper-50}"
  text-secondary: "{colors.paper-400}"
  text-muted: "{colors.paper-500}"
  text-disabled: "{colors.paper-600}"
  text-inverse: "{colors.ink-950}"
  # --- SEMANTIC: ACCENT ---
  # `primary` is an alias of the marine accent — Remora's single brand color.
  primary: "{colors.marine-500}"
  accent: "{colors.marine-500}"
  accent-hover: "{colors.marine-400}"
  accent-press: "{colors.marine-600}"
  accent-bright: "{colors.marine-pulse}"
  accent-subtle: "rgba(30, 111, 245, 0.14)"
  accent-border: "rgba(30, 111, 245, 0.42)"
  on-accent: "#ffffff"
  focus-ring: "rgba(30, 111, 245, 0.55)"
  # --- SEMANTIC: STATUS (chrome) ---
  success: "{colors.green-400}"
  success-bg: "rgba(78, 203, 131, 0.14)"
  warning: "{colors.amber-400}"
  warning-bg: "rgba(232, 163, 61, 0.14)"
  danger: "{colors.red-400}"
  danger-bg: "rgba(240, 85, 107, 0.14)"
  info: "{colors.blue-400}"
  info-bg: "rgba(84, 160, 240, 0.14)"
  # --- SEMANTIC: SIGNATURE PULSE (canonical = marine) ---
  pulse: "{colors.marine-pulse}"
  # --- TERMINAL (stays dark in both themes) ---
  term-bg: "{colors.ink-1000}"
  term-fg: "{colors.paper-200}"
typography:
  display:
    fontFamily: '"Inter", -apple-system, BlinkMacSystemFont, "Segoe UI", system-ui, sans-serif'
    fontSize: "2.25rem"
    fontWeight: 600
    lineHeight: 1.2
    letterSpacing: "-0.012em"
  heading:
    fontFamily: '"Inter", -apple-system, BlinkMacSystemFont, "Segoe UI", system-ui, sans-serif'
    fontSize: "1.25rem"
    fontWeight: 600
    lineHeight: 1.35
    letterSpacing: "0em"
  title:
    fontFamily: '"Inter", -apple-system, BlinkMacSystemFont, "Segoe UI", system-ui, sans-serif'
    fontSize: "1rem"
    fontWeight: 600
    lineHeight: 1.35
    letterSpacing: "0em"
  body:
    fontFamily: '"Inter", -apple-system, BlinkMacSystemFont, "Segoe UI", system-ui, sans-serif'
    fontSize: "0.875rem"
    fontWeight: 400
    lineHeight: 1.5
    letterSpacing: "0em"
  ui:
    fontFamily: '"Inter", -apple-system, BlinkMacSystemFont, "Segoe UI", system-ui, sans-serif'
    fontSize: "0.8125rem"
    fontWeight: 500
    lineHeight: 1.35
    letterSpacing: "0em"
  meta:
    fontFamily: '"Inter", -apple-system, BlinkMacSystemFont, "Segoe UI", system-ui, sans-serif'
    fontSize: "0.75rem"
    fontWeight: 500
    lineHeight: 1.35
    letterSpacing: "0em"
  label:
    fontFamily: '"Inter", -apple-system, BlinkMacSystemFont, "Segoe UI", system-ui, sans-serif'
    fontSize: "0.6875rem"
    fontWeight: 600
    lineHeight: 1
    letterSpacing: "0.04em"
  terminal:
    fontFamily: '"JetBrains Mono", ui-monospace, "SF Mono", "Berkeley Mono", Menlo, Consolas, monospace'
    fontSize: "0.8125rem"
    fontWeight: 400
    lineHeight: 1.55
    letterSpacing: "0em"
  code:
    fontFamily: '"JetBrains Mono", ui-monospace, "SF Mono", "Berkeley Mono", Menlo, Consolas, monospace'
    fontSize: "0.8125rem"
    fontWeight: 400
    lineHeight: 1.55
    letterSpacing: "0em"
rounded:
  xs: "3px"
  sm: "5px"
  md: "7px"
  lg: "10px"
  xl: "14px"
  pill: "999px"
spacing:
  "0": "0"
  "1": "2px"
  "2": "4px"
  "3": "6px"
  "4": "8px"
  "5": "10px"
  "6": "12px"
  "7": "14px"
  "8": "16px"
  "10": "20px"
  "12": "24px"
  "16": "32px"
  "20": "40px"
  "24": "48px"
  "32": "64px"
components:
  button-primary:
    backgroundColor: "{colors.accent}"
    textColor: "{colors.on-accent}"
    typography: "{typography.ui}"
    height: "30px"
    rounded: "{rounded.sm}"
  button-primary-hover:
    backgroundColor: "{colors.accent-hover}"
    textColor: "{colors.on-accent}"
  button-primary-pressed:
    backgroundColor: "{colors.accent-press}"
    textColor: "{colors.on-accent}"
  button-secondary:
    backgroundColor: "{colors.bg-panel}"
    textColor: "{colors.text-primary}"
    typography: "{typography.ui}"
    height: "30px"
    rounded: "{rounded.sm}"
  button-ghost:
    backgroundColor: "transparent"
    textColor: "{colors.text-secondary}"
    typography: "{typography.ui}"
    height: "30px"
    rounded: "{rounded.sm}"
  button-danger:
    backgroundColor: "transparent"
    textColor: "{colors.danger}"
    typography: "{typography.ui}"
    height: "30px"
    rounded: "{rounded.sm}"
  tag:
    backgroundColor: "{colors.bg-elevated}"
    textColor: "{colors.text-secondary}"
    typography: "{typography.code}"
    height: "20px"
    rounded: "{rounded.xs}"
  tag-branch:
    backgroundColor: "{colors.bg-elevated}"
    textColor: "{colors.accent}"
    typography: "{typography.code}"
    height: "20px"
    rounded: "{rounded.xs}"
  badge:
    backgroundColor: "{colors.bg-hover}"
    textColor: "{colors.text-secondary}"
    typography: "{typography.label}"
    height: "18px"
    rounded: "{rounded.xs}"
  badge-accent:
    backgroundColor: "{colors.accent-subtle}"
    textColor: "{colors.accent}"
    typography: "{typography.label}"
    height: "18px"
    rounded: "{rounded.xs}"
  input:
    backgroundColor: "{colors.bg-input}"
    textColor: "{colors.text-primary}"
    typography: "{typography.body}"
    height: "30px"
    rounded: "{rounded.sm}"
  input-focus:
    backgroundColor: "{colors.bg-input}"
    textColor: "{colors.text-primary}"
  select:
    backgroundColor: "{colors.bg-input}"
    textColor: "{colors.text-primary}"
    typography: "{typography.body}"
    height: "30px"
    rounded: "{rounded.sm}"
  tab:
    backgroundColor: "transparent"
    textColor: "{colors.text-secondary}"
    typography: "{typography.ui}"
    height: "38px"
  tab-active:
    backgroundColor: "{colors.bg-app}"
    textColor: "{colors.text-primary}"
    typography: "{typography.ui}"
    height: "38px"
  session-row:
    backgroundColor: "transparent"
    textColor: "{colors.text-primary}"
    typography: "{typography.ui}"
    rounded: "{rounded.sm}"
  session-row-active:
    backgroundColor: "{colors.accent-subtle}"
    textColor: "{colors.text-primary}"
    typography: "{typography.ui}"
    rounded: "{rounded.sm}"
  count-chip:
    backgroundColor: "{colors.bg-active}"
    textColor: "{colors.text-secondary}"
    rounded: "{rounded.xs}"
  dialog:
    backgroundColor: "{colors.bg-panel}"
    textColor: "{colors.text-primary}"
    rounded: "{rounded.xl}"
  activity-pulse-working:
    backgroundColor: "{colors.pulse}"
  activity-pulse-needs:
    backgroundColor: "{colors.pulse}"
  activity-pulse-idle:
    backgroundColor: "{colors.text-secondary}"
  activity-pulse-done:
    backgroundColor: "{colors.success}"
  activity-pulse-error:
    backgroundColor: "{colors.danger}"
---

# Remora — Design System

This is the written home for Remora's design *decisions*. The exact token values
live in `apps/desktop/src/styles/tokens/*.css` and are mirrored in the YAML
front matter above; this prose explains **why** those values exist and **how** to
apply them — the calls a contributor or an AI agent would otherwise have to
re-derive from the CSS every time.

The front matter is the machine-readable contract (lint it with
`pnpm design:lint`); the body is the human contract. When the two disagree, the
token CSS is the source of truth — open a PR to bring this file back in sync.

## Overview

Remora lets you spawn a coding agent on a remote host and drive it from any
device, with the session surviving disconnects. The UI is the calm shell around
a live terminal: a sidebar of sessions, a tab bar, and the agent's PTY. So the
design system serves one job — **stay out of the way of the terminal while making
session state legible at a glance.**

**Aesthetic: industrial / utilitarian, dark-first.** Think a command center, not
a marketing site. Dense dev-tool scale (13px is the default UI size), hairline
borders instead of heavy cards, soft low-opacity shadows, and color used rarely
and meaningfully. Light theme is co-equal, not a bolt-on — it follows the OS
setting and has its own tuned surfaces and shadows.

**The one memorable thing: the activity pulse.** A calm breathing glow that says
"your agent is working" even while you're away, and a faster, higher-contrast
beat for "needs you." Every other motion in the app is a quiet functional
transition. The pulse is where Remora gets its face. Protect it: don't add a
second competing animation, and don't let the pulse drift off the signature
marine color.

**Posture in three lines:**
- Legibility over decoration. The active rail and a tint do the work a heavy
  highlight would; type weight stays understated.
- Monospace means "machine value." Branch names, model ids, paths, hosts, and
  diff counts are mono; prose and labels are sans.
- One accent. Marine blue is the only brand color. If a screen needs a second
  accent to make sense, the layout is wrong, not the palette.

## Colors

**Approach: restrained.** One signature accent (marine blue) over two neutral
ramps — `ink` for dark surfaces, `paper` for text on dark; `cloud`/`slate` for
the light theme. Status colors (green/amber/red/blue) are chrome-only and appear
as small dots, badges, and diff stats — never as fields of color.

**Marine blue `#1e6ff5` is the accent** and the only brand color. It marks the
one primary action on a surface, the focus ring, the active session, and the
activity pulse. Use `accent-subtle` (a 14%-opacity tint) for active-row and
selected fills; use the solid `accent` for primary buttons and the pulse. If you
reach for a second hue to create hierarchy, use weight, spacing, or a neutral
step instead.

**Dark is the default theme.** The `ink` ramp runs from `#08090c` (deepest, the
terminal floor) up through `#1a1b22` (panels) to `#3e404c` (strong borders).
Text rides the `paper` ramp: `#f4f5f8` primary, `#9ca0ae` secondary, `#777c8b`
muted. Borders are translucent white hairlines (7–16% alpha), never solid lines —
this keeps depth quiet on dark surfaces.

**Light is co-equal.** It is a real redesign of the surfaces (`cloud`/`slate`),
not an inversion: shadows soften, the accent shifts one step deeper to
`marine-600` for contrast on white, and saturation drops slightly. Light applies
two ways — automatically via `prefers-color-scheme: light`, or forced through
`:root[data-theme="light"]`.

**The terminal owns its own palette and stays dark in both themes.** Chrome status
colors (`success`/`warning`/`danger`/`info`) never bleed into the terminal — the
PTY renders its own ANSI set (`--ansi-*`), and `term-bg` stays near-black
(`#0b0c10` dark, `#14151a` even under light chrome) so the agent's output reads
consistently regardless of the surrounding UI. This is deliberate: the terminal
is the content, the chrome is the frame.

**Contrast.** Primary text on app background clears WCAG AA for body text; muted
text (`#777c8b`) is for non-essential meta only and should never carry meaning a
user must read to act. Status colors are paired with their `*-bg` tints so a
colored chip never relies on hue alone — shape and label carry the meaning too.

## Typography

**Two families, two jobs.** Sans for chrome, mono for anything machine-generated.

- **Sans — Inter.** Chrome text: sidebar rows, buttons, dialogs, labels. Inter is
  a deliberate, documented choice, not a default: it is exceptionally legible at
  the 11–14px sizes this UI lives in, has the tabular figures the session/diff
  counters need, and disappears into the tool the way a workhorse UI face should.
  It is loaded from the Google Fonts CDN with a `system-ui` fallback stack, so
  the app degrades to the platform sans if the CDN is unreachable.
- **Mono — JetBrains Mono.** The terminal, code, diffs, and every "machine value"
  chip (branch, model, path, host, counts). Mono is the signal that a string is a
  literal value you could copy, not prose.

**Scale (16px root).** Dense by design — the default UI size is **13px**
(`--text-sm`), not 14–16px. The ladder: `2xs` 11px (micro labels), `xs` 12px
(meta/captions), `sm` 13px (UI default, sidebar rows), `md` 14px (body, inputs),
`lg` 16px (section titles), `xl` 20px (dialog headings), `2xl` 26px, `3xl` 36px
(hero display). Mono runs 12/13/14px with the terminal default at 13px.

**Weight is restrained — hierarchy comes from role, not heft.** Four weights:
400 regular, 500 medium, 600 semibold, 700 bold. Body is 400; most UI chrome is
500; titles and headings are 600. There is no routine 700 in the chrome — bold is
reserved.

**Semantic roles** (use these, not raw size + weight): `display`, `heading`,
`title`, `body`, `ui`, `meta`, `label` (uppercase micro-label, +0.04em tracking),
`terminal`, `code`. Tracking is `0` everywhere except display headings
(`-0.012em`, tightened) and uppercase labels (`+0.04em`, opened up).

## Layout

**Approach: grid-disciplined app shell.** Fixed rails and bars, predictable
alignment, no editorial asymmetry. The shell is sidebar + tab bar + workspace
(terminal/diff/panels), assembled from `.rk-*` shell classes in `app.css`.

**Spacing is a 4px grid** (base unit 4px; the `1` step is a 2px half-step for
hairline nudges). Density is **dense but breathable** — use the semantic density
tokens rather than raw steps where they exist: `gap-row` (4px between sidebar
rows), `pad-row` (6/8px row padding), `pad-control` (4/12px button & input
interior), `pad-panel` (16px), `pad-dialog` (24px).

**Fixed rails and bar heights** (so the shell never reflows under content):
- Sidebar rail: 240px default, 264px medium; drag-resizable between **180–480px**
  (clamped in `ui/use-rail-width.ts`, not via CSS min/max — the 56px collapsed
  state is intentionally below the min).
- Side panel (files / diff / PR peek): 300px.
- Title bar 40px, tab bar 38px, status bar 26px.
- Controls: 30px default height, 24px small, 36px large.

**No max content width** — this is a tool, not a reading column. Surfaces fill
their rail or panel and the terminal takes the rest.

## Elevation & Depth

**Soft depth, never heavy cards.** Raised surfaces read through a 1px hairline
border plus a low-opacity shadow, not a thick drop shadow. Shadows are tuned
twice: deeper and darker for dark surfaces, lighter for the light theme.

The ladder: `shadow-xs` (1px, controls) → `shadow-sm` (rows, raised chips) →
`shadow-md` (menus, dropdowns) → `shadow-lg` (dialogs) → `shadow-popover`
(floating panels). Raised surfaces also get a 1px inset top highlight
(`--ring-inset-top`) so an edge catches light without a visible border.

**Glow is rationed to exactly two uses:**
- `glow-accent` — the focus ring (a 3px accent halo). Every focusable control
  gets it; nothing else does.
- `glow-pulse` — the activity pulse's halo. **Canonical color is marine**
  (`pulse` / `#6ea4ff`): a single hue runs end-to-end from the pulse core
  through the halo and the `remora-pulse-glow` keyframe.

Radii reinforce depth: tighter corners sit closer to the surface, looser corners
float higher — see Shapes.

## Shapes

**Restrained, Mac-quality rounding** that scales with elevation. The more a
surface floats, the rounder it gets:

- `xs` 3px — badges, tags, micro-controls, the count chip.
- `sm` 5px — buttons, inputs, selects, sidebar rows (the workhorse radius).
- `md` 7px — cards, tabs, menus.
- `lg` 10px — panels, popovers.
- `xl` 14px — dialogs and modals (the highest-floating surface).
- `pill` 999px — the active-session accent rail and any fully-rounded affordance.
- Circle (50%) — status dots and the activity pulse core.

Never apply one uniform radius to everything — the hierarchy is the point. A 14px
dialog radius on a 20px-tall tag would read as a toy.

## Components

Components are pure CSS (custom properties + colocated `.css` per component in
`apps/desktop/src/ui/`); there is no Tailwind. Icons come from `lucide-react` at
12–18px depending on context.

**Buttons** (`Button.tsx`). Four variants, three sizes. `primary` is the solid
marine action (one per surface). `secondary` is a bordered panel-fill button.
`ghost` is transparent until hover. `danger` is a bordered/transparent red action.
Sizes: `sm` 24px, `md` 30px (default), `lg` 36px; all weight 500. Focus shows
`glow-accent`; press nudges 0.5px down. A loading spinner disables the button.

**Tag vs Badge — the machine-value rule.** These look similar and are *not*
interchangeable:
- **Tag** (`Tag.tsx`) — a **monospace** chip (20px tall, 12px mono, radius 3px,
  elevated fill, hairline border) for **machine values**: branch names, model
  ids, paths, tokens. The `branch` variant tints text and icon marine. Tags can
  carry a close affordance.
- **Badge** (`Badge.tsx`) — a **sans, uppercase** chip (18px tall, 11px semibold,
  +0.04em tracking, radius 3px) for **status and counts**, with tones
  `neutral / accent / success / warning / danger / info`, a `solid` accent
  variant, a `count` mode (tabular-nums, min-width 18px), and an optional leading
  `dot`. Badges are chrome-only — never inside the terminal.

**Host label vs chip — bare wins.** A session's host is rendered as a **bare,
muted, monospace label** folded into the meta row, *not* a Tag chip. Reserve
chips for values the user acts on (branch, model). Decorating the host with a
chip would over-weight ambient context and compete with the session name. The
transport (ssh / kubectl) rides the host label as a small glyph, not a separate
badge.

**Session row** (`SessionRow.tsx`). The sidebar's core unit. The session **name
is weight 500** (`ui` role) — understated on purpose; the **active state** is
carried by a 2.5px pill accent rail on the left plus an `accent-subtle` fill and
a hairline `accent` border, not by bolding the text. Agent + branch meta sit
below in muted mono. The activity pulse occupies a fixed 7px slot that is
**reserved even when empty** (disconnected rows keep the footprint so the list
never jitters). Reconnecting rows drop to 0.55 opacity with an italic
"reconnecting…" suffix.

> Note the deliberate split: the session **row** name is weight 500, but the
> session **bar** name (the header above the workspace, `.rk-session-bar__name`)
> is weight **600**. The row lives in a dense list where the rail carries
> emphasis; the bar is a singular title that can afford the heavier weight.

**Tabs** (`SessionTab.tsx`). 38px tall, 120–220px wide, horizontal scroll on
overflow. Label is 13px/500, secondary until active. Active tab takes the app-bg
and a 2px bottom accent underline. Drag-to-reorder uses native HTML5 DnD with a
2px accent drop-indicator bar. A 6px dirty dot yields to the close affordance on
hover.

**Inputs & selects** (`Input.tsx`, `Select.tsx`). 30px tall, `bg-input` fill,
`border-default` → `border-strong` on hover, radius 5px. Focus switches the
border to `accent-border` and adds `glow-accent`; invalid switches to `danger`
with a danger halo. Both have a `mono` variant for machine-value entry (paths,
commands). Field labels are 12px/500 secondary; hints/errors are 12px muted/danger.

**Dialogs** (`Dialog.tsx`). Centered over a `bg-overlay` scrim, max-width 460px,
`bg-panel` fill, radius 14px (`xl`), `shadow-lg`. Enters with a quiet
fade + 8px rise + 0.99→1 scale over `dur-base`. Header icon is accent; title is
20px semibold with tight tracking.

**Activity pulse** (`ActivityPulse.tsx`) — the signature component. A core dot
(7/9/11px) with states: `idle` (muted, still), `done` (success, still), `error`
(danger, still), `working` (marine, breathing ring at 1600ms), `needs` (marine,
faster 900ms attention beat with a glow halo). `StatusIndicator` is the
size-locked `sm` wrapper used inline in rows and tabs. The pulse color is marine
in every active state — keep it that way.

## Do's and Don'ts

**Do**
- Read the token CSS / this file before any visual change; reuse a semantic token
  (`bg-panel`, `text-secondary`, `accent`) rather than a raw ramp value or a
  literal hex.
- Use **monospace for machine values** (branch, model, path, host, counts) and
  sans for prose and labels. When in doubt: could the user copy-paste it as a
  literal? Then it's mono.
- Reach for **Tag** (mono) for machine values and **Badge** (sans, uppercase) for
  status/counts. Keep the host a bare muted label.
- Carry emphasis with the **active rail + tint**, spacing, and role — not by
  bolding text. Session-row names stay weight 500.
- Keep motion fast and quiet (120–180ms, `ease-out`, no bounce). Let the activity
  pulse be the only expressive animation.
- Reserve the pulse's footprint even when there's nothing to show, so lists don't
  reflow.

**Don't**
- **Don't introduce a second accent color.** Marine is the only brand hue. Build
  hierarchy with neutrals, weight, and space.
- **Don't let status color into the terminal**, and don't lighten `term-bg` under
  the light theme — the terminal stays dark on purpose.
- **Don't use a heavy card or a thick drop shadow.** Depth is a hairline border
  plus a soft shadow; that's the house style.
- **Don't spend `glow`** outside the focus ring and the activity pulse. A glow
  anywhere else cheapens both.
- **Don't apply one uniform radius.** Rounding scales with elevation
  (3px chips → 14px dialogs).
- **Don't add a competing animation.** A second looping motion next to the pulse
  destroys the one signature moment.
- **Don't reintroduce the lavender glow.** The activity-pulse halo is **marine**
  end to end — `--glow-pulse` and the `remora-pulse-glow` keyframe derive from
  `--marine-pulse` (`#6ea4ff`, deepening to `--marine-500` `#1e6ff5` in light).
  The old `rgba(169, 156, 255, …)` / `rgba(124, 108, 240, …)` lavender values are
  retired (#180); a single hue is what makes the signature moment read.

## Decisions Log

| Date | Decision | Rationale |
|------|----------|-----------|
| 2026-06-29 | Formalize the shipped token system as DESIGN.md in the Google `design.md` format | Token CSS held the values but the *decisions* had no home (issue #150). Front matter mirrors `styles/tokens/*.css`; prose captures the calls surfaced during the session-row review. |
| 2026-06-29 | Canonical activity-pulse color is marine `#6ea4ff` | The pulse is the one signature moment and reads strongest as a single hue end to end. The lavender glow was never a chosen split — it was an inconsistency where the halo drifted off the marine core. Marine is canonical; the lavender values are corrected, not "retired" (fixed in #180). |
| 2026-06-29 | Document Inter as the shipped chrome face | Re-fonting a shipped app is a product decision, not a docs task; Inter's legibility at 11–14px and tabular figures justify keeping it. Recorded so the choice is intentional. |
| 2026-06-29 | Host as a bare muted label; Tag for machine values, Badge for status | Resolves the chip-vs-bare-label question from the session-row review — chips are for values the user acts on, not ambient context. |
