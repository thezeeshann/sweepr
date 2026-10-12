# sweepr

Monitor, clean, analyze and maintain your Mac from the terminal. One small Rust
binary, no runtime dependencies.

```
sweepr status       live dashboard: CPU, memory, disk + I/O, network, controls, processes
sweepr clean        deep clean: caches, logs, leftovers, orphaned app data
sweepr analyze      disk analyzer: where did the space go, find large files
sweepr uninstall    remove an app with its preferences, caches and LaunchAgents
sweepr maintain     flush DNS, refresh QuickLook/icons, compact databases
```

<img width="1470" height="956" alt="Screenshot 2026-10-13 at 12 37 28 AM" src="https://github.com/user-attachments/assets/800e1816-bd82-420e-9bf3-93b911ee302b" />

## Install

macOS only. Tested on macOS 27 / Apple Silicon (Intel and older macOS untested).
You need a Rust toolchain: `curl https://sh.rustup.rs -sSf | sh` (or `brew install rust`).

```sh
cd ~/Desktop/sweepr
cargo install --path . --locked      # builds a release binary into ~/.cargo/bin
sweepr --version                     # check it works
```

If `sweepr` is "not found", add Cargo's bin folder to your `PATH`:

```sh
echo 'export PATH="$HOME/.cargo/bin:$PATH"' >> ~/.zshrc && source ~/.zshrc
```

Try it without installing: `cargo run --release -- status`.
Update after changing the code: run the `cargo install` line again.
Uninstall: `cargo uninstall sweepr && rm -rf ~/.config/sweepr`.

## Run

Use a real terminal (Terminal, iTerm2, Ghostty); the interactive screens need a TTY.
Do not run it with `sudo`: `clean` and `uninstall` refuse, because they would act on root's home.
`maintain` asks for your password itself when a task needs it.

**Start here, in this order:**

1. `sweepr status` to look around. Quit with `q`.
2. `sweepr clean --dry-run` to preview. It deletes nothing and writes the full list to
   `~/.config/sweepr/clean-list.txt`.
3. `sweepr clean` when you like what you saw. It shows the total and asks `[y/N]`.
4. `sweepr analyze` to see what is using the disk.

### Full Disk Access (recommended)

macOS hides some folders (Safari and Messages data, Trash, parts of Library) from
terminals. Without access, sweepr tells you what it could not read instead of
guessing. To grant it: **System Settings → Privacy & Security → Full Disk Access**,
enable your terminal app, then restart the terminal.

## Commands

### `sweepr status`

| Key | Action |
|---|---|
| `q` / `Esc` | quit |
| `c` / `m` | sort processes by CPU / memory |
| `space` | pause |

Panels: CPU (per core, P/E cores), memory and swap, disk usage and read/write rates,
network per interface, **Controls** (Wi-Fi, Bluetooth, AirDrop, Focus, display
brightness, sound volume, now playing), top processes.

```sh
sweepr status --json                  # one snapshot (field names match Mole's JSON)
sweepr status --watch --interval 2s   # newline-delimited JSON stream
sweepr status --now-playing           # also show Music/Spotify (macOS asks permission once)
```

Controls are read-only, and anything macOS will not reveal shows as unknown, never
as off: the Wi-Fi network name is hidden by macOS privacy, Focus needs Full Disk
Access, and Chrome/Safari media is not readable (Music and Spotify only).

### `sweepr clean`

```sh
sweepr clean --dry-run                   # preview only
sweepr clean                             # scan, confirm, delete
sweepr clean --yes                       # no prompt (required in scripts)
sweepr clean --only browsers --only dev  # selected sections (prefix match)
sweepr clean --protect '~/Library/Caches/Foo*'   # whitelist a path or glob
sweepr clean --unprotect '~/Library/Caches/Foo*'
sweepr clean --whitelist                 # show active protections
```

Sections: User essentials, App caches, Browsers, Developer tools, Application
Support, App leftovers (orphaned data of apps that are gone). Caches of running
apps are skipped.

### `sweepr analyze [PATH]`

No path opens an overview of your Mac; a path opens that folder.

| Key | Action |
|---|---|
| `↑↓` `j k` | move |
| `enter` / `→` | open folder |
| `esc` / `←` | back (quits at the first screen) |
| `space` | select (then act on all selected) |
| `/` | filter by name (`esc` clears) |
| `s` | sort: size, name, modified |
| `t` | large files under here (default 100 MB) |
| `o` / `p` / `f` | open / Quick Look preview / reveal in Finder |
| `del` or `d` | move to Trash (asks first; only `y` confirms) |
| `r` | refresh |
| `q` | quit |

```sh
sweepr analyze ~/Developer --min-size 500MB
sweepr analyze --json ~/Library          # sizes as JSON
```

Deleting only ever moves items to the Trash, only inside your home folder, and never
a system folder (Desktop, Documents, Library, ...) itself. Empty the Trash to
reclaim the space.

### `sweepr uninstall [APP...]`

```sh
sweepr uninstall                 # pick from a searchable list (space select, / search)
sweepr uninstall --list          # installed apps with sizes
sweepr uninstall Slack --dry-run # show exactly what would be removed
sweepr uninstall Slack           # remove it (asks first)
```

Finds the app's preferences, caches, containers, group containers, extension data,
LaunchAgents and recent-documents lists from its bundle id and name variants, stops
its launch agents, and moves everything to the Trash. Running apps are refused,
Homebrew Cask apps are pointed to `brew uninstall --cask --zap`, and files that need
administrator rights are listed but not touched.

### `sweepr maintain`

```sh
sweepr maintain --list
sweepr maintain --dry-run
sweepr maintain --only quicklook      # dns | quicklook | databases
```

DNS flush needs your password (run it in a terminal). Database compaction needs
Safari/Messages closed and Full Disk Access; it checks integrity first and skips
anything over 100 MB.

## Where it differs from Mole

- **Asks before deleting.** Mole deletes immediately; `clean` and `uninstall` ask first.
- **Home folder only.** No sudo cleaning of system locations.
- **Trash for uninstall and analyze.** Recoverable, unlike permanent deletion.
- **Stricter symlink guard.** Any symlink among a target's ancestors below `$HOME`
  is refused.
- **Running browsers** are skipped entirely while open.
- **Smaller catalog.** About 190 curated clean targets plus the generic cache sweep and
  orphan detection. Mole's CLI-driven cleaners (pnpm, pip, Go, Docker, Xcode, ...),
  Cloud & Office, health score, thermal/battery/GPU panels, project-artifact purge and
  installer cleanup are not ported.
- **Uninstaller** does not derive `~/.config/<name>`-style folders from an app's
  display name (Mole does, with extra guards), and does not remove login items.
- **Maintenance** covers DNS, QuickLook/icons and databases only; Mole's other
  tasks (LaunchServices rebuild, network stack, Spotlight rebuild, ...) are not ported.

## Safety model

Every deletion candidate passes the same gate: absolute path, no `..`, no control
characters, critical-path denylist, protected app/bundle lists (extracted from Mole's
data files), whitelist, running-owner deferral, then sizing. Removal re-validates and
requires the same device/inode that was scanned. Symlinks are unlinked, never
followed. Orphan detection fails closed if any installed app's identity cannot be
read. Unreadable data is reported, never counted as empty.

Config lives in `~/.config/sweepr/`.

## Development

```sh
cargo test          # 100+ tests, including temp-home end-to-end runs
cargo clippy --all-targets
```
