# Private Account Swapper web panel

The panel listens on the Unix socket `~/.claude-account-web/web.sock`, not on a TCP port. Only the owning user and members of the socket's group can connect: the socket has mode `0660` and its directory drops all access for others. Caddy publishes it at `http://v50671-kde/accounts/` from the site bound to the machine's Tailscale address and strips `/accounts/` before proxying. The panel is not published on the public Caddy site.

Every request must carry a `Host` header from the allowlist given with `--allowed-host` (default `v50671-kde`, the unit adds the tailnet name and the Tailscale IPv4 address). Other hosts get `403`, which blocks DNS rebinding. Every `POST` additionally needs an `Origin` header equal to `http://` or `https://` plus that allowed host.

## Install

Build and install the command, then install the user service. The web unit uses the same placeholders as the sync unit:

```bash
cargo install --path . --locked
mkdir -p ~/.config/systemd/user
claude_bin="$(command -v claude)"
sed -e "s|__BINARY__|$HOME/.cargo/bin/claude-account|g" \
    -e "s|__CLAUDE_BIN_ENV__|Environment=CLAUDE_ACCOUNT_SWITCHER_CLAUDE_BIN=$claude_bin|g" \
    systemd/claude-account-web.service > ~/.config/systemd/user/claude-account-web.service
systemctl --user daemon-reload
```

Caddy runs as the system user `caddy` and already has `r-x` on `/home/nathanael` through an ACL. Create the socket directory once, owned by the user, with group `caddy` and the setgid bit, so the socket the panel creates inherits group `caddy`:

```bash
sudo install -d -o nathanael -g caddy -m 2750 /home/nathanael/.claude-account-web
getfacl -p /home/nathanael | grep '^user:caddy'
systemctl --user enable --now claude-account-web.service
stat -c '%A %U %G %n' /home/nathanael/.claude-account-web /home/nathanael/.claude-account-web/web.sock
```

The last line must show `drwxr-s--- nathanael caddy` for the directory and `srw-rw---- nathanael caddy` for the socket. Without the directory step the panel creates the directory itself with mode `0750` and group `nathanael`, and Caddy gets `502`.

The account sync service unit can be installed from `systemd/claude-account-sync.service` with the same two replacements. Keep it stopped until the user starts it from the Automatik tab.

Use this route inside the Caddy site that binds to the Tailscale address. It replaces the former TCP route with the marker header:

```caddyfile
handle /accounts {
    redir * /accounts/ 308
}
handle_path /accounts/* {
    reverse_proxy unix//home/nathanael/.claude-account-web/web.sock
}
```

Caddy passes the client's `Host` header through unchanged, so the site addresses `v50671-kde`, `v50671-kde.taile28573.ts.net` and `100.117.29.112` must match the `--allowed-host` list in the unit. Validate the Caddyfile before reloading it. Do not bind this site to a public interface.

## Features

- Inspect saved accounts, subscription names, current usage windows, reset times and sync state.
- Refresh cached usage for all profiles from the dashboard.
- Save the active login, start a new official login, switch accounts with verification, or explicitly skip that check.
- Set independent five-hour and weekly limits per account, including hard limits, or clear those limits.
- Configure automatic switching and the reset ping, run an automatic-switch preview, or execute the selected action immediately.
- Synchronize active credentials, refresh inactive profiles and send a window ping manually.
- Start, stop, restart and enable or disable autostart for the background service.
- Queue a new prompt or resume a recent Claude session, with working directory, title, settings file, model, permission behavior and repeat options. The job waits for a free five-hour window.
- Enable, pause, run, inspect logs for and remove queued jobs.
- Read the automation service journal and background action status.

The OAuth login can depend on the machine's browser session. If the remote login action cannot open or complete the official flow, run the displayed `claude-account login <name>` command in a terminal on the machine, then refresh the panel.
