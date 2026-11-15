# Running Patina as a systemd user service

A user service starts Patina at boot, restarts it if it stops, and gives it the cgroup that turns on per-command resource limits. The unit file is `packaging/patina.service` in the repository and in each release.

## Install

```sh
mkdir -p ~/.config/systemd/user
cp patina.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now patina
loginctl enable-linger "$USER"      # keep it running while you are logged out
journalctl --user -u patina -f      # its log, including the setup token on the first start
```

The unit expects the binary at `~/.local/bin/patina`, where the install script puts it; change `ExecStart` if yours is elsewhere. Pass flags there too, for example `ExecStart=%h/.local/bin/patina serve --port 7900`.

## Resource limits

`Delegate=yes` gives Patina its own cgroup. At start it moves itself into a `daemon` child and gives each command a cgroup of its own with the project's `limits` (see [config.md](config.md)). The log says which limits hold:

```
commands get cgroup v2 limits on memory, processes and CPU under /sys/fs/cgroup/user.slice/.../patina.service
```

and Settings, Sandbox lists them. A user service can hand out only the controllers your user's systemd instance has. If the log names fewer than memory, processes and CPU, see what your user gets:

```sh
cat /sys/fs/cgroup/user.slice/user-$(id -u).slice/user@$(id -u).service/cgroup.controllers
```

and, if `memory` or `cpu` is missing, delegate them to user services once (as root):

```sh
sudo mkdir -p /etc/systemd/system/user@.service.d
printf '[Service]\nDelegate=cpu cpuset io memory pids\n' | sudo tee /etc/systemd/system/user@.service.d/delegate.conf
sudo systemctl daemon-reload
```

then log out and in, and restart Patina. This needs the unified cgroup v2 hierarchy, the default on current distributions. Without it Patina runs without per-command memory, process and CPU limits and says so; the container backend and the per-file size cap still apply.

## Stopping

`systemctl --user stop patina` sends SIGTERM to Patina alone (`KillMode=mixed`). It stops accepting requests, ends model streams, marks running runs interrupted and stops their commands. Resume those runs after the next start.

## Keys from the environment

A provider with `api_key_ref = "env:NAME"` reads the variable from the daemon's environment. Put such variables in a file only you can read and uncomment `EnvironmentFile=` in the unit. Keys entered in Settings need none of this: they are stored encrypted in the database.
