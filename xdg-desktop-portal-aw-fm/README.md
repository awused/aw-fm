# AW-FM File Picker

This is an implementation of xdk-desktop-portal that provides the file chooser interface.

This is not the most feature complete chooser, but it is fast and minimal.

## Installation and Usage

Install with
`cargo install --git https://github.com/awused/aw-fm xdg-desktop-portal-aw-fm --locked`

Follow the instructions in the [the portal file](../desktop/aw-fm.portal) to make
`xdg-desktop-portal` know it is available and preferred.

Ensure `xdg-desktop-portal-aw-fm` runs at session startup. This is left as an
exercise for the reader. There are no current plans to make a systemd service available.

After restarting `xdg-desktop-portal`, and potentially other running portals,
it should replace your file picker.
