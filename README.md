# claude-usage

A lightweight GNOME tray indicator for Claude Code quota usage. No GTK, no
Electron, no polling libraries beyond a plain HTTP client — a single small
binary that sits in the panel.

## What it shows

A [StatusNotifierItem](https://www.freedesktop.org/wiki/Specifications/StatusNotifierItem/)
in the top panel with two parts:

- **A text label** next to the icon (e.g. `17% · 18%`), showing the current
  5-hour session usage and 7-day weekly usage. The template is configurable
  — see [Configuration](#configuration).
- **A drawn ring icon**, an ARGB arc that fills clockwise with the weekly
  usage percentage. It uses no image assets; every pixel is computed at
  runtime.

Clicking the icon opens a menu with the session and weekly percentages
(plus per-category breakdown rows when the endpoint reports them), the
active Claude Code project and its token counts, and a theoretical cost
estimate for the current session. Desktop notifications fire when a
configured usage threshold is crossed or a quota window resets.

A screenshot is pending — this was built and verified headlessly (over
D-Bus introspection) and nobody has looked at the actual panel yet.

## ⚠ The quota endpoint is undocumented and unsupported

`claude-usage` polls `https://api.anthropic.com/api/oauth/usage`, an
endpoint used internally by Claude Code that Anthropic has not published or
committed to. It is not part of any supported API. **It can change shape or
disappear without notice**, at which point this tool may show stale, wrong,
or no data until updated. Treat it as a convenience, not a guarantee.

The response parsing is intentionally tolerant of unknown or missing
fields — an unrecognized shape degrades to showing nothing meaningful
rather than crashing the daemon — but it cannot protect against Anthropic
removing or renaming the fields this tool actually reads.

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
| `[bar].format` | `"{session}% · {weekly}%"` | Placeholders: `{session}`, `{weekly}`, `{reset}`, `{cost}`, `{project}`, `{model}`, `{tokens}` |
| `[bar].rules` | `[]` | Array of `{metric, above, format?, color?, effect?}`; first match (top to bottom) wins |
| `[bar].rules[].effect` | `"pulse"` (opt-in per rule) | **Works** — flips the tray status to `NeedsAttention`; see note below |
| `[threshold].levels` | `[75, 90, 95]` | Percentages that trigger a notification |
| `[threshold].hysteresis` | `2.0` | Points a level must drop below itself to rearm |
| `[threshold].notify` | `true` | Works |
| `[reset].notify` | `true` | Works |
| `[reset].effect` | `"pulse"` | **Inert** — not the same key as `[bar].rules[].effect` above; see [Not yet implemented](#not-yet-implemented) |
| `[reset].duration` | `2` | **Inert** — same |

There are two unrelated `effect` keys in this config, and only one of them
does anything:

- `[bar].rules[].effect = "pulse"` **works today**. A matching rule sets the
  StatusNotifierItem's status to `NeedsAttention` instead of `Active` (see
  `src/render.rs` and `src/tray.rs::status()`). Whether the host renders
  that any differently — some panels flash or highlight a `NeedsAttention`
  icon, most don't — is up to the host, not this program. It is still a
  real, wired effect, just not a custom animation.
- `[reset].effect` and `[reset].duration` **do nothing**. See
  [Not yet implemented](#not-yet-implemented).

## Measured footprint

RSS measured on the release binary, idle, polling every 10 seconds: **6604
KB** (budget: under 10240 KB). `ldd` on the release binary shows only
`libc`, `libm`, `libgcc_s`, and the dynamic linker — no GTK, no GLib, no
libdbus.

## Not yet implemented

- **A frame-by-frame pulse animation on window reset does not exist.**
  `[reset].effect` and `[reset].duration` are parsed and stored, but nothing
  in the daemon reads either field — the code that would have drawn an
  animated icon (`icon::pulse_frames`) was removed as dead code during
  development because nothing ever called it. A window reset today only
  produces the notification configured by `[reset].notify`; there is no
  accompanying visual change, animated or otherwise.
  - This is unrelated to `[bar].rules[].effect = "pulse"` (see
    [Configuration](#configuration)), which does work — it changes the tray
    item's status, just not by drawing an animation.
- The two `[reset]` keys are kept in the config schema so a config file
  written against a future version that does implement this will not need
  to change; for now, setting them does nothing.
