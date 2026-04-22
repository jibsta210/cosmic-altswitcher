# cosmic-altswitcher

A Windows 11-style thumbnail alt-tab switcher for the [COSMIC desktop](https://system76.com/cosmic).

Replaces the default cosmic-launcher-based alt-tab dropdown with a horizontal strip of live window thumbnails. Hold Alt, Tab through windows, release Alt to switch.

## Features

- **Live window thumbnails** via the `ext-image-copy-capture` Wayland protocol
- **Horizontal carousel layout** — selected window centered and scaled up, neighbors fade with distance
- **Keyboard navigation** — Tab / Shift+Tab / arrow keys to cycle, Enter or Alt release to activate, Escape to cancel
- **Click-to-activate** — mouse click on any thumbnail switches to that window
- **Fade in/out** — 250ms smooth transitions
- **App icon badge** overlaid on the thumbnail (bottom-left) for easy identification
- **Single-instance via D-Bus** — subsequent Alt+Tab presses cycle the existing switcher rather than launching new instances

## Status

**Early prototype.** Works end-to-end on stock COSMIC. Known limitations:
- No MRU (most-recently-used) ordering yet — windows appear in toplevel-list order
- Captures are refreshed on each Alt+Tab invocation; during a hold, they don't update live

## Build

```bash
cargo build --release
```

## Install

```bash
sudo cp target/release/cosmic-altswitcher /usr/local/bin/
sudo cp data/com.github.jibsta210.CosmicAltSwitcher.desktop /usr/share/applications/

# Redirect the COSMIC WindowSwitcher system action to this binary
mkdir -p ~/.config/cosmic/com.system76.CosmicSettings.Shortcuts/v1
# Back up first if you care:
# cp /usr/share/cosmic/com.system76.CosmicSettings.Shortcuts/v1/system_actions ~/.config/cosmic/com.system76.CosmicSettings.Shortcuts/v1/system_actions.bak

# Edit ~/.config/cosmic/com.system76.CosmicSettings.Shortcuts/v1/system_actions and change:
#   WindowSwitcher: "cosmic-launcher alt-tab"
# to:
#   WindowSwitcher: "cosmic-altswitcher alt-tab"
# (and WindowSwitcherPrevious similarly)
```

## Uninstall

```bash
sudo rm /usr/local/bin/cosmic-altswitcher
sudo rm /usr/share/applications/com.github.jibsta210.CosmicAltSwitcher.desktop
# Restore ~/.config/cosmic/com.system76.CosmicSettings.Shortcuts/v1/system_actions
```

## License

GPL-3.0-only
