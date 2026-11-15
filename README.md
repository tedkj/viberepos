# VibeRepos

Every git repo on your Windows PC at a glance: what is not committed, not pushed, behind, or left
half done.

Point it at the folders that hold your projects (it suggests the usual ones on first start) and
it finds every repo in them, worktrees, submodules and bare repos included. One row per repo: the
branch, changes not committed, commits not pushed or not pulled, stashes, a merge or rebase left
half done, a branch whose upstream is gone, no remote at all, and how old that knowledge is. Filter
the list, open a repo in Explorer or a terminal, or copy the list as CSV.

It is strictly read-only. The one thing that touches your repos is **Fetch all**, when you press
it, and that only updates the remote branches, the same as `git fetch`. It never commits, pushes,
pulls, checks out or stashes, and it switches off the hooks and filters a repo's own config
would otherwise run.

It is written in Rust with a native Slint interface, with no browser engine inside.

## What it needs

Git for Windows, 2.36 or newer. VibeRepos reads repos through your own git and doesn't bring its
own; it finds git on your PATH or in the usual install folders, or you can point it at `git.exe`.
It never runs a `git.exe` from inside a folder it scans.

## What it touches on your PC

- **Admin rights:** not needed.
- **Keeps:** its settings (the folders it looks in) in `%APPDATA%\com.tedkj.viberepos`. Run from a
  USB stick (or with a file named `portable` next to the exe) it keeps them in `VibeRepos-data`
  beside the exe instead.

## What it connects to

Only your repos' remotes when you press Fetch all (through your git and its own credential
settings; VibeRepos never asks for or stores passwords), and getvibeware.com when you press
About's "Check this install". Nothing else is sent anywhere: no accounts, no tracking, no
GitHub or other host API.

## Download

Get the installer from [Releases](https://github.com/tedkj/viberepos/releases) or from
[getvibeware.com](https://getvibeware.com). It installs for your user only, without admin rights.
The installer and the app are code signed by "Open Source Developer Ted Kenneth Johansen" (the
certificate's name): right-click the file, Properties › Digital Signatures. Compare the
installer's SHA-256 with the one on the release page:

```powershell
Get-FileHash .\VibeRepos_0.1.0-preview_x64-setup.exe
```

## Building from source

You need a current stable Rust toolchain on Windows (the MSVC target).

```sh
cargo build --release --locked
```

The app is then at `target\release\viberepos.exe`. `vendor/` holds Slint's `i-slint-core` with
one small repaint fix; `vendor/README.md` says what changed and why.

### Building the installer

`installer\build.ps1` builds the app and its installer,
`target\release\bundle\VibeRepos_<version>_x64-setup.exe`. It needs the official NSIS 3.13:
download `nsis-3.13.zip` from SourceForge (SHA-256
`ba63dffc4410ee89193e1cb5a41989991bd77c61068da17e3156d136b7b0b3d8`) and unpack it to
`_tools\nsis-3.13` in the folder that holds this repository (`..\_tools\nsis-3.13`). The script
checks that whole folder against a pinned hash before it runs anything from it.
`installer\notices.ps1` writes `THIRD-PARTY-NOTICES.txt` for each build (every crate the app
links, with its licence text), and the installer puts it and `LICENSE` next to the app.

## License

[AGPL-3.0](LICENSE). Use it, change it, share it. If you hand out a changed version, or run one as
a service for other people, you share your changes under the same license. Slint is used under
its GPLv3 option.

Made in Norway by Ted K. Johansen.
