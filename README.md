# AW-FM

Awused's personal gui file manager.

It is a simple file manager designed to be fast and efficient at doing what I actually do.

# Features

* Fast
  * Opening directories containing tens of thousands of images shouldn't take hours or lock up the UI.
* Correct gamma and alpha handling during scaling and presentation.
* Wide support for many archive and image formats.
* Proper natural sorting.
* Configurable shortcuts to run external scripts. <!--and a basic IPC interface.-->
* Not much more, anything I don't personally use doesn't get implemented.

# Installation

`cargo install --git https://github.com/awused/aw-fm --locked`

Install and run with aw-man. Optionally edit the defaults in [aw-fm.toml.sample](aw-fm.toml.sample)
and copy it to `~/.config/aw-fm/aw-fm.toml` or `~/.aw-fm.toml`.

<!-- Recommended to install the desktop file in the [desktop](desktop) folder. -->

# Dependencies

Required:

* GTK - GTK4 libraries and development headers must be installed.
* gnome-desktop utility libraries

On fedora all required dependencies can be installed with `dnf install gtk4-devel gnome-desktop4-devel`.

# Usage

# Shortcuts

## Defaults

The defaults should make some level of sense. Hit `?` for a popup containing all customizable
keybinds.

## Customization

Keyboard shortcuts and context menu entries can be customized in
[aw-fm.toml](aw-fm.toml.sample). See the comments in the config file for how to specify them.

Recognized commands:

* `Help`
  * List current keybinds.
* `Quit`

### Navigation

* `Navigate directory/file`
  * Navigates the current tab to a directory or jumps to a file in that directory.
  * Opens a new tab if one isn't active.
* `JumpTo path`
  * Jumps to the parent directory of `path` and scrolls so that `path` is visible.
  * Opens a new tab if one isn't active.
* `Parent`
  * Navigates to the parent of the current directory.

### Tabs

* `NewTab [directory/file]` and `NewBackgroundTab [directory/file]`
  * Opens a new tab in the foreground or background.
  * If directory or file is set, it will behave like `Navigate`.
  * By default it will clone the current tab or the user's home directory.
  * Examples: `Navigate /path/to/directory` `Navigate /path/to/file.png`
* `CloseTab`, `ClosePane`, `CloseActive`
  * Close the active tab, pane, or both.
  * All panes can be closed without closing all tabs.
* `Split horizontal|vertical`
  * Splits the current tab in two.
  * The new tab is on the right or bottom of the split.
  * If no tabs are visible, opens a new on.

### Settings

* `Mode icons|columns`
  * Changes the mode of the current directory.

TODO ---------------------------------

* Child
    * Navigates to the child of the current directory.
    * If there is more than one child this will fail unless "Parent" was used earlier.

* Activate
  * The same as hitting enter or using "Open" in the menu on selected files.
  * It is not recommended to bind this as a shortcut


* Cut/Copy
  * Cuts or copies the current selection.
  * Clears the clipboard if nothing is selected.
* Paste
  * Pastes into the current tab.
  * Calling this from scripts would be strange.
* Execute
  * Requires a single string argument which will be run as an executable.
  * Example: `Execute /path/to/save-page.sh`
* Script
  * Like Execute but reads stdout from the executable as a series of commands to run, one per line.
  * Example: `Script /path/to/sample-script.sh`

TODO ---------------------------------

# Custom Actions

Custom actions are enabled by scripts in the custom-actions directory, default `$HOME/.config/aw-fm/custom-actions/`. Depending on how they are configured they do not always appear in the context menu.

They must be executable text files and options are read from within the file. See the [example script](examples/sample.sh) for an explanation of all the options and environment variables.

## External Executable Environment

The executables from `Execute`, `Script`, and custom actions will be called with no arguments and several environment variables set. [rofi-jump-home.sh](examples/rofi-jump-home.sh) is an example that opens rofi to navigate to a directory inside the user's home directory.

Where relevant, tabs and panes are communicated as JSON:
{
  <!-- "id": number, -->
  "path": string,
  "search": undefined|string,
}

Environment Variable | Explanation
-------------------- | ----------
AWFM_CURRENT_TAB | The currently selected tab, which is also the current pane, as JSON. May be empty.
AWFM_SELECTION | A newline-separated set of selected files. May be empty. <!-- TODO -- how does it handle being huge -->
<!-- AWFM_NEXT_TAB | The next tab as visually seen in the tabs list on the left. If tabs are open but no panes are open, this will be the first tab. May be empty. -->
<!-- AWFM_PREV_TAB | The previous tab as visually seen in the tabs list on the left. May be empty. -->
<!-- AWFM_NEXT_PANE | The tab open in the "next" pane. Pane ordering is based on how they were opened as a tree, with left/top tabs coming before right/bottoms tabs. May be empty. -->
<!-- AWFM_PREV_PANE | The tab open in the "previous" pane. Pane ordering is based on how they were opened as a tree, with left/top tabs coming before right/bottoms tabs. May be empty. -->
<!-- AWFM_PID | The PID of the aw-fm process. -->
<!-- AWFM_SOCKET | The socket used for IPC, if enabled. -->
<!-- AWFM_WINDOW | The window ID for the primary window. Currently only on X11. -->

## IPC Socket API

<!-- TODO -- socket print -->
If configured, aw-fm will expose a limited API over a unix socket, one per process. See the documentation in [aw-fm.toml](aw-fm.toml.sample) and the [example script](examples/socket-print.sh).

Request | Response
--------|---------------------------------------------------------------------------------------
Status  | The same set of environment variables sent to shortcut executables.
<!-- ListTabs  | List all the open tabs in visual order. Format is { "id": number, "path": string, "search": undefined|string } -->
<!-- DumpSession  | Dumps the session as a series of commands that can be run in a script to load a session. See [save-session.sh](examples/save-session.sh) and [load-session.sh] (examples/save-session.sh)-->

The API also accepts any valid action that you could specify in a shortcut, including external executables. Don't run this as root.

# Building on Windows

Not planned, good luck.

# Development

* RUST_LOG=Trace for spam
* GTK_DEBUG=Interactive
* G_MESSAGES_DEBUG=GnomeDesktop for thumbnailer issues

# Why

Gui file managers on Linux aren't in a good state. I can't solve that. I can write a file manager for myself, though.

