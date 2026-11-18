# unrun

Undo what a command did to your project folder. Built for the case where an AI
agent (or you) runs `rm`, `mv` or a script and you want the files back.

`unrun` is a working name.

## Use

```
unrun init                      # start tracking this folder
unrun run -c "rm -r src"        # run a shell string and record what it changed
unrun run -- cargo fmt          # or run a program directly
unrun log                       # recorded commands
unrun show [id]                 # files a command changed
unrun undo [id]                 # revert it (latest by default)
unrun redo [id]                 # re-apply it
```

`undo` refuses to overwrite a file that changed again after the command ran;
`--force` overrides.

### PowerShell: record every command

Add to `$PROFILE`:

```powershell
unrun shell-init pwsh | Out-String | Invoke-Expression
```

### Claude Code: record every command the agent runs

In `.claude/settings.json`:

```json
{
  "hooks": {
    "PreToolUse": [
      { "matcher": "Bash|PowerShell", "hooks": [{ "type": "command", "command": "unrun hook pre" }] }
    ],
    "PostToolUse": [
      { "matcher": "Bash|PowerShell", "hooks": [{ "type": "command", "command": "unrun hook post" }] }
    ]
  }
}
```

The hook is a no-op in folders without `.unrun/`, and never blocks the agent.

## How it works

No interception. Before a command, `unrun` walks the folder and copies any file
that changed since the last walk into a content-addressed store
(`.unrun/objects`, BLAKE3). After the command it walks again and writes the
difference to `.unrun/journal`. Undo copies the old contents back.

Because it only compares before and after, it works for any program: static
binaries, Go programs, scripts, anything.

## Limits in v0

- Only the tracked folder is covered. `rm -rf ~` from inside a project is not undoable.
- Symlinks are not tracked. Files over 256 MB (`UNRUN_MAX_FILE_MB`) are noticed but not restorable.
- Names in `.unrun/ignore` (`node_modules`, `target`, `.venv`, `__pycache__` by default) are skipped.
- One command at a time: overlapping `begin`/`end` pairs are merged into one entry.
- The store only grows; there is no garbage collection yet.
- The first snapshot is slow on Windows (about 2 ms per file); later ones only stat.

## Roadmap

- v1 Windows: hook file calls in `ntdll` across the process tree, for whole-disk coverage.
- v1 Linux: overlay in a mount namespace per command.
- Pack small objects into one file to speed up the first snapshot; `unrun gc`.

## Build

Rust stable. On Windows with the GNU toolchain, a MinGW `bin` folder (WinLibs)
must be on `PATH` for `dlltool` and `as`.

```
cargo test
cargo install --path .
```
