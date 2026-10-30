# UI audit and redesign, 4.0.2 → unreleased

A harsh pass over every screen of the unified window, the Sharing page
and the stream overlay, as rendered by the snapshot harness and as read
in code. Severity is the effect on someone using Latch, not the effort
to fix. Every item was found on 4.0.2 (`f96e1a3`); the last column is
its state on this branch.

Renders: `cargo test -p latch-client -p latch-host snapshots -- --ignored`
writes `target/ui-snapshots/`. Names below refer to that folder
(`client-lobby-1280x800@2x.png` and so on).

## Findings

### Structure and flows

| # | Severity | Finding | State |
|---|----------|---------|-------|
| A1 | Critical | The unified window never drew the host's panel. `NodeApp` used `HostApp` only for its state; the panel with this machine's Tailscale, engine, network, Wake-on-LAN and controller status, the paired devices and Unpair, the service log, Open log folder, Stop service and Repair was reachable only from snapshot tests. On a real install none of it could be seen. | Fixed: the **Sharing** tab is that page, drawn by the host crate inside the window (`SharePage`). |
| A2 | High | The snapshot harness loaded the real `client.toml` and `host.toml`, so renders showed the developer's own settings (20 Mbps, 1080p in the 4.0.2 set) and a test that clicked a switch would have written them. | Fixed: headless constructors with default settings that never save; host actions are inert when headless. |
| A3 | High | The six relay-state renders were byte-identical: the Relay card sat below the bottom of a 1100-point render, so the harness showed none of the states it named. | Fixed: relay states are asserted by query at the minimum size, and `*-full.png` renders whole pages. |
| A4 | High | `client-stream-toolbar.png` never showed the toolbar (it is hidden until the host key), the tests that did draw it were named so the `snapshots` filter skipped them, and two tests wrote the same file. | Fixed: `stream-*` renders cover the toolbar at every size and each menu, panel and overlay. |
| A5 | Medium | Renders existed at one or two sizes, mostly 2×, with three machines. No many-machine list, long names, empty, looking, offline, Tailscale-stopped, updating, pairing-stuck, confirm or session-ended states. | Fixed: every state at 640×420, 1280×800 and 1920×1200, at 1× and 2×, plus unignored fit tests at the minimum and large sizes. |
| A6 | Medium | Settings was a toggle ("Settings" / "Close settings") that replaced the list; there was no navigation, and no way to reach settings from the keyboard. | Fixed: tabs (Machines, Sharing, Settings) with Command/Ctrl-1, 2, comma and Escape. |
| A7 | Medium | "This machine" in the list offered only Share or Repair; the paired devices, options and diagnostics lived in a window no one saw (A1), and the three sharing switches were in the viewer's Settings, away from the thing they configure. | Fixed: the row summarises and opens Sharing; the switches live there. |

### Performance and threads

| # | Severity | Finding | State |
|---|----------|---------|-------|
| P1 | High | The idle window repainted every 400 ms forever (`request_repaint_after(400ms)` on every frame), and the host poller asked for a repaint every second whether anything changed or not. | Fixed: nothing repaints on a timer when idle (tested: `an_idle_window_asks_for_no_repaint`); the poller repaints on change. |
| P2 | High | Every two seconds the UI thread called `tailscale::cli()`, which, when Tailscale is not installed, spawns `tailscale --version` and waits for it. | Fixed: probed on its own thread. |
| P3 | Medium | Unpair, Install controller driver and the setup request (a 600 ms HTTP probe plus a process spawn) ran on the UI thread. | Fixed: worker threads, with the result shown as a notice. |
| P4 | Low | `NodeApp` cloned the host's whole status, service log included, into the viewer every frame, stream frames included. | Fixed: copied only when it changes (`Shared::rev`). |
| P5 | — | Stream view per-frame cost. | Unchanged: the toolbar, menus and panels are laid out only while shown; notices ask for a 500 ms repaint only while up. |

### Visual design

