#! /bin/sh

# Install scripts to ~/.config/aw-fm/scripts or wherever you've configured it to look.
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
#
# directories=true
#
## Whether this runs on regular files or not.
## Defaults to true but is subject to mimetype/extension filtering.
#
# files=true
#
## Set of semicolon-separated mimetypes.
## Files can be any type
## If empty, accept all. If set, only accept those mimetypes.
## Wildcards are allowed, like audio/*
#
# mimetypes=
#
## Like mimetypes, but for extensions instead.
## If both mimetypes and extensions are set any file matching either will be accepted.
#
# extensions=
#
## Allowed paths, as a glob.
## This should be absolute.
## All files must pass this glob if set.
#
# glob=
#
## Allowed paths, as a regular expression.
## This should be absolute.
## All files must match this regex if set.
#
# regex=
#
## Whether this can run on multiple files or not.
## This includes directories.
## Only runs if all files pass the above filters.
#
# multiple=true
#
## How to handle sorting actions.
## This can be any integer (32 bit) and lower numbers appear earlier in the list.
## The default priority is 0.
## Priorities below 0 will appear above context menu items defined in aw-fm.toml.
#
# priority=0
#
#**aw-fm-settings-end**


# Example settings for a script that can run on any single directory.

#**aw-fm-settings-begin**
# name=Directories
# files=false
# multiple=false
#**aw-fm-settings-end**

# A script that can run on multiple mp3 files.
# Mimetypes are not always obvious.

#**aw-fm-settings-begin**
# name=Play Music
# directories=false
# mimetypes=audio/mpeg
#**aw-fm-settings-end**

# A script that can run on multiple video files, png images, or files ending in abc or abcd.

#**aw-fm-settings-begin**
# directories=false
# mimetypes=video/*;image/png
# extensions=abc;abcd
#**aw-fm-settings-end**

# A script that can run on anything but only inside in a user's Downloads folder.
# The glob and regular expression should behave the same.
# Shows up in the context menu as "Remove Downloads"
#**aw-fm-settings-begin**
# name=Remove Downloads
# glob=/home/*/Downloads/**
# regex=^/home/[^/]+/Downloads/
#**aw-fm-settings-end**



# Environment variables that are set
# The files are passed in, by absolute paths, as arguments.
#
## The path to the current directory.
## If searching is happening, not all files may be inside this.
#
echo $AWFM_CURRENT_DIR
# echo $AWFM_SELECTION

