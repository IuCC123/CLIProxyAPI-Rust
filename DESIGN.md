---
name: CLIProxyAPI-Rust dashboard
description: Pure-black OLED operator panel for a local AI API proxy.
colors:
  bg: "#000000"
  raise: "#0a0a0a"
  raise-hover: "#111111"
  line: "#1a1a1a"
  line-strong: "#2a2a2a"
  line-hover: "#3a3a3f"
  bar-idle: "#3f3f46"
  meter-track: "#232327"
  switch-on: "#e4e4e7"
  lit: "#ffffff"
  fg: "#f4f4f5"
  fg-2: "#a1a1aa"
  fg-3: "#7c7c85"
  ok: "#4ade80"
  warn: "#fbbf24"
  err: "#fb7185"
  brand: "#e11d48"
typography:
  body:
    fontFamily: "ui-sans-serif, -apple-system, BlinkMacSystemFont, Segoe UI, system-ui, sans-serif"
    fontSize: "14px"
    fontWeight: 400
    lineHeight: 1.5
  title:
    fontFamily: "ui-sans-serif, -apple-system, BlinkMacSystemFont, Segoe UI, system-ui, sans-serif"
    fontSize: "22px"
    fontWeight: 600
    lineHeight: 1.2
    letterSpacing: "-0.015em"
  section:
    fontFamily: "ui-sans-serif, -apple-system, BlinkMacSystemFont, Segoe UI, system-ui, sans-serif"
    fontSize: "15px"
    fontWeight: 600
    lineHeight: 1.3
  label:
    fontFamily: "ui-sans-serif, -apple-system, BlinkMacSystemFont, Segoe UI, system-ui, sans-serif"
    fontSize: "12px"
    fontWeight: 500
    lineHeight: 1.4
  figure:
    fontFamily: "ui-sans-serif, -apple-system, BlinkMacSystemFont, Segoe UI, system-ui, sans-serif"
    fontSize: "20px"
    fontWeight: 500
    lineHeight: 1.2
    letterSpacing: "-0.02em"
  figure-compact:
    fontFamily: "ui-sans-serif, -apple-system, BlinkMacSystemFont, Segoe UI, system-ui, sans-serif"
    fontSize: "17px"
    fontWeight: 500
    lineHeight: 1.2
  data:
    fontFamily: "ui-monospace, SF Mono, SFMono-Regular, Menlo, Consolas, monospace"
    fontSize: "12.5px"
    fontWeight: 400
    lineHeight: 1.5
rounded:
  hair: "1.5px"
  bar: "2px"
  meter: "3px"
  xs: "4px"
  tag: "5px"
  sm: "6px"
  seg-inner: "7px"
  md: "8px"
  seg: "9px"
  lg: "12px"
spacing:
  xs: "4px"
  sm: "8px"
  md: "12px"
  lg: "20px"
  xl: "32px"
  xxl: "48px"
components:
  button:
    backgroundColor: "{colors.bg}"
    textColor: "{colors.fg}"
    rounded: "{rounded.md}"
    height: "32px"
    padding: "0 12px"
  button-hover:
    backgroundColor: "{colors.raise-hover}"
  button-primary:
    backgroundColor: "{colors.fg}"
    textColor: "{colors.bg}"
    rounded: "{rounded.md}"
    height: "32px"
    padding: "0 14px"
  input:
    backgroundColor: "{colors.raise}"
    textColor: "{colors.fg}"
    rounded: "{rounded.md}"
    height: "34px"
    padding: "0 10px"
  code-block:
    backgroundColor: "{colors.raise}"
    textColor: "{colors.fg-2}"
    typography: "{typography.data}"
    rounded: "{rounded.lg}"
    padding: "14px 16px"
---

## Overview

An operator panel that behaves like an always-on display: the screen is black, and only information is lit. Luminance, not color, carries hierarchy (fg → fg-2 → fg-3). Color is reserved for state (ok / warn / err) and for the providers' own logos. Nothing glows. There are no cards: sections are separated by space and single hairlines.

Mode: Operate. Familiar controls, dense tables, tabular numbers, system fonts.

## Colors