| # | Severity | Finding | State |
|---|----------|---------|-------|
| V1 | High | No design system: one palette struct, but sizes chosen inline (13.5, 12.5, 17, 28, 30, 40 pt), screen-local `semibold(28.0)` headlines, an inline `egui::Grid` for stats, inline colours. | Fixed: tokens in `theme.rs` (below); screens use only tokens and components. |
| V2 | High | The app did not look like the product site: violet, pink and cyan chrome in Inter against the original product site's black, white, hairlines and Geist. | Fixed: Geist and Geist Mono, neutral surfaces, white primary, colour only for status. |
| V3 | Medium | A marketing headline inside the app ("Your workspace, anywhere.") at 30 pt, pushing the machine list below the fold at 640×420. | Fixed: a plain page title. |
| V4 | Medium | Status pills used as data chips ("1920 × 1080" in cyan, "60 fps" in grey): colour that meant nothing. | Fixed: mono tags; colour only on status. |
| V5 | Medium | Three filled violet Connect buttons in a three-row list: the "primary" colour repeated until it said nothing. | Fixed: outlined Connect; one white primary per screen at most. |
| V6 | Medium | Four control styles side by side: egui's framed menu buttons ("PC ⌄"), frameless ghost buttons, egui's filled-triangle dropdowns, egui's default collapsing triangle, and "●/○" glyphs as radio buttons in menus. | Fixed: one button family, painted chevrons, check marks, popup menus. |
| V7 | Medium | Notices glued the dot to the text ("●Gaming-PC's…") and wrapped the second line under the dot. | Fixed: a fixed gutter; every line starts at the same x. |
| V8 | Medium | Host setup card: the caption shared a wrapping row with the button and wrapped under it; checklist lines 14 pt apart. | Fixed. |
| V9 | Low | The stream toolbar was a rectangle with a white hairline on all four sides. | Fixed: bottom hairline only. |
| V10 | Low | Bitrate slider: large hollow knob, violet trail, a drag box beside it. | Fixed: a slim slider with a mono value, arrow-key steps. |
| V11 | Low | The App field was 20 pt tall beside 30-pt controls. | Fixed. |
| V12 | Low | A notice's quiet action ("Dismiss", "Cancel") sat 12 points right of the text above it: its label is inset by the button's padding, and no fill or outline shows the box. A notice with no actions kept an empty row's height; the setup card's "Get Tailscale" link started left of the item it belongs to. | Fixed: `action_row` pulls a leading quiet button back by its padding; an empty row takes no room; `dot_item` puts the link in the item's text column. |

### Contrast (WCAG 2.2 AA, computed)

| # | Severity | Finding | State |
|---|----------|---------|-------|
| C1 | High | Faint text `#5c6676` (footer, small print, stream status line): 3.45:1 on the window, 3.22:1 on cards, 3.62:1 on black. Fails 4.5:1. | Fixed: tertiary `#8c8c8c`, ≥ 4.5:1 on every surface up to a hovered control. |
| C2 | High | Destructive confirm button: `#e8ecf2` on `#f85149`, 2.83:1. | Fixed: white on `#cd2b31`, 5.27:1. |
| C3 | Medium | Violet pill text on its own tint ("Connecting", "Needs setup"): 3.89:1. | Fixed: status text is secondary grey beside a coloured dot. |
| C4 | Low | Violet accent as text at 4.51:1 on cards: passing by 0.01. | Fixed: the accent is `#a78bfa` (6.9:1) and is never text on a fill. |

All pairs are asserted in `crates/ui/src/theme.rs` and `widgets.rs` tests.

### Copy and behaviour

