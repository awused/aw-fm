# AW-FM

Awused's personal gui file manager.

It is a simple file manager designed to be fast and efficient at doing what I
actually do.

## Features

* Fast and efficient
  * Opening directories containing hundreds of thousands of images shouldn't
      lock up the UI for minutes/hours.
* Natural sorting, `abc` sorts before `XYZ` and `a2.png` sorts before `a10.png`.
* Highly customizable, up to a point.
  * A reasonably complete set of text commands to control the application.
  * Define custom shortcuts, custom bookmarks, and custom context menu actions.
* A UI charitably described as minimal.
  * Panes/splits work out of the box, but are maybe _too_ flexible.
* Custom actions showing up in context menus.
  * Just flat scripts, easy to write and back up.
* Not much more, anything I don't personally use doesn't get implemented.
  * Will not cover every use case, like mounting external drives.

## Installation and Usage

Clone the repository and run `make install` to install aw-fm and the extra files
in the [desktop](desktop) directory to their default locations. Alternately run
`cargo install --git https://github.com/awused/aw-fm --locked` and install those
extra files manually.

Run with `aw-fm` or your application launcher of choice.

Optionally edit the config in [aw-fm.toml.sample](aw-fm.toml.sample) and copy it
to `~/.config/aw-fm/aw-fm.toml`.

### Dependencies

Required:

* GTK - GTK4 libraries and development headers must be installed.
* gnome-desktop utility libraries

On fedora all required dependencies can be installed with
`dnf install gtk4-devel gnome-desktop4-devel`.

## Shortcuts

### Defaults

The defaults should make some level of sense. Hit `?` for a popup containing all
current customizable keybinds.

Mouse controls are not customizable but should work as expected. Middle clicking
on a file or directory is the same as the `NewBackgroundTab` command below. Control
clicking on a tab will open it in a horizontal split, shift clicking leads to a vertical
split.

### Customization

#### Custom Actions

Custom actions are enabled by scripts in the actions directory, default
`$HOME/.config/aw-fm/actions/`. Depending on how they are configured
they do not always appear in the context menu.

They must be executable text files and options are read from within the file.
See the [example script](examples/sample.sh) for an explanation of the
options.

Custom actions behave as if run by `Script`: any output will be treated as a
newline-separated series of commands to run.

#### Commands

Keyboard shortcuts, bookmarks, and context menu entries can be customized in
[aw-fm.toml](aw-fm.toml.sample). See the comments in the config file for how to
specify them.

* `Help`
  * List current keybinds.
* `Quit`
* `Refresh`
  * Refreshes all tabs.
  * This shouldn't be necessary unless file system notifies are incomplete,
    like over NFS.
* `Activate`
  * The same as hitting enter or double clicking on the selected file(s).
  * This will run executable files or open non-executable files in their default
    applications.
  * It is not recommended to bind this as a shortcut.
* `OpenDefault`
  * Opens the selected files in their default applications.
  * Will _not_ run executable files.
* `OpenWith`
  * Spawns a fairly standard "Open With" dialog to select the application.
  * Allows changing the default application. <!-- or defining new applications. -->
* `Cut`/`Copy`
  * Cuts or copies the current selection.
  * Will set the clipboard even if nothing is selected.
* `Paste` -- partially implemented
  * Pastes into the active tab.
  * Can receive cuts and copies from aw-fm, caja, or nautilus.
  * Using this in scripts would be odd.
* `Trash`
  * Moves the selected items to trash.
  * aw-fm doesn't have utilities to manage trash.
* `Delete`
  * Spawns a confirmation dialog before permanently deleting the selected items.
* `Rename`
  * Spawns a rename dialog for the current file.

##### Navigation Commands

* `Navigate directory/file`
  * Navigates the current tab to a directory or jumps to a file in that directory.
  * Opens a new tab if one isn't active.active
* `Home`
  * Navigates to the user's home directory.
* `JumpTo path`
  * Jumps to the parent directory of `path` and scrolls so that `path` is visible.
  * Opens a new tab if one isn't active.
* `Forward` and `Back`
  * Navigates through the history of the active tab.
* `Parent`
  * Navigates to the parent of the current directory.
