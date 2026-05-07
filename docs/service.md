# Background Refresh Service

Darc can keep the session archive and SQLite index usually fresh by running the normal refresh workflow continuously.

`darc service` is currently beta. The foreground watch mode is the stable primitive; the background service wrapper is
macOS-only and still hardening around launchd integration, permission prompts, and operational polish.

The quickstart command for automatic background refresh on macOS is:

```bash
darc refresh --auto
```

This is equivalent to `darc service enable` followed by `darc service start`: it enables auto-start on future logins and
starts or restarts the background refresh service now. If auto-refresh is already running, Darc stops the existing
LaunchAgent and starts the updated one.

The foreground command is:

```bash
darc refresh --watch --all
```

This is the process used by the background service. It watches configured Claude and Codex source roots, debounces file
events, periodically reconciles missed events, and runs the same refresh path as `darc refresh --all`.

For Codex sessions, Darc reads Codex's own log files and matches sessions from recorded metadata. It does not probe
arbitrary historical `cwd` directories from those logs during background refresh; older Codex logs without
`git.repository_url` may need the checkout to be explicitly registered or linked before Darc can associate it with the
current project. During `darc project rename-from`, explicitly linked source paths remain recoverable when the linked
path has scoped remote evidence for the pre-rename URL; Darc still skips broad linked child paths with unverified or
mismatched logged remotes.

## macOS support

`darc service` is currently beta and macOS-only. It manages a user LaunchAgent for the current macOS login session.

```bash
darc service enable
darc service start
darc service status
darc service stop
darc service restart
darc service disable
```

- `enable` writes `~/Library/LaunchAgents/com.0xjunha.darc.refresh.plist` so the service auto-starts on future logins.
- `start` loads and starts the service in the current login session, restarting an already loaded service from the
  current plist. If auto-start is not enabled, it uses a runtime plist under `~/.darc/run` instead of writing a
  LaunchAgent auto-start file.
- `stop` unloads the LaunchAgent in the current login session without removing the auto-start file.
- `restart` stops and starts the LaunchAgent.
- `status` reports whether the LaunchAgent file exists, whether launchd has it loaded, the active watch settings, and
  the latest Darc watch status.
- `disable` unloads the LaunchAgent and removes the auto-start file.

Linux systemd user units and Windows service or Task Scheduler support are not implemented yet.

## Configuration

Watch defaults can be stored in `~/.darc/config.toml`:

```toml
[watch]
debounce = "30s"
min_interval = "60s"
reconcile_interval = "10m"
providers = ["claude", "codex"]
poll = false
```

Command-line flags override config values:

```bash
darc refresh --watch --all --debounce 15s --min-interval 60s --reconcile-interval 10m
```

The watch tuning flags `--debounce`, `--min-interval`, `--reconcile-interval`, and `--poll` are valid only with
`--watch`; one-shot `darc refresh` ignores the watch loop entirely. `darc refresh --auto` sets up the macOS background
service instead and cannot be combined with refresh selection or watch-mode flags.

The reconcile interval is a safety refresh measured from the previous refresh attempt. With the default `10m`, `status`
can show a last refresh timestamp that is several minutes old even when the service is healthy.

Runtime files live under the Darc root:

```text
~/.darc/run/status.json
~/.darc/run/refresh.lock
~/.darc/log/refresh-watch.out.log
~/.darc/log/refresh-watch.err.log
```
