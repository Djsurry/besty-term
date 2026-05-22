<p align="center">
  <img src="icon.png" width="160" alt="Besty Term icon" />
</p>

<h1 align="center">Besty Term</h1>

<p align="center">
  A fast, vim-flavored Slack client that lives in your terminal.<br/>
  <sub>6 MB on disk · 27 MB RAM · single static binary</sub>
</p>

---

Heavily inspired by [erroneousboat/slack-term](https://github.com/erroneousboat/slack-term) — the original terminal Slack client, and the jumping-off point for the layout, keybindings, and overall approach.

## What it is

A Rust + ratatui TUI for reading and writing Slack messages. Sidebar of conversations (channels, DMs, MPIMs), threaded chat view, sidebar/chat search, vim-style navigation, mention autocomplete, socket-mode real-time delivery. Stays out of the way.

Every line was written by Claude. I directed the work, but I didn't type any Rust.

## About the name

"Besty" is the company I work at ([besty.ai](https://besty.ai)) — a small team that lives in Slack all day, and this is the client we wanted for ourselves. Nothing in the code is workspace-specific, so anyone's welcome to use it.

## Requirements

- macOS or Linux
- Rust (`brew install rust` or [rustup.rs](https://rustup.rs))
- A Slack user token (`xoxp-…`) — see setup below

## Quick start

```bash
git clone <this-repo> besty-term
cd besty-term
echo 'SLACK_USER_TOKEN=xoxp-your-token-here' > .env
cargo run --release
```

For real-time message delivery (optional but recommended), also set `SOCKET_MODE_APP_TOKEN=xapp-…`. Without it the app polls Slack once a minute as a fallback.

---

## Slack setup

You need a Slack app with the right scopes installed to your workspace, then a User OAuth Token (`xoxp-…`) from it. There are two paths depending on whether your team is in one workspace or many.

### Same workspace as your teammates

**Recommended.** Create one app, share the install link, every teammate gets their own token from the same app.

1. **Workspace owner/admin creates the app once.**
   - Visit <https://api.slack.com/apps?new_app=1>, choose **From manifest**, pick the workspace, and paste the [manifest](#slack-app-manifest) below.
   - On the app's **OAuth & Permissions** page, click **Install to Workspace**. Copy the **User OAuth Token** (starts with `xoxp-`) — this is the admin's token.
   - On **Manage Distribution**, you can either keep the app private (admins approve each install) or enable distribution so teammates can install themselves.
2. **Each teammate installs the app.**
   - Visit the Install link from the app page (or the workspace's "Manage apps" admin page).
   - After OAuth confirmation, copy the **User OAuth Token** issued for *their* account.
   - Put it in their `.env` as `SLACK_USER_TOKEN`.

This way the app is configured once, and each teammate has their own token tied to their own user account.

### Different workspaces

Each teammate creates their own app in their workspace — Slack apps are scoped to a workspace, so there's no way around it.

1. Visit <https://api.slack.com/apps?new_app=1>, choose **From manifest**, pick the workspace.
2. Paste the [manifest](#slack-app-manifest).
3. On **OAuth & Permissions** → **Install to Workspace**. Copy the **User OAuth Token** (`xoxp-…`) into your `.env`.

Takes about 3 minutes per person.

### Slack app manifest

```yaml
display_information:
  name: Besty Term
  description: A fast TUI Slack client for the terminal.
  background_color: "#16161c"
features:
  bot_user:
    display_name: besty-term
    always_online: false
oauth_config:
  scopes:
    user:
      - channels:history
      - channels:read
      - channels:write
      - groups:history
      - groups:read
      - groups:write
      - im:history
      - im:read
      - im:write
      - mpim:history
      - mpim:read
      - mpim:write
      - users:read
      - chat:write
settings:
  org_deploy_enabled: false
  socket_mode_enabled: true
  token_rotation_enabled: false
```

### Optional: socket mode (real-time push)

Without socket mode the app polls Slack once a minute. With socket mode, new messages arrive instantly via WebSocket.

1. In the app's settings, go to **Socket Mode** → enable.
2. Go to **Basic Information** → **App-Level Tokens** → **Generate Token** → add the `connections:write` scope → copy the `xapp-…` token.
3. Add to your `.env`: `SOCKET_MODE_APP_TOKEN=xapp-your-token-here`.

---

## Keybindings

| Mode | Key | Action |
|---|---|---|
| Normal | `j` / `k` | Move conversation selection |
| Normal | `gg` / `G` | Jump to top / bottom |
| Normal | `Ctrl-D` / `Ctrl-U` | Jump section down / up |
| Normal | `i` | Enter insert mode |
| Normal | `/` | Filter sidebar |
| Normal | `?` | Search current chat |
| Normal | `dd` | Hide selected conversation (persists) |
| Normal | `r` | Force-refresh selected conversation |
| Normal | `q` | Quit |
| Insert | type `@` or `#` | Open mention / channel popup |
| Insert (popup open) | `Ctrl-J` / `Ctrl-K` | Cycle popup |
| Insert (popup open) | `Tab` / `Enter` | Accept highlighted match |
| Insert | `Enter` | Send message |
| Insert | `Esc` | Back to normal mode |

---

## Optional: run as a macOS app

Want a Dock icon, single-app feel, and `cmd-tab` switching independent of Terminal.app? Bundle the binary into a `.app` with a vendored Alacritty terminal emulator.

> ⚠️ One-time prep: build Alacritty from source so its binary isn't subject to Apple Gatekeeper quarantine.

```bash
cargo install alacritty                       # ~3 min on M1
cargo build --release                         # ours
```

Then create the bundle (`Besty Term.app/`) with this structure:

```
Besty Term.app/
  Contents/
    Info.plist
    MacOS/
      alacritty                       # cp ~/.cargo/bin/alacritty here
      besty-term                      # the launcher script below
      slack-term                      # cp target/release/slack-term here
    Resources/
      BestyTerm.icns                  # built from icon.png via iconutil
      alacritty.toml                  # window/colors/font config
```

**Launcher** (`Contents/MacOS/besty-term`, `chmod +x` it):

```bash
#!/bin/bash
set -e
DIR="$(cd "$(dirname "$0")" && pwd)"
RES="$(cd "$DIR/../Resources" && pwd)"
exec "$DIR/alacritty" \
    --config-file "$RES/alacritty.toml" \
    --title "Besty Term" \
    -e "$DIR/slack-term"
```

(Your `.env` will be read from the working directory the binary is launched from. Either `cd` into a directory containing it inside the script, or set `SLACK_USER_TOKEN` system-wide.)

**Build the `.icns`:**

```bash
mkdir BestyTerm.iconset
for s in 16 32 64 128 256 512 1024; do
  sips -z $s $s icon.png --out BestyTerm.iconset/icon_${s}x${s}.png
done
iconutil -c icns BestyTerm.iconset
mv BestyTerm.icns "Besty Term.app/Contents/Resources/"
```

**Info.plist** just needs `CFBundleExecutable=besty-term` and `CFBundleIconFile=BestyTerm`. Double-click and go.

---

## Benchmarks

Measured on an M-series Mac, same workspace, same user, both apps fully signed in. Slack measured fresh after launch (process tree settled). The 90% case (read a conversation, reply, see who's pinged you) is here — huddles, canvases, file uploads, and full-history search aren't.

| | Besty Term | Slack | Delta |
|---|---:|---:|---:|
| **Disk (app bundle)** | 6.4 MB | 292 MB | **46× smaller** |
| **RAM (resident, all procs)** | 27 MB¹ | 1,183 MB | **44× smaller** |
| **Processes** | 1¹ | 7 | — |
| **Threads** | 10 | 128 | **13× fewer** |
| **Open file descriptors** | 88 | 393 | **4.5× fewer** |
| **Cold start to first paint** | ~400 ms | ~800 ms² | **2× faster** |
| **Idle CPU (60 s avg)** | 0.0% | 1.4% of one core | — |
| **Idle energy (macOS `top` power score)** | 0.0 | 1.5 | — |

¹ Core `slack-term` Rust binary only. The optional `Besty Term.app` bundle adds a vendored Alacritty terminal host (~110 MB RAM, +1 process) so it can run as a standalone Dock app.

² Time from `open -a Slack` until the renderer process is alive and drawing — comparable to "first frame" for the TUI. Slack continues loading the workspace for several more seconds; Besty Term is usable as soon as the first frame paints.

Methodology: `du -sh` for disk, summed `ps -axo rss` for RAM, `ps -M` for threads, `lsof` for fds, `top -l 60 -s 1` for steady-state CPU/power, scripted `open -a` + process-watch for cold start. Reproduce the cold-start measurement yourself with `osascript -e 'tell application "Slack" to quit'` followed by `time open -a Slack`.