* `Child`
  * Navigates into a child directory of the current directory if there is only
  one or if you previously navigated from a subdirectory of the current directory.
  * `Parent` followed by `Child` will return you to the same directory.
* `Search [query]`
  * Opens a recursive search in the current directory.
  * Searching requires at least three characters and uses a simple substring match.
  * For more powerful/flexible searching, use an external program like rofi or fzf.

##### Tabs

* `NewTab [directory|file]` and `NewBackgroundTab [directory/file]`
  * Opens a new tab in the foreground or background.
  * If directory or file is set, it will behave like `Navigate`.
  * By default it will clone the current tab or the user's home directory.
  * Examples: `Navigate /path/to/directory` `Navigate /path/to/file.png`
* `CloseTab`, `ClosePane`, `CloseActive`
  * Close the active tab, pane, or both.
  * All panes can be closed without closing all tabs.
* `ReopenTab`
  * Reopens the last closed tabs in reverse order.
* `Split horizontal|vertical`
  * Splits the current tab in two.
  * The new tab is on the right or bottom of the split.
  * If no tabs are visible, opens a new on.
* `SaveSession name`, `LoadSession name`, and `DeleteSession name`
  * Saves, loads, or deletes the current session.
  * Only currently saves the list of open tabs.

##### Display Settings

* `Display icons|columns`
  * Changes the display mode of the current directory.
* `SortBy name|mtime|size`
* `SortDir ascending|descending`
  * Change how the current directory is sorted.

##### Other

* `Execute`
  * Requires a single string argument which will be run as an executable.
  * Example: `Execute /path/to/some-program`
* `Script`
  * Like Execute but reads stdout from the executable as a series of commands to
    run, one per line.
  * These programs will be killed on exit from aw-fm.
  * Example: `Script /path/to/sample-script.sh` if the script prints "Quit" the
    program will exit.

### External Executable Environment

The executables from `Execute`, `Script`, and custom actions will be called
with no arguments and several environment variables set.
[rofi-jump-home.sh](examples/rofi-jump-home.sh) is an example that opens rofi
to navigate to a directory inside the user's home directory.

All of these variables may be empty or absent.

Environment Variable | Explanation
-------------------- | ----------
`AWFM_CURRENT_TAB_PATH` | The currently selected tab, which is also the current pane.
`AWFM_CURRENT_TAB_SEARCH` | The currently selected tab's search.
`AWFM_SELECTION` | A newline-separated list of selected files in the current displayed sort order. Scripts that run against directories may need to check both `AWFM_SELECTION` and `AWFM_CURRENT_TAB_PATH` to decide what to operate on.
`AWFM_NEXT_TAB_PATH` | The next(lower) tab as visually seen in the tabs list on the left. If tabs are open but no panes are open, this will be the first tab.
`AWFM_NEXT_TAB_SEARCH` | See above.
`AWFM_PREV_TAB_PATH` | The previous(higher) tab as visually seen in the tabs list on the left. If tabs are open but no panes are open, this will be absent.
`AWFM_PREV_TAB_SEARCH` | See above.

<!-- AWFM_NEXT_PANE | The tab open in the "next" pane. Pane ordering is based on how they were opened as a tree, with left/top tabs coming before right/bottoms tabs. May be empty. -->
<!-- AWFM_PREV_PANE | The tab open in the "previous" pane. Pane ordering is based on how they were opened as a tree, with left/top tabs coming before right/bottoms tabs. May be empty. -->
<!-- AWFM_PID | The PID of the aw-fm process. -->
<!-- AWFM_SOCKET | The socket used for IPC, if enabled. -->
<!-- AWFM_WINDOW | The window ID for the primary window. Currently only on X11. -->

## Building on Windows

Not planned, good luck. Probably won't work even if the trivial things like
unix-only imports are fixed.

## Development

* `RUST_LOG=Trace` for spam
* `GTK_DEBUG=Interactive`
* `G_MESSAGES_DEBUG=GnomeDesktop` for thumbnailer issues or `G_MESSAGES_DEBUG=All`

## Why

Gui file managers on Linux are almost all descended from Nautilus and have
similar characteristics including performance traps and a lack of
customization. They do, however, support things I probably won't,
like automount and udev.

