# AW-FM

Awused's personal gui file manager.

It is a simple file manager designed to be fast and efficient.

## Features

* Fast and efficient
  * Opening directories containing hundreds of thousands of images shouldn't
      lock up the UI for minutes/hours.
  * In the default configuration, it uses a tiny fraction of the memory of
      other GUI file managers in large directories with many thumbnails.
  * Cloning tabs is instant.
* Natural sorting, `abc` sorts before `XYZ` and `a2.png` sorts before `a10.png`.
* A UI charitably described as minimal.
  * Panes/splits and tab groups function like minimal workspaces.
* Custom actions showing up in context menus.
  * Just flat scripts, easy to write and back up.
* Seeking without requiring a full search.
* Shell-like completion - typing `/p/t/f` can complete `/path/to/file`.
* Session saving and loading.
* Highly customizable, up to a point.
  * A reasonably complete set of text commands to control the application.
  * Define custom shortcuts, custom bookmarks, and custom context menu actions.
  * Custom context menu actions come with powerful filtering to control when
      they're visible.
* Not much more, anything I don't personally use doesn't get implemented.

## Installation and Usage

Clone the repository and run `make install` to install aw-fm and the extra files
in the [desktop](desktop) directory to their default locations. Alternately run
`cargo install --git https://github.com/awused/aw-fm --locked` and install those
extra files manually, or skip them.

Run with `aw-fm` or your application launcher of choice.

Optionally edit the config in [aw-fm.toml.sample](aw-fm.toml.sample) and copy it
to `~/.config/aw-fm/aw-fm.toml`.

### Setting as the default file manager

If you've copied the desktop files, use
`xdg-mime default aw-fm-folder.desktop inode/directory` to update your default
handler. Then, if you run into other file managers auto-starting, follow the
instructions in
[the dbus service file](desktop/org.aw-fm.freedesktop.FileManager1.service)
to disable other dbus file managers.

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

Mouse controls are somewhat customizable but the defaults should work as expected.
Middle clicking on a file or directory is the same as the `NewBackgroundTab`
command below. Control clicking on a tab will open it in a horizontal split, shift
clicking leads to a vertical split.

Seeking can be done by typing some alphanumeric characters and hitting tab or shift-tab.

Completion can be triggered with `ctrl+space` in the location bar. Currently this
is hardcoded. `ctrl+space` and `ctrl+shift+space` will cycle through matching paths.

### Customization

#### Custom Actions

Custom actions are enabled by scripts in the actions directory, default
`$HOME/.config/aw-fm/actions/`. Depending on how they are configured
they do not always appear in the context menu and can be triggered based on the number
of files, types of files (mimetypes or extensions), and the location of the files.

They must be executable text files and options are read from within the file.
See the [example script](examples/sample-action.sh) for an explanation of the
options.

Custom actions behave as if run by `Script`: any output will be treated as a
newline-separated series of commands to run. See below for more details.

#### Commands

Keyboard shortcuts, bookmarks, and context menu entries can be customized in
[aw-fm.toml](aw-fm.toml.sample). See the comments in the config file for how to
specify them.

* `Help`
  * List current keybinds.
* `Quit`
* `Refresh`/`RefreshAll`
  * Refreshes all visible or all tabs.
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
  * Allows changing the default application.
* `Cut`/`Copy`
  * Cuts or copies the current selection.
  * Will set the clipboard even if nothing is selected.
* `Paste`
  * Pastes into the active tab.
  * Can receive cuts and copies from aw-fm, caja, or nautilus.
  * Using this in scripts would be odd.
* `Trash`
  * Moves the selected items to trash.
  * aw-fm doesn't have utilities to manage trash or restoring files.
* `Delete`
  * Spawns a confirmation dialog before permanently deleting the selected items.
  * As a special case, `Script`s can only run `Delete` on the _currently_ active
    tab and it will fail if the active tab has changed without calling
    `ClearTargetTab`.
* `Rename`
  * Spawns a rename dialog for the current file.
* `Properties`
  * Opens a fairly standard properties dialog for the current selection.
* `FocusLocation`
  * Moves the focus to the location bar and selects the text.
* `Unselect`
  * Unselects everything in the current tab.

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
* `BackOrParent`
  * Equivalent to `Back` if there is history for the current tab, or `Parent` if
  not.
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
* `CloseTab`
  * Close the active tab.
  * Closing the pane of a tab inside a group will remove that tab from the group.
