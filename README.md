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

Default Shortcut | Action
-----------------|-----------

## Customization

Keyboard shortcuts and context menu entries can be customized in [aw-man.toml](aw-man.toml.sample). See the comments in the config file for how to specify them.

Recognized internal commands:

* Help
  * List current keybinds.
* NextPage/PreviousPage/FirstPage/LastPage
  * Optionally takes an argument of `start`, `end`, or `current` to determine what portion of the page will be visible.
* ScrollDown/ScrollUp
  * These may switch to the next or previous page outside of strip mode.
  * Optionally takes a scroll amount as a positive integer `ScrollDown 500`
* ScrollRight/ScrollLeft
  * Optionally takes a scroll amount as a positive integer `ScrollRight 500`
* SnapTop/SnapBottom/SnapLeft/SnapRight
  * Snaps the screen so that the edges of the current page are visible.
* FitToContainer/FitToWidth/FitToHeight/FullSize
* SinglePage/VerticalStrip/HorizontalStrip/DualPage/DualPageReversed
  * Change how pages are displayed.
* NextArchive/PreviousArchive
* Quit
* SetBackground
  * Spawns a dialog allowing the user to select a new background colour.
  * Optionally takes a string recognized by GDK as a colour.
  * Examples: `SetBackground #aaaaaa55` `SetBackground magenta`
* Fullscreen/MangaMode/Upscaling/Playing/UI
  * Toggle the status of various modes.
    * Fullscreen - If the application is full screen.
    * MangaMode - If scrolling down from the last image in an archive will automaticlly load the next archive.
    * Upscaling - Whether or not external upscalers are in use.
    * Playing - Set whether animations and videos are playing.
    * UI - Hide or show the visible portions of the UI.
  * These optionally take an argument of `toggle`, `on` or `off`
  * Examples: `Fullscreen` (equivalent to `Fullscreen toggle`), `MangaMode on`, or `Playing off`
  * ToggleFullscreen/ToggleMangaMode/ToggleUpscaling/TogglePlaying/ToggleUI are older, deprecated versions that do not take arguments.
* Jump
  * Spawns a dialog allowing the user to enter the number of the page they want to display, or the number of pages to shift.
  * Optionally takes an integer argument as either an absolute jump within the same chapter or a relative jump, which can span multiple chapters in Manga mode.
  * Optionally takes a second argument of `start`, `end`, or `current` to determine what portion of the page will be visible.
  * Absolute jumps are one-indexed.
  * Examples: `Jump 25`, `Jump +10`, `Jump -5`, `Jump -4 start`, `Jump +1 current`
* Execute
  * Requires a single string argument which will be run as an executable.
  * Example: `Execute /path/to/save-page.sh`
* Script
  * Like Execute but reads stdout from the executable as a series of commands to run, one per line.
  * Waits for the script to finish. Use `Execute` and the unix socket for more interactive scripting.
  * Example: `Script /path/to/sample-script.sh`
* Open/OpenFolder
  * Spawns a dialog allowing the user to open new files or a new folder.
  * Open can take a series of unescaped but quoted paths.
  * Example `Open /first/path/file.jpg /second/path/file2.jpg "/path/with spaces/file3.jpg"`

# Custom Actions

Custom actions are enabled by scripts in the scripts directory, default `$HOME/.config/aw-fm/scripts/`. 

They must be executable text files and options are read from within the file. See the [example script](examples/sample.sh) for an explanation of all the options and environment variables.

# Building on Windows

Not planned.

# Development

* RUST_LOG=Trace for spam
* GTK_DEBUG=Inspector
* G_MESSAGES_DEBUG=GnomeDesktop for thumbnailer issues

# Why

Gui file managers on Linux aren't in a good state. I can't solve that. I can write a file manager for myself, though.

