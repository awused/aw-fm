#! /bin/sh

# Opens rofi, selects a single file from the user's home directory, and jumps to it
# Use the command "Script /path/to/rofi-jump-home.sh"

target=$(find "$HOME" -not \( -name ".*" -prune \) -type f | sed "s?${HOME}/??" | rofi -dmenu -i)

[ -n "$target" ] || exit 0

echo JumpTo "$HOME/$target"
