# Running `rings` Persistently

The `rings` CLI does not manage operating-system service state: `rings run` runs in the
foreground until it receives `SIGINT` or `SIGTERM`. Operators who want a node to start on
login and restart on failure install and remove one of the static service definitions in this
directory explicitly, so the file on disk and the service manager remain the single source of
truth.

## Prerequisite

Install `rings` and initialize the default configuration (`~/.rings/config.yaml`) and API token
before enabling the service:

```sh
rings init
```

A delegatee key expires after `--ttl` (30 days by default), and a daemon without renewal stops 25 s
before expiry (`DelegationHorizonReached`) because a service supervisor restart alone cannot mint a
new delegation. For unattended persistent daemons, either configure `delegator_key` (mode `0600`)
in `~/.rings/config.yaml` so `rings run` renews its delegation in process before the horizon, or
refresh `~/.rings/delegatee_key` offline with `rings new-delegation` before the horizon; see
[Delegations and keys](../docs/src/cli.md#delegations-and-keys).

## Linux (`systemd` user unit)

[`contrib/systemd/rings.service`](./systemd/rings.service) runs `rings run -c %h/.rings/config.yaml`
under your user manager. By default its `ExecStart` points at `%h/.local/bin/rings`; if
`install.sh` placed the binary in `/usr/local/bin/rings`, edit `ExecStart` in your copy to use
`/usr/local/bin/rings`.

### Enable and start

```sh
mkdir -p ~/.config/systemd/user
cp contrib/systemd/rings.service ~/.config/systemd/user/rings.service
systemctl --user daemon-reload
systemctl --user enable --now rings.service
```

To start the user manager at boot even when no interactive session is logged in:

```sh
loginctl enable-linger "$USER"
```

### Inspect status and logs

```sh
systemctl --user status rings.service
journalctl --user -u rings.service -f
```

### Stop and remove

```sh
systemctl --user disable --now rings.service
rm -f ~/.config/systemd/user/rings.service
systemctl --user daemon-reload
```

## macOS (`launchd` LaunchAgent)

[`contrib/launchd/rs.rings.node.plist`](./launchd/rs.rings.node.plist) runs `/usr/local/bin/rings run`
as a per-user LaunchAgent (`rs.rings.node`). If `install.sh` placed the binary in
`~/.local/bin/rings`, replace `/usr/local/bin/rings` in `ProgramArguments` with the absolute path
to `/Users/<you>/.local/bin/rings` (`launchd` does not expand `~` or `$HOME` in
`ProgramArguments`).

### Enable and start

```sh
mkdir -p ~/Library/LaunchAgents
cp contrib/launchd/rs.rings.node.plist ~/Library/LaunchAgents/rs.rings.node.plist
launchctl bootstrap gui/"$(id -u)" ~/Library/LaunchAgents/rs.rings.node.plist
```

### Inspect status and logs

```sh
launchctl print gui/"$(id -u)"/rs.rings.node
tail -f /tmp/rs.rings.node.out.log /tmp/rs.rings.node.err.log
```

### Stop and remove

```sh
launchctl bootout gui/"$(id -u)" ~/Library/LaunchAgents/rs.rings.node.plist
rm -f ~/Library/LaunchAgents/rs.rings.node.plist /tmp/rs.rings.node.out.log /tmp/rs.rings.node.err.log
```
