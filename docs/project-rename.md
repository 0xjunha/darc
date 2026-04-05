# Project rename and linking

Darc has three project-management commands for renamed or merged checkouts:

- `darc link <project>` links one old project's paths into the current project.
- `darc remove <project>` removes one configured project plus its archived and indexed data.
- `darc rename-from <project>` is the full rename migration workflow.

## darc link

Use `link` when you want the current project to recognize another configured project's old paths, but you do not want to remove the old project yet.

Run it from the target project directory. The argument is the old or source project name stored in `~/.darc/config.toml`.

Example:

```bash
cd /path/to/new-project
darc link old-project
```

This means:

- current directory `/path/to/new-project` is the target project
- `old-project` is the old or source project already known to Darc

`link` only updates config. It does not run `sync`, does not run `parse`, and does not remove the source project.

## darc remove

Use `remove` when you want to delete a configured project entirely.

Example:

```bash
darc remove old-project
```

`remove` matches the configured project by its `name` in `~/.darc/config.toml`. The name must match exactly one project.

It deletes:

- the project entry from `config.toml`
- the project's archived sessions directory under `~/.darc/projects/...`
- the project's indexed SQLite rows

You can run `remove` from any directory.

## darc rename-from

Use `rename-from` when you just renamed a project from one name to another and want Darc to move future history under the new project identity.

Example:

```bash
cd /path/to/new-project
darc rename-from old-project
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
3. runs `darc sync`
4. runs `darc parse`
5. removes the old source project if the previous steps succeed

So it is the safe built-in version of:

```bash
cd /path/to/new-project
darc link old-project
darc sync
darc parse
darc remove old-project
```

If you have not initialized Darc yet and `~/.darc/config.toml` does not exist, run:

```bash
darc init
```