* `CloseTabNoReplacement`
  * Like `CloseTab`, but if the tab was the only visible tab it does not open a
    replacement.
* `ClosePane`
  * Hides the current pane.
  * If that tab was part of a tab group, also removes the tab from that group.
* `HidePanes`
  * Hides all visible panes. Does not remove any tabs from groups.
* `ReopenTab`
  * Reopens the last closed tabs in reverse order.
* `Split horizontal|vertical`
  * Splits the current tab in two, creating or adding to the existing group.
  * The new tab is on the right or bottom of the split.
  * If no tabs are visible, opens a new one.
  * In `Script`s, if the target tab is no longer visible, does nothing.
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
* `Cancel`
  * Cancels all ongoing operations (copies, moves, deletions, etc).
  * Any changes that have already been made or are in flight are not reversed.
  * If multiple operations are ongoing their ordering for `Undo` is not defined.
* `Undo`
  * Undoes the last completed file operation (copy, move, deletion, rename, etc).
  * Undoing is best effort but pessimistic to avoid destroying data.
  * Copies are deleted and moves are reversed. A move will not be undone if a new
    file was created with the original path, but copies that overwrote existing
    files _will_ still be deleted.
  * Not all operations or actions can be undone.
    Deletion is not undoable and trashing is currently not undoable.
* `ClearTargetTab`
  * Changes the target for later commands from whatever the active tab was when
    the script was called to whatever the active tab is currently.
  * Only useful in the context of custom actions or `Script` calls.
* `ReloadActions`
  * Reloads and re-parses custom actions from the configured directory.
  * This is not needed for every update to custom actions, it's only necessary
    when custom actions are added, removed, or their settings are changed.

### External Executable Environment

The executables from `Execute`, `Script`, and custom actions will be called
with no arguments and several environment variables set.
[rofi-jump-home.sh](examples/rofi-jump-home.sh) is an example that opens rofi
to navigate to a subdirectory inside the user's home directory.

All of these variables may be empty or absent. They are not updated in response
to actions taken by the script or the user while the script is running, so they
may become stale. The current working directory will not be set to anything
specific and scripts should use `AWFM_CURRENT_TAB_PATH` when appropriate.

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

By default commands run in the context of tab that was active when they spawn.
This is to prevent surprises if the user switches tabs while a script is running.
Calling `ClearTargetTab` will instead run them in the context of the currently
active tab, even if it changes.

This script will open a new tab and close the previous tab, if any was open,
since CloseTab will run in the context of whatever tab was initially open.

```bash
echo NewTab
echo CloseTab
```

This script will open a new tab and then immediately close the new tab,
leaving whatever tab was open initially untouched.

```bash
echo ClearTargetTab
echo NewTab
echo CloseTab
```

## Building on Windows

Not planned, good luck. Probably won't work even if the trivial things like
unix-only imports are fixed.

## Development

* `RUST_LOG=Trace` for spam
* `GTK_DEBUG=Interactive`
* `G_MESSAGES_DEBUG=GnomeDesktop` for thumbnailer issues or `G_MESSAGES_DEBUG=All`

## Unplanned Features

While I use aw-fm as my only daily driver file manager, there are a few things I
don't plan to implement as I do not use them or use them so rarely it is not worth
implementing them. Aw-fm will not cover every standard file manager feature. If I
only need to open another file manager once or twice a year, that's acceptable.

* Mounting, unmounting, formatting, or otherwise managing drives and file systems
* MTP, WebDAV, FTP, or other protocols
* Browsing inside of archives
  * Nice to have, but isn't worth the complexity given how infrequently I'd use it
* Multiple windows in a single process
  * Aw-fm is very efficient at sharing resources between tabs. This could be
      extended across multiple windows but I just don't use them myself and find
      tabs and splits to be more than enough.

## Why

The major gui file managers on Linux are almost all descended from Nautilus and
have similar characteristics including performance traps and a lack of
customization and features. Even among more niche projectsI wasn't able to find
a file manager that fit my use cases while also being performant, so I made one.

## Screenshots

Panes and tab groups
![Panes](/../screenshots/screenshots/panes.webp)
Support for transparent backgrounds
![Transparency](/../screenshots/screenshots/transparency.jpg)
Media properties for most formats
![Media](/../screenshots/screenshots/media.webp)
