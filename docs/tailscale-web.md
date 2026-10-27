# Private Account Swapper web panel

The panel runs on `127.0.0.1:8785`. Caddy publishes it at `http://v50671-kde/accounts/` from the site bound to the machine's Tailscale IPv4 address. Caddy strips `/accounts/` before proxying and adds a private marker header that the app checks on every request. The panel is not published on the public Caddy site.

## Install

Build and install the command, then install the user service:

```bash
cargo install --path . --locked
mkdir -p ~/.config/systemd/user
cp systemd/claude-account-web.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now claude-account-web.service
```

The account sync service unit can be installed from `systemd/claude-account-sync.service` after replacing `__BINARY__` with the absolute path to `claude-account` and `__CLAUDE_BIN_ENV__` with the `CLAUDE_ACCOUNT_SWITCHER_CLAUDE_BIN` environment line. Keep it stopped until the user starts it from the Automatik tab.

Add this route inside the Caddy site that binds to the Tailscale address:

```caddyfile
handle /accounts {
    redir * /accounts/ 308
}
handle_path /accounts/* {
    reverse_proxy 127.0.0.1:8785 {
        header_up X-Account-Swapper-Private-Proxy tailscale-caddy-v50671
    }
}
```

Validate the Caddyfile before reloading it. Do not bind this site to a public interface.

## Features

- Inspect saved accounts, subscription names, current usage windows, reset times and sync state.
- Refresh cached usage for all profiles from the dashboard.
- Save the active login, start a new official login, switch accounts with verification, or explicitly skip that check.
- Set independent five-hour and weekly limits per account, including hard limits, or clear those limits.
- Configure automatic switching and the reset ping, run an automatic-switch preview, or execute the selected action immediately.
- Synchronize active credentials, refresh inactive profiles and send a window ping manually.
- Start, stop, restart and enable or disable autostart for the background service.
- Queue a new prompt or resume a recent Claude session, with working directory, title, settings file, model, timeout, permission behavior and repeat options.
- Enable, pause, run, inspect logs for and remove queued jobs.
- Read the automation service journal and background action status.

The OAuth login can depend on the machine's browser session. If the remote login action cannot open or complete the official flow, run the displayed `claude-account login <name>` command in a terminal on the machine, then refresh the panel.
