### Fixed
- **Plugin windows open centred on the active monitor:** opening a plugin's own GUI on Omarchy/Hyprland (especially with multiple monitors or scaled displays under XWayland) now centres the window on the focused RandR monitor instead of the combined X11 root screen, so it no longer lands at the side of the display.
