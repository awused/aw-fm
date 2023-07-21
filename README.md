# AW-FM

Awused's personal gui file manager.

It is a simple file manager designed to be fast and efficient at doing what I actually do.

# Features

* Fast
    * The priorities are quality, then latency, then vram usage, then memory usage, then CPU usage.
    * Animated gifs are moderately memory-inefficient.
* Correct gamma and alpha handling during scaling and presentation.
* Wide support for many archive and image formats.
* Proper natural sorting of chapters even with decimal chapter numbers.
    * Works well with [manga-syncer](https://github.com/awused/manga-syncer), but generally matches expected sorting order.
* Configurable shortcuts to run external scripts and a basic IPC interface.
* Support for custom external upscalers. See [aw-upscale](https://github.com/awused/aw-upscale).
* Good support for manga layouts including side-by-side pages and long strips.
* Not much more, anything I don't personally use doesn't get implemented.

# Installation

`cargo install --git https://github.com/awused/aw-man --locked`

Install and run with aw-man. Optionally edit the defaults in [aw-fm.toml.sample](aw-fm.toml.sample)
and copy it to `~/.config/aw-fm/aw-fm.toml` or `~/.aw-fm.toml`.

<!-- Recommended to install the desktop file in the [desktop](desktop) folder. -->

# Dependencies

Required:

* GTK - GTK4 libraries and development headers must be installed.

On fedora all required dependencies can be installed with `dnf install gtk4-devel`.

# Usage

# Shortcuts

## Customization

Keyboard shortcuts and context menu entries can be customized in [aw-man.toml](aw-man.toml.sample). See the comments in the config file for how to specify them.

Recognized commands:

* Help
  * List current keybinds.
* Quit

Navigation
* Parent
  * Navigates to the parent of the current directory

Settings
* Mode icons|columns
  * Changes the mode of the current directory

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
* JumpToFile file
  * Opens the parent directory in the current tab (or creates a new tab) and navigates to file.
  * Examples: `JumpTo /home/me/some_important_file.png`
* OpenToFile
  * Like JumpToFile but always opens a new tab.
  * Examples: `OpenToFile /home/me/some_important_file.png`
* Navigate
  * Navigates the current tab to a directory.
  * If no tab is open, one will be opened.
  * Examples: `Navigate /path/to/directory`
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

Environment Variable | Explanation
-------------------- | ----------
AWFM_CURRENT_DIR | The directory of the current selected tab. May be empty.
AWFM_SELECTION | A newline-separated set of selected files.
AWMAN_ARCHIVE | The path to the current archive or directory that is open.
AWMAN_ARCHIVE_TYPE | The type of the archive, one of `archive`, `directory`, `fileset`, or `unknown`.
AWMAN_BACKGROUND | The current background colour in `rgb(int, int, int)` or `rgba(int, int, int, float)` form.
AWMAN_CURRENT_FILE | The path to the extracted file or, in the case of directories, the original file. It should not be modified or deleted.
AWMAN_DISPLAY_MODE | The current display mode, either `single` or `verticalstrip`.
AWMAN_FIT_MODE | The current fit mode, one of `container`, `height`, `width`, or `verticalstrip`.
AWMAN_FULLSCREEN | Wether or not the window is currently fullscreen.
AWMAN_MANGA_MODE | Whether manga mode is enabled or not.
AWMAN_PAGE_NUMBER | The page number of the currently open file.
AWMAN_PID | The PID of the aw-man process.
AWMAN_RELATIVE_FILE_PATH | The path of the current file relative to the root of the archive or directory.
AWMAN_SOCKET | The socket used for IPC, if enabled.
AWMAN_UI_VISIBLE | Whether the UI (bottom bar) is currently visible.
AWMAN_UPSCALING_ENABLED | Whether upscaling is enabled or not.
AWMAN_WINDOW | The window ID for the primary window. Currently only on X11.

# Building on Windows

Not planned, good luck.

# Development

* RUST_LOG=Trace for spam
* GTK_DEBUG=Inspector
* G_MESSAGES_DEBUG=GnomeDesktop for thumbnailer issues

# Why

Gui file managers on Linux aren't in a good state. I can't solve that. I can write a file manager for myself, though.