- `bg` #000 everywhere; OLED pixels stay off. `raise` (#0a0a0a) only for inputs, code and inline panels.
- Text ramp: `fg` for primary values and titles, `fg-2` for body and secondary values, `fg-3` (5.1:1) for labels and metadata. Never go dimmer than `fg-3` for text.
- State: `ok` ready/success, `warn` cooling down, `err` failures. Used for dots and short status words, never for large fills.
- `brand` rose is the mark only. It is not a text color (4.47:1 on black).
- Providers are identified by their real logos, in their own colors. One-color marks (OpenAI, Grok, xAI, OpenRouter, Ollama, LM Studio, Groq) take `fg`; the generic OpenAI-compatible mark takes `fg-3`.
- Supporting neutrals: `line-hover` for hovered control borders, `meter-track` for the empty part of usage meters, `bar-idle` for traffic bars and disabled dots, `switch-on` for an enabled switch track, and `lit` (#fff) only for the instant a new row lights up and for the hovered primary button.

## Typography

One system sans family for all UI; monospace only for real code and data (endpoints, model ids, keys, config, numbers in logs). All numbers use `font-variant-numeric: tabular-nums`. Section titles are sentence case at 15px/600 with no eyebrows or tracking.

## Layout

Single column, max width 1180px, 32px side padding (16px on mobile). Sticky 56px top bar with tabs and the privacy toggle; a red "Reconnecting" appears beside it only while the live connection is down (the brand mark gives way to the tabs under 480px). More space above a section title (32–40px) than below it (12px).

Overview, top to bottom: a one-line endpoint strip (endpoint, key, model count, and a "Set up a client" disclosure that expands the client snippets; open until the first request, then remembered), traffic, accounts at full width, latest requests. The accounts table gives subscription limits their own columns (5-hour, weekly) so several subscriptions compare at a glance; accounts that report limits come first. A Used / Remaining switch beside the section title flips every meter between the share used and the share left (remembered per browser, synced across tabs). On phones each row stacks: name and status, then the two meters side by side with inline labels ("5h used", "Week left").

Config: a section list on the left (Server, Access, Routing, Connections, Providers, Models, Notifications, Diagnostics, YAML file) beside one form at a time, with Save and Discard in a footer shared by every section. Fields sit in a two-column grid that collapses to one on phones; settings that need a restart carry a "Needs restart" note. YAML file is the whole config.yaml in the monospace editor, for anything the forms don't cover; form drafts and file drafts never stack.

## Elevation & Depth

Flat. Depth comes only from `raise` surfaces and 1px `line` hairlines. No shadows on the page; the floating elements (copy feedback, the reset panel dialog over a dimmed `#000b` backdrop) use a soft offset shadow.

## Shapes

8px radius for controls, 12px for code blocks and inline panels, full round for status dots (6px). Small radii exist only where the element is small: 2px/1.5px traffic bars, 4px focus ring and skeleton lines, 5px tags, 9px/7px segmented control and its buttons.

## Components

- Buttons: outline (line-strong border) by default; one white-filled primary per view at most; ghost buttons for row actions.
- Tables: 12px fg-3 headers, 10px cell padding, 1px line separators, row hover `#070707`. The request table's account cell has a second `fg-3` line: why the account was chosen ("Same session", amber for "Moved: quota used up" or "Detour: account busy") and the session's 8-character fingerprint, which filters the table to that session.
- Status: dot + word ("Ready", "Cooling 4:12", "Disabled", "Error").
- Provider logos: an inline SVG sprite (`ui/logos.svg`, from LobeHub Icons, MIT) used through `<use>`; 18px beside account names, 14px in routes, buttons and the segmented control, 20px in the sign-in picker. OpenAI-compatible groups get their vendor's logo when the group name gives it away (OpenRouter, Ollama, LM Studio, DeepSeek, Groq, Mistral, Qwen, Kimi), otherwise the generic mark. xAI API keys show the xAI mark; Grok sign-ins show Grok.
- Privacy toggle: a 30px ghost icon button (eye / eye-off) at the right end of the bar, pressed state on `#18181b`, remembered per browser. When on, emails read `••••••@••••••`, key ends `••••…••••`, the client key `••••••••`, sign-ins without an email are hidden whole, home folders read `~`, and the YAML file section waits behind "Show file"; secret fields in the settings forms stay masked. Copy buttons still copy the real value.
- Usage meters: 6px track (`meter-track`, 3px radius) with the shown share (used or left) in `fg-2`; `warn` once a window is 75% used and `err` from 95%, in either mode, and a used-up window keeps a 1px `err` outline. Whole percentages in `fg` with tabular numbers to the right ("<1%" and ">99%" at the ends), "used" or "left" in the column heading, and "Resets in 2h 14m" in `fg-3` underneath. Meters for one window share a column; accounts without subscription limits leave the columns empty, and subscriptions that haven't reported yet show a dash.
- Banked reset badge (only with `banked-resets` on): a 22px outline tag beside the account name (`line-strong` border, 6px radius, `fg-2`, refresh icon, "2 resets"), amber when an earlier request needs review, with a 5px amber dot when a reset expires within a day. It opens a native dialog panel (`raise`, 12px radius) listing grants, with Refresh and a single primary "Use 1 reset" that leads to a separate confirmation.
- Notifications: destination forms use the existing Config grid, switches and hairline separators. Credential entry is opt-in and write-only, with an HTTPS dashboard-origin confirmation; its password fields clear after submission. Delivery activity uses the existing table style, newest first, with destination filtering and account names covered by the privacy toggle. Credentials and remote response bodies never appear in the activity table.
- Live rows: a new request row lights up at full white and settles to its resting luminance over 1.8s (the one authored motion; disabled for reduced motion).

## Do's and Don'ts

- Do keep the background pure #000.
- Do show time-to-recovery for cooling accounts.
- Don't use glows, gradients, glass, or colored side borders.
- Don't use monospace for labels or headings.
- Don't put more than one primary (white) button in a view.
