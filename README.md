# wow-tux-forge

Terminal UI (TUI) manager for World of Warcraft addons hosted on GitHub.

![Installed addons view](assets/installed.png)
![GitHub search results view](assets/search.png)

## Features

- **GitHub-native:** Search repositories (including forks), pick from available release `.zip` versions, and install directly into `Interface/AddOns`.
- **Automatic updates & pinning:** Checks for updates on launch (cached once per hour), updates only outdated addons with `u`, and lets you pin versions with `p`.
- **Local addon linking:** Automatically scans existing local addons on launch and links them to GitHub via `.toc` metadata.
- **Interactive security audit:** Blocks executables, flags suspicious Lua patterns before extraction, and lets you inspect flagged code snippets (`Enter`) or accept/cancel the install.
- **Rate-limit friendly:** Uses `GITHUB_TOKEN` or `gh auth token` automatically when available.

## Install

### Homebrew (Recommended)

```bash
brew tap pandabytez/tap && brew trust pandabytez/tap && brew install wow-tux-forge
```

### Cargo

```bash
cargo install --git https://github.com/PandaBytez/wow-tux-forge
```

Run `wow-tux-forge` and press `c` on first launch to set your WoW `Interface/AddOns` directory path.

## Keybindings

| Key | Action |
|---|---|
| `1` / `2` / `Tab` | Switch between **Installed** and **Search** tabs |
| `c` | Configure `AddOns` directory path |
| `j` / `k` / `↑` / `↓` | Navigate lists |
| `u` | Update all outdated addons (Installed) |
| `l` | Link local addons from `.toc` GitHub URLs (Installed) |
| `a` | Security audit all addons (Installed) |
| `s` | Rescan local `AddOns` folder (Installed) |
| `p` | Pin/unpin selected addon version (Installed) |
| `d` | Delete selected addon (Installed) |
| `Enter` / `i` | Install/reinstall selected addon (Search) or confirm |
| `/` | Focus search input (Search) |
| `q` | Quit |

## Data Locations

- Config: `~/.config/wow-tux-forge/config.json` (platform `dirs::config_dir`)
- State: `~/.local/share/wow-tux-forge/state.json` (platform `dirs::data_local_dir`)

## License

[GPL-3.0](LICENSE)
