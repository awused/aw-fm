#! /usr/bin/env bash

# Opens rofi, selects one or multiple directories inside the current directory.
# For single output it navigates the current tab, for multiple it opens them as background tabs
# in order.
# Use the command "Script /path/to/rofi-open-subdirs.sh"

target=$(find "$AWFM_CURRENT_TAB_PATH" -not \( -name ".*" -prune \) -type d | sed "s?${AWFM_CURRENT_TAB_PATH}/??" | tail -n +2 | rofi -dmenu -multi-select -i)

[ -n "$target" ] || exit 0

lines=$(echo "$target" | wc -l)
if [ "$lines" = "1" ]; then
  echo "Navigate $AWFM_CURRENT_TAB_PATH/$target"
else
  # Reverse the list because new tabs open below the current tab

  while IFS= read -r line; do
    echo "NewBackgroundTab $AWFM_CURRENT_TAB_PATH/$line"
  done < <(echo "$target" | tac)
fi

