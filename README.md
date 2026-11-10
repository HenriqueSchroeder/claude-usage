# claude-usage

A lightweight GNOME tray indicator for Claude Code quota usage. No GTK, no
Electron, no polling libraries beyond a plain HTTP client — a single small
binary that sits in the panel.

## What it shows

A [StatusNotifierItem](https://www.freedesktop.org/wiki/Specifications/StatusNotifierItem/)
in the top panel with two parts:

- **A text label** next to the icon (e.g. `17% · 18%`), showing the current
  5-hour session usage and 7-day weekly usage. When there is no 5-hour
  window — none running, or a plan without that limit — it shows the weekly
  usage alone (e.g. `35%`). The template is configurable — see
  [Configuration](#configuration).
- **A drawn ring icon**, an ARGB arc that fills clockwise with the weekly
  usage percentage. It uses no image assets; every pixel is computed at
  runtime.

Clicking the icon opens a menu with the session and weekly percentages,
each with the time until that window clears (plus per-category breakdown
rows when the endpoint reports them), and — only while a session is
actually running — the active Claude Code project, how long it has been
going, its token counts and a theoretical cost estimate. When nothing has
happened for half an hour the session block disappears rather than
presenting yesterday's work as today's. `Atualizar agora` forces a reading
without waiting for the next interval, which matters when the endpoint has
asked for a long backoff. Desktop notifications fire when a configured
usage threshold is crossed or a quota window resets.

A screenshot is pending — this was built and verified headlessly (over
D-Bus introspection) and nobody has looked at the actual panel yet.

## ⚠ The quota endpoint is undocumented and unsupported

`claude-usage` polls `https://api.anthropic.com/api/oauth/usage`, an
endpoint used internally by Claude Code that Anthropic has not published or
committed to. It is not part of any supported API. **It can change shape or
disappear without notice**, at which point this tool may show stale, wrong,
or no data until updated. Treat it as a convenience, not a guarantee.

The response parsing is intentionally tolerant of unknown or missing
fields: unknown keys are ignored, and half a known response still reads as
a reading. But a response containing **none** of the fields this tool reads
is treated as a failed poll, not as a reading of 0% — the panel keeps the
last good value, labels it with its age, and backs off. A green `0% · 0%`
would be the one wrong thing to show if Anthropic renames those fields
while your weekly quota is nearly spent.

The same rule applies one level down: if the response carries one window
but drops the other, the missing one is filled in from the last good
reading for that window (not zeroed), and the panel marks the whole
reading as stale until a response with both windows arrives. On the very
first successful poll there is nothing yet to fall back on, so a response
missing a window there is treated the same as a failed poll.

A window sent as an explicit `null` is different from a missing one. The
endpoint answers `"five_hour": null` when there is no 5-hour window — none
running, or a plan without that limit — and that is a reading, not a gap:
the session part leaves the bar and the menu instead of being carried over
or shown as 0%.

## Credentials: read-only, always

`claude-usage` reads the OAuth access token from
`~/.claude/.credentials.json` to call the quota endpoint. **It never writes
to that file.** Claude Code itself refreshes that token in place; a
concurrent write from a background monitor could race that refresh and
corrupt the login. If the token is invalid or missing, the panel shows a
warning state instead of trying to fix it.

## Install

Requires a Linux desktop with a StatusNotifierItem host (GNOME Shell with
the AppIndicator extension, KDE Plasma, etc.) and the Rust toolchain to
build.

```bash
git clone https://github.com/HenriqueSchroeder/claude-usage.git
cd claude-usage
cargo build --release

install -Dm755 target/release/claude-usage ~/.local/bin/claude-usage
install -Dm644 packaging/claude-usage.service \
  ~/.config/systemd/user/claude-usage.service

systemctl --user daemon-reload
systemctl --user enable --now claude-usage
```

Check it came up:

```bash
systemctl --user status claude-usage
```

To use a custom config, copy the example and edit it:

```bash
mkdir -p ~/.config/claude-usage
cp config.example.toml ~/.config/claude-usage/config.toml
```

A missing or broken config file is not a startup error — the daemon falls
back to its built-in defaults and keeps running.

## Configuration

Read from `~/.config/claude-usage/config.toml`. Every key is optional. See
[`config.example.toml`](config.example.toml) for the full file with comments
and every default value spelled out. Summary of the real keys (note these
are flat top-level sections — `[bar]`, `[threshold]`, `[reset]` — not
nested under an `[events]` table):

| Key | Default | Notes |
|---|---|---|
| `poll_seconds` | `60` | Floored to 10 |
| `[bar].format` | `"{auto}"` | Placeholders: `{auto}` (both windows, or the weekly one alone when there is no 5-hour window), `{session}` (`—` when there is no 5-hour window), `{weekly}`, `{session_reset}`, `{weekly_reset}`, `{reset}`, `{cost}`, `{project}`, `{model}`, `{tokens}` — see the note on `{reset}` below |
| `[bar].rules` | `[]` | Array of `{metric, above, format?, color?, effect?}`; first match (top to bottom) wins; `above` is inclusive |
| `[bar].rules[].effect` | `"pulse"` (opt-in per rule) | **Works** — flips the tray status to `NeedsAttention` and plays a brief frame-by-frame pulse the moment the level is crossed; see note below |
| `[threshold].levels` | `[75, 90, 95]` | Percentages that trigger a notification |
| `[threshold].hysteresis` | `2.0` | Points a level must drop below itself to rearm |
| `[threshold].notify` | `true` | Works |
| `[reset].notify` | `true` | Works |
| `[reset].effect` | `"pulse"` | **Works** — `"pulse"` plays a frame-by-frame animation on the icon when a window resets; anything else (e.g. `"none"`) plays nothing |
| `[reset].duration` | `2` | **Works** — how many seconds the reset animation plays; `0` plays nothing, regardless of `effect`. Capped at 30s |

### `{reset}` means the nearest window, not the weekly one

`{reset}` renders the time until **whichever of the two windows clears
first**. The 5-hour window is the nearer one almost always, so a template
like `"⚠ {weekly}% · reset {reset}"` will show `⚠ 96% · reset 2h10m` while
the weekly window is actually four days out — the words say weekly, the
number is the session.

Use `{session_reset}` and `{weekly_reset}` when you mean a specific window;
they never stand in for the other one, and render `—` when the endpoint did
not say. `{reset}` is kept as-is for templates already written against it.
Both windows' reset times are shown in the menu, so whatever the panel says
can be checked against the source.

### Two unrelated `effect` keys

There are two `effect` keys in this config. They are unrelated to each
other, and both work, but they trigger on different moments and one of them
has no `duration` of its own:

- `[bar].rules[].effect = "pulse"` sets the StatusNotifierItem's status to
  `NeedsAttention` instead of `Active` for as long as the matching rule
  stays matched (see `src/render.rs` and `src/tray.rs::status()`) — whether
  the host renders that any differently is up to the host, not this
  program. On top of that, the moment a threshold is actually *crossed*
  (not every cycle it stays crossed), the icon plays a short, fixed,
  2-second frame-by-frame pulse of its own drawing, independent of the host.
  There is no config knob for that duration — rules have no `duration`
  field — so it is a small fixed nudge, not something to tune.
- `[reset].effect` and `[reset].duration` animate the icon, the same way,
  when a quota window resets. `effect` must be exactly `"pulse"` — anything
  else, including the empty string, means no animation — and `duration` is
  in seconds, configurable, capped at 30 and floored at "0 means none".
  Both of these must mean silence, not a one-frame flicker: turning the
  feature off has to look like nothing happening, not a blink.

Both animations are event-driven, not continuous: with no threshold
crossing and no window reset, the daemon never touches the icon between
polls, exactly as before this feature existed.

## Measured footprint

RSS measured on the release binary, idle, with `poll_seconds` lowered to 10
so several session-directory rediscovery cycles land inside the
measurement window: **6816 KB after 5 seconds, unchanged at 45 seconds**
(4+ rediscovery cycles over the real `~/.claude/projects` tree) — call it
**~6.8 MB**, against a 10240 KB budget. `ldd` on the release binary shows
only `libc`, `libm`, `libgcc_s`, and the dynamic linker — no GTK, no GLib,
no libdbus.

## How the pulse animation is drawn

Both `effect = "pulse"` paths (a crossed `[bar].rules[]` entry and a
`[reset]` window rollover) end up in the same place: `icon::pulse_frame`
alpha-scales the same ring `icon::ring` draws every cycle, on a cosine curve
that dims at both ends of the cycle and peaks in the middle.

There is deliberately no dedicated animation thread. The main loop already
sleeps between polls; it spends the first part of that sleep pushing frames
to the tray (one `ksni::Handle::update` call per frame, `FRAME_INTERVAL_MS`
apart) and sleeps the rest as before. That keeps the tray to a single writer
— a second thread on its own timer would race the loop's normal per-cycle
update, and whichever one landed last would decide whether the icon got
stuck mid-pulse.

**A long animation extends the effective poll interval — it does not just
round the edges off it.** A pulse longer than the remaining sleep delays the
next poll by the difference; nothing crashes or hangs (the subtraction that
computes the remaining sleep saturates at zero rather than going negative),
but the delay can be substantial. Worst case: `poll_seconds` at its 10-second
floor with `[reset].duration` at its 30-second cap animates for the full 30
seconds and sleeps for 0, so the next poll lands roughly 30 seconds after
the last one — three times the configured interval, not "a little late."
Anyone running a short `poll_seconds` who also wants long reset animations
should expect polling to slow down to roughly `duration` seconds whenever a
window resets, and lower `duration` if that matters more than the pulse
being long. When the animation ends, the override is cleared and the icon
goes back to exactly the ring implied by the current reading — never a
leftover frame.

## Roadmap

Three things are deliberately not in v1. Listed here so "not built yet" reads
as "not built yet," not "considered and rejected" or "the project is done":

- **Floating PiP overlay** — a GNOME Shell extension showing the ring
  always on top of other windows. Not in the daemon itself because Wayland
  doesn't let an ordinary application declare itself always-on-top:
  `wlr-layer-shell` doesn't exist on GNOME, so a real overlay has to live
  inside the Shell, as an extension, not as this binary.
- **Dock badge** — a numeric badge on the dock icon via the Unity
  `LauncherEntry` D-Bus API. It needs the app pinned to the dock to have an
  icon to badge in the first place, which is why the panel indicator was
  chosen for v1 instead.
- **Usage history** — indexing the full set of past session transcripts for
  trends over time. v1 deliberately reads only the currently active
  session; history would need an offset cache per transcript, rotation
  handling as old ones age out, and a graphical window to show it in, none
  of which exist yet.

No dates attached to any of these — they're documented gaps, not commitments.