| # | Severity | Finding | State |
|---|----------|---------|-------|
| U1 | High | "This Mac" everywhere, on Windows and Linux viewers too: errors, key-expiry notices, path explanations, settings hints. | Fixed: "this machine", or the right noun per OS. |
| U2 | High | Errors sent people to a UI that no longer exists: "Open Latch Host on the PC and run setup", "Check Wake-on-LAN in Latch Host". | Fixed: they point at the Sharing tab and say what to do. |
| U3 | Medium | "Keep this machine awake while plugged in" on Mac and Linux, where `keep_awake` does nothing. | Fixed: Windows only. |
| U4 | Medium | "Command key acts as Ctrl" on Windows and Linux viewers, which have no Command key. | Fixed: Mac only. |
| U5 | Medium | Connect buttons vanished while any connection ran, reflowing the list. | Fixed: disabled in place, with a tooltip. |
| U6 | Medium | "Wake" beside "Connect", which also wakes. | Fixed: "Wake and connect"; Wake alone is in the row's menu. |
| U7 | Medium | The lobby's power menu was "PC" for every OS and hid Test wake behind power permissions. | Fixed: a "…" menu with what each machine allows, plus Copy Tailscale address. |
| U8 | Medium | While the app updated itself the connection card had an empty title. | Fixed: "Installing a Latch update". |
| U9 | Medium | Every failure offered "Try again at Smooth (1080p · 20 Mbps)", even a wake failure it cannot help, and a connection that never started was titled "… disconnected". | Fixed: "Couldn't connect to …" or "… disconnected"; the lighter profile only for stream failures. |
| U10 | Medium | "Connecting failed at {stage} (code {code})." | Fixed: says the machine answered and points at the network. |
| U11 | Medium | Pairing called the other side "Latch Host"; a stuck PIN hand-off showed as a violet notice. | Fixed: PIN in boxes, plain explanation, amber notice. |
| U12 | Medium | "Tailscale: Tailscale is stopped." beside "Sign in to Tailscale to see your machines" over a list of remembered machines. | Fixed: one banner that says what happened and what to do; the list stays. |
| U13 | Medium | Stream quality was two controls for one choice: four unselectable "quick profile" buttons plus Recommended/Manual, and a Reset. | Fixed: one control, Recommended · Smooth · Balanced · Sharp · Custom. |
| U14 | Low | Settings was one long card; the licence only as raw NOTICE text. | Fixed: grouped sections and an About section. |
| U15 | Low | Relay copy: "That is not a denial." | Fixed: "That does not mean access is denied." |
| U16 | Low | A key expiring in 176 days shown in a bright notice on the main page. | Fixed: folded into Connection details; within 30 days it goes to the top in red. |
| U17 | Medium | Open log folder ran `explorer` on every OS. | Fixed: `open` on macOS, `xdg-open` on Linux. |
| U18 | Low | Setup failure showed the script's raw error. | Fixed: what happened and what to do, raw error in small print. |
| U19 | Medium | No way out of "Connecting to …" in the stream view without knowing the host key. | Fixed: a Cancel button. |
| U20 | Medium | The host key was only a five-second text toast, and read "Ctrl+Alt" on a Mac keyboard labelled control and option. | Fixed: keycaps at stream start and on the toolbar, named per platform. |
| U21 | Low | Mouse menu items were whole sentences with "●" prefixes; Keys had "○ ⌘ acts as Ctrl (now the Windows key)". | Fixed. |
| U22 | Low | Stream performance used egui's title bar and a raw grid; its "Auto · Balanced" label did not match Settings' "Recommended". | Fixed. |
| U23 | Low | Stream settings used egui's title bar; Escape did nothing. | Fixed. |
| U24 | Medium | Black picture: the fix (Turn off HDR) was a quiet button after Restart stream, and the headline kept "Asking the PC why…" after the answer. | Fixed. |
| U25 | Low | A "Next session" card of pills plus a "Configure stream" button repeated the settings summary. | Fixed: one mono line with a Change link. |
| U26 | Low | "Paired Macs", "Start the background service with Windows" on a Mac. | Fixed. |
| U27 | Low | The poor-network toast said "Open Stats" without saying how to reach the toolbar. | Fixed. |
| U28 | Low | The Sharing page's network line said "a Mac … reaches this PC" on a Mac. | Fixed in the page; the service log's own line is unchanged. |
| U29 | Low | Phones on the tailnet were listed as "Latch isn't installed". | Fixed. |
| U30 | Low | A relayed path was red, like a failure. | Fixed: amber, with the explanation in Connection details. |

### Accessibility and input

| # | Severity | Finding | State |
|---|----------|---------|-------|
| X1 | High | Tab order ran backwards through right-aligned controls (egui focuses in creation order; right-to-left layouts create the rightmost first): a machine's menu before its Connect, the toolbar from Disconnect leftwards. | Fixed: `trailing` layout; tested. |
| X2 | Medium | Focus rings drawn inside controls in text colour, and egui's own widgets showed focus only as the pressed style. | Fixed: a ring in the logo's blue outside every custom control and on built-in ones. |
| X3 | Medium | No window shortcuts. | Fixed: Command/Ctrl-1, 2, comma; Escape. |
| X4 | Low | Switch hit target 40×22 (< 24). | Fixed: 36×24. |
| X5 | Low | Pills and other painted text were missing from the accessibility tree. | Fixed: status, tags, keycaps and the PIN report labels. |
| X6 | High | At 640×420 the stream toolbar's More menu ran past the bottom of the window: "Command acts as Ctrl" and the PC items could not be reached, by pointer or keyboard. Stream performance ran off the bottom, and panels sat under the toolbar. Tab could move focus to a control scrolled out of sight. | Fixed: menus open where they fit (upwards when there is more room above) and scroll inside when neither side has room; dropdown lists, panels and questions do the same, below the toolbar; Tab scrolls the focused control into view; focus rings are no longer cut at a window's edge. Tested at 640×420 by rect, with snapshots. |

