# Project rename and linking

Darc has three project-management commands for renamed or merged checkouts:

- `darc project link <project>` links one old project's paths into the current project.
- `darc project remove <project>` removes one configured project plus its archived and indexed data.
- `darc project rename-from <project>` is the full rename migration workflow.

The old top-level forms (`darc link`, `darc remove`, and `darc rename-from`) remain callable during development, but
they are hidden from `darc --help`.

## darc project link

Use `link` when you want the current project to recognize another configured project's old paths, but you do not want to remove the old project yet.

Run it from the target project directory. The argument is the old or source project name stored in `~/.darc/config.toml`.

Example:

```bash
cd /path/to/new-project
darc project link old-project
```

This means:

- current directory `/path/to/new-project` is the target project
- `old-project` is the old or source project already known to Darc

`link` only updates config. It does not run `refresh`, and it does not remove the source project.

## darc project remove

Use `remove` when you want to delete a configured project entirely.

Example:

```bash
darc project remove --dry-run old-project
darc project remove old-project
```

`remove` matches the configured project by its `name` in `~/.darc/config.toml`. The name must match exactly one project.
Use `--dry-run` first when you want the resolved project, archive path, and indexed row counts without deleting anything.

It deletes:

- the project entry from `config.toml`
- the project's archived sessions directory under `~/.darc/projects/...`
- the project's indexed SQLite rows

You can run `remove` from any directory.

## darc project rename-from

Use `rename-from` when you just renamed a project from one name to another and want Darc to move future history under the new project identity.

Example:

```bash
cd /path/to/new-project
darc project rename-from --dry-run old-project
darc project rename-from old-project
```

This is the intended workflow when:

- the old project was named `old-project`
- the new checkout path is now `/path/to/new-project`
- Darc config still only knows the old project name `old-project`

In that example:

- current directory `/path/to/new-project` is the new or target project
- `old-project` is the old or source project name

`rename-from` does all of this:

1. creates or reuses the target project from the current checkout
2. links the old project's paths into the target project
3. runs `darc refresh`
4. removes the old source project if the previous steps succeed

Use `--dry-run` first when you want to confirm the target project, linked paths, refresh step, and source cleanup before writing.

So it is the safe built-in version of:

```bash
cd /path/to/new-project
darc project link old-project
darc refresh
darc project remove old-project
```

If you have not initialized Darc yet and `~/.darc/config.toml` does not exist, run:

```bash
darc init
```
