#! /bin/sh

#**aw-fm-settings-begin**
#**aw-fm-settings-end**

# Install custom actions to ~/.config/aw-fm/actions or wherever you've configured it to look.
#
# Aw-fm looks for a block beginning with **aw-fm-settings-begin** near the start of the file
# (first 20 lines) and near the beginning of a line with no whitespace. The configuration block
# ends with **aw-fm-settings-end** near the start of a line.
#
# The block must be present even if it is empty.
#
# The settings control when it is displayed in context menus.
#
# Inside that block, all settings are parsed after the first whitespace character, one per line.
# Unrecognized lines are ignored.
#
# Custom actions SHOULD NOT produce any output to stdout that isn't intended as a command for aw-fm.
# Sink any unwanted command output to /dev/null or an appropriate log file.

# All default settings.
# The default is to just run on anything and everything.
#**aw-fm-settings-begin**
#
## Name, defaults to the name of the script.
#
# name=
#
## Whether this can run on directories or not.
## Defaults to true and is not subject to mimetype/extension filtering.
## true/false
#
# directories=true
#
## Whether this runs on regular files or not.
## Defaults to true but is subject to mimetype/extension filtering.
## true/false
#
# files=true
#
## Set of semicolon-separated mimetypes.
## Files can be any type
## If empty, accept all. If set, only accept those mimetypes.
## Treated as prefixes: audio;video/ will match all audio*/* or video/* mimetypes.
#
# mimetypes=
#
## Like mimetypes, but for extensions instead.
## If both mimetypes and extensions are set any file matching either will be accepted.
#
# extensions=
#
## Allowed paths, as a regular expression.
## All files/directories must match this regex if set.
#
# regex=
#
## Whether this can run on multiple files or not.
## This includes directories.
## Only runs if all files pass the above filters.
## If "any", "at_most_n", or "exactly_0" are specified and no items are selected, the current
## directory will need to pass the above regex and 'directories' must be set
## to true. The script will be called with an empty "AWFM_SELECTION" variable.
##
## 'any' will impose no requirements, and is the default.
## 'exactly_n' will require exactly n items (example: exactly_5 is exactly 5)
## 'at_least_n' will require at most n items (count >= n)
## 'at_most_n' will require at most n items, including zero (count <= n)
## 'n_to_m' will require at least n items and at most m (n <= count <= m)
##
## There are some legacy names that are still supported:
## 'zero' -> 'exactly_0'
## 'one' -> 'exactly_1'
## 'at_least_one' -> 'at_least_1'
## 'multiple' -> 'at_least_2'
## 'maybe_one' -> 'at_most_1'
#
# selection=any
#
## How to handle sorting actions.
## This can be any integer (32 bit) and lower numbers appear earlier in the list.
## The default priority is 0.
## Negative priorities will appear above context menu items defined in aw-fm.toml,
## non-negative values will appear below them.
#
# priority=0
#
## Whether to read stdout of the script as a series of commands to execute.
## Use this to have the script control something in aw-fm.
## true/false
#
# parse_output=false
#
#**aw-fm-settings-end**


# Example settings for a script that can run on any single directory, including the current
# directory. The output will be parsed as commands.

#**aw-fm-settings-begin**
# name=Directories
# files=false
# selection=at_most_1
# parse_output=true
#**aw-fm-settings-end**

# A script that can run on multiple mp3 files.
# Mimetypes are not always obvious.

#**aw-fm-settings-begin**
# name=Play Music
# directories=false
# mimetypes=audio/mpeg
#**aw-fm-settings-end**

# A script that can run on multiple video files, png images, or files ending in abc or abcd.
# Will not show up unless multiple items are selected.

#**aw-fm-settings-begin**
# directories=false
# mimetypes=video/;image/png
# extensions=abc;abcd
# selection=at_least_2
#**aw-fm-settings-end**

# A script that can run on anything inside a user's Downloads folder.
# The glob and regular expression should behave the same.
# Shows up in the context menu as "Remove Downloads"
#**aw-fm-settings-begin**
# name=Remove Downloads
# regex=^/home/[^/]+/Downloads/
#**aw-fm-settings-end**



# Environment variables are set.
# See the "dump-env.sh" script.


# All this action does is navigate to the parent directory or $HOME if this is the root.
[ -n "$AWFM_CURRENT_TAB_PATH" ] || exit 0

dir=$(dirname "$AWFM_CURRENT_TAB_PATH")

if [ "$dir" = "$AWFM_CURRENT_TAB_PATH" ]; then
  echo Home
else
  echo Parent
fi