## Design system

All in `crates/ui/src/theme.rs`; components in `widgets.rs`.

**Colour** (dark only, by design)

| Token | Value | Use |
|-------|-------|-----|
| `bg` | `#0a0a0a` | Window, elevation 0 |
| `surface` | `#111111` | Cards, grouped lists, panels, elevation 1 |
| `raised` / `raised_hover` / `pressed` | `#1a1a1a` / `#242424` / `#2e2e2e` | Controls and their states |
| `well` | `#060606` | Logs, code |
| `overlay` | `#080808` at 92% | Panels over the stream |
| `hairline` / `border` / `border_strong` | `#242424` / `#363636` / `#4a4a4a` | Separators, control outlines, hovered outlines |
| `text` / `text_secondary` / `text_tertiary` / `text_disabled` | `#ededed` / `#a1a1a1` / `#8c8c8c` / `#5e5e5e` | 16.9, 7.7, 5.9:1 on `bg` |
| `primary` (+ hover, pressed) / `on_primary` | `#ededed` / `#0a0a0a` | The one main action; a switch that is on; the chosen segment |
| `accent` | `#a78bfa` | Keyboard focus, selection, progress. Never a fill behind text |
| `success` / `warning` / `danger` | `#4cc38a` / `#f0b249` / `#ff6369` | Ready / needs attention / broken |
| `danger_fill` | `#cd2b31` | Behind white text on a confirming destructive button |

**Type**: Geist and Geist Mono (OFL 1.1). Page title 22 semibold, −0.44
tracking · card title 15 semibold · body 14 regular, controls 14 medium ·
caption 12.5 · data 12.5 mono · section label 11 mono medium, uppercase,
+0.66 tracking (the site's label style) · PIN 32 mono medium.

**Spacing**: 2 · 4 · 8 · 12 · 16 · 24 · 32 · 48. Page gutter 24; content
column 880 (lists) or 720 (forms).

**Sizes**: control 32, compact control 28, minimum hit target 24, top bar
48, toolbar 44, list and settings rows 44 to 60, status dot 8, switch 36×20.

**Radii**: 4 controls and keycaps · 6 cards, lists, notices, menus · 10
panels over the stream.

**Elevation**: 0 window · 1 surface with a hairline, no shadow · popup
(menus, dropdowns) shadow 0/8/24 at 55% black · overlay (stream panels)
shadow 0/12/32 at 63% black.

**Components**: buttons in five kinds (primary, secondary, quiet, danger,
destructive), icon button, link, popup menu with items, choices, labels,
notes and separators, tabs, segmented control, switch, slider, select,
setting row and block, list row, grouped list and section, card and toned
card, notice and banner, key/value grid, disclosure, well and log,
status dot and text, tag, keycap and shortcut, PIN digits, spinner,
brand lockup, top and bottom bars, overlay and panel frames.

## Not done, and not verified

- The stream picture itself: every stream render has no video behind the
  overlay, because the harness has no engine to connect to. How the
  toolbar and notices read over a real, bright desktop is unverified.
- Real windows: font hinting at 1× on Windows, Retina at 2× on a Mac,
  VoiceOver and Narrator reading the accesskit tree, IME, and the
  Command/Ctrl shortcuts with a real keyboard were not tried; the rules
  forbid launching the app here. Windows-only code compiles in CI only.
- The service's own log lines (`service.rs`) still say "the Mac"; they
  are logs, and the file belongs to the service.
- `Live::quality_label` in `session.rs` has no caller now; the stream
  overlay names profiles as Settings does.
- The product site's screenshots show the 4.0.2 window.
- No light theme.
