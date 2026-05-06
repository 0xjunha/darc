## Upgrade

Check for newer Darc CLI releases:

```sh
darc upgrade --check
darc upgrade --check --json
darc upgrade
```

Darc can show a short startup nudge when a newer release is available. To enable it, set
`check_for_update_on_startup = true` in `~/.darc/config.toml`. Write-oriented human commands such as `refresh`, `sync`,
`index`, and mutating project/service commands read the cached release metadata under `~/.darc/run`; when the cache is
stale, Darc refreshes it after the command completes. Read-only commands such as `status`, `search`, `list`, and
`service status` do not perform passive checks. Set `DARC_NO_UPDATE_CHECK=1` to suppress passive checks for one process.
To hide one release:

```sh
darc upgrade dismiss <VERSION>
darc upgrade dismiss --root <ROOT> <VERSION>  # custom Darc root
```

## Uninstall

If you enabled the macOS background refresh service, turn it off before removing the binaries:

```sh
darc service disable
```

Then remove the binaries installed by the release installer:

```sh
rm -f ~/.local/bin/darc ~/.local/bin/darc-update
```

If you installed Darc into a custom directory, remove both binaries from that directory instead.

Darc keeps local data under `~/.darc`. Uninstalling the binary does not delete that archive. To delete Darc data too:

```sh
rm -rf ~/.darc
```

If you used `--root <path>` with Darc, remove that custom root instead.
