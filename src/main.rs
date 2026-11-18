mod ignore;
mod journal;
mod snapshot;
mod workspace;

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};

use ignore::Ignore;
use journal::{Entry, Pending};
use snapshot::{Kind, Manifest};
use workspace::{Workspace, now_ns, read_json, write_json};

#[derive(Parser)]
#[command(name = "unrun", version, about = "Undo what a command did to your project folder")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start tracking the current folder
    Init,
    /// Run a command and record what it changed
    Run {
        /// Run this string through the shell (pwsh on Windows, sh elsewhere)
        #[arg(short = 'c', long = "shell", value_name = "STRING", conflicts_with = "argv")]
        shell: Option<String>,
        /// Program and arguments to run directly
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        argv: Vec<String>,
    },
    /// Revert a recorded command (the latest by default)
    Undo {
        id: Option<u64>,
        /// Overwrite files that changed again since the command ran
        #[arg(long)]
        force: bool,
    },
    /// Re-apply a command that was undone (the latest by default)
    Redo {
        id: Option<u64>,
        #[arg(long)]
        force: bool,
    },
    /// List recorded commands
    Log {
        #[arg(short, default_value_t = 15)]
        n: usize,
    },
    /// Show the files a recorded command changed
    Show { id: Option<u64> },
    /// Snapshot before a command (for shell and agent integrations)
    Begin {
        #[arg(long, default_value = "")]
        command: String,
    },
    /// Record what changed since the matching `begin`
    End {
        #[arg(long, allow_hyphen_values = true)]
        exit: Option<i32>,
    },
    /// Claude Code hook entry point: reads the hook JSON on stdin
    Hook {
        #[arg(value_enum)]
        phase: Phase,
    },
    /// Print the shell integration snippet
    ShellInit {
        #[arg(value_enum)]
        shell: Shell,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum Phase {
    Pre,
    Post,
}

#[derive(Clone, Copy, ValueEnum)]
enum Shell {
    Pwsh,
}

const PWSH_INIT: &str = r#"# unrun shell integration. Add to your $PROFILE:
#   unrun shell-init pwsh | Out-String | Invoke-Expression
function global:PSConsoleHostReadLine {
    $exit = $global:LASTEXITCODE
    unrun end
    $line = [Microsoft.PowerShell.PSConsoleReadLine]::ReadLine($Host.Runspace, $ExecutionContext)
    if ($line -and $line -notmatch '^\s*unrun\b') { unrun begin "--command=$line" }
    $global:LASTEXITCODE = $exit
    $line
}
"#;

fn main() {
    if let Err(err) = dispatch(Cli::parse()) {
        eprintln!("unrun: {err:#}");
        std::process::exit(1);
    }
}

fn dispatch(cli: Cli) -> Result<()> {
    let cwd = std::env::current_dir()?;
    match cli.cmd {
        Cmd::Init => {
            let ws = Workspace::init(&cwd)?;
            // Take the first snapshot now so the first tracked command is fast.
            let manifest = snapshot::snapshot(&ws, None, &Ignore::load(&ws))?;
            write_json(&ws.index_path(), &manifest)?;
            println!("tracking {} ({} files)", ws.root.display(), manifest.files.len());
        }
        Cmd::Run { shell, argv } => {
            let ws = require_workspace(&cwd)?;
            let (label, mut child) = match (shell, argv.split_first()) {
                (Some(line), _) => (line.clone(), shell_command(&line)),
                (None, Some((program, args))) => {
                    let mut c = Command::new(program);
                    c.args(args);
                    (argv.join(" "), c)
                }
                (None, None) => bail!("nothing to run: pass a program, or -c \"<shell string>\""),
            };
            begin(&ws, &cwd, &label)?;
            let status = child.status();
            // Record even when the command failed to start or exited non-zero.
            let code = status.as_ref().ok().and_then(|s| s.code());
            end(&ws, code)?;
            let status = status.with_context(|| format!("could not start `{label}`"))?;
            std::process::exit(status.code().unwrap_or(1));
        }
        Cmd::Undo { id, force } => replay(&require_workspace(&cwd)?, id, true, force)?,
        Cmd::Redo { id, force } => replay(&require_workspace(&cwd)?, id, false, force)?,
        Cmd::Log { n } => log(&require_workspace(&cwd)?, n)?,
        Cmd::Show { id } => show(&require_workspace(&cwd)?, id)?,
        // The plumbing commands stay silent outside a workspace, so a global
        // shell hook costs nothing in folders that are not tracked.
        Cmd::Begin { command } => {
            if let Some(ws) = Workspace::find(&cwd) {
                begin(&ws, &cwd, &command)?;
            }
        }
        Cmd::End { exit } => {
            if let Some(ws) = Workspace::find(&cwd) {
                end(&ws, exit)?;
            }
        }
        Cmd::Hook { phase } => {
            // A hook must never block the agent, so failures are reported but not fatal.
            if let Err(err) = hook(phase) {
                eprintln!("unrun: hook failed: {err:#}");
            }
        }
        Cmd::ShellInit { shell: Shell::Pwsh } => print!("{PWSH_INIT}"),
    }
    Ok(())
}

fn require_workspace(cwd: &Path) -> Result<Workspace> {
    Workspace::find(cwd).context("not inside a tracked folder (run `unrun init` first)")
}

fn shell_command(line: &str) -> Command {
    if cfg!(windows) {
        // Prefer PowerShell 7, fall back to the Windows PowerShell that ships with the OS.
        let has_pwsh = Command::new("pwsh").arg("-Version").output().is_ok();
        let mut c = Command::new(if has_pwsh { "pwsh" } else { "powershell" });
        c.args(["-NoProfile", "-Command", line]);
        c
    } else {
        let mut c = Command::new("sh");
        c.args(["-c", line]);
        c
    }
}

fn load_index(ws: &Workspace) -> Option<Manifest> {
    read_json(&ws.index_path()).ok()
}

fn begin(ws: &Workspace, cwd: &Path, command: &str) -> Result<()> {
    // A `begin` with no matching `end` (a killed shell, say) still gets recorded.
    if ws.pending_path().exists() {
        end(ws, None)?;
    }
    let manifest = snapshot::snapshot(ws, load_index(ws).as_ref(), &Ignore::load(ws))?;
    write_json(&ws.index_path(), &manifest)?;
    let pending = Pending {
        command: command.to_string(),
        cwd: cwd.display().to_string(),
        started_ns: now_ns(),
        manifest,
    };
    write_json(&ws.pending_path(), &pending)
}

fn end(ws: &Workspace, exit_code: Option<i32>) -> Result<()> {
    if !ws.pending_path().exists() {
        return Ok(());
    }
    let pending: Pending = read_json(&ws.pending_path())?;
    let after = snapshot::snapshot(ws, Some(&pending.manifest), &Ignore::load(ws))?;
    let diff = snapshot::diff(&pending.manifest, &after);
    write_json(&ws.index_path(), &after)?;
    std::fs::remove_file(ws.pending_path())?;

    let entry = Entry {
        id: journal::next_id(ws)?,
        command: pending.command,
        cwd: pending.cwd,
        started_ns: pending.started_ns,
        finished_ns: now_ns(),
        exit_code,
        changes: diff.changes,
        dirs_created: diff.dirs_created,
        dirs_deleted: diff.dirs_deleted,
        undone: false,
    };
    if entry.is_empty() {
        return Ok(());
    }
    journal::save(ws, &entry)?;
    eprintln!("unrun: #{} recorded {} (`unrun undo` to revert)", entry.id, entry.summary());
    Ok(())
}

fn replay(ws: &Workspace, id: Option<u64>, undo: bool, force: bool) -> Result<()> {
    let verb = if undo { "undo" } else { "redo" };
    let mut entry = match id {
        Some(id) => journal::load(ws, id)?,
        None => journal::latest(ws, !undo)?.with_context(|| format!("nothing to {verb}"))?,
    };
    if entry.undone == undo {
        bail!("#{} is {}", entry.id, if undo { "already undone" } else { "not undone" });
    }
    let applied = journal::apply(ws, &entry, undo, force)?;
    entry.undone = undo;
    journal::save(ws, &entry)?;

    println!(
        "{} #{} `{}`: {} restored, {} removed",
        if undo { "undid" } else { "redid" },
        entry.id,
        entry.command,
        applied.restored,
        applied.removed
    );
    for path in &applied.unrecoverable {
        println!("  could not restore {path}: it was over the size limit and never stored");
    }
    Ok(())
}

fn age(ns: i64) -> String {
    let secs = ((now_ns() - ns) / 1_000_000_000).max(0);
    match secs {
        0..60 => format!("{secs}s ago"),
        60..3600 => format!("{}m ago", secs / 60),
        3600..86400 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86400),
    }
}

fn log(ws: &Workspace, n: usize) -> Result<()> {
    let ids = journal::list_ids(ws)?;
    if ids.is_empty() {
        println!("no recorded commands yet");
    }
    for id in ids.into_iter().rev().take(n) {
        let e = journal::load(ws, id)?;
        let command = if e.command.is_empty() { "(unknown command)" } else { e.command.as_str() };
        let undone = if e.undone { " [undone]" } else { "" };
        println!("#{:<4} {:<8} {}{}  ({})", e.id, age(e.finished_ns), command, undone, e.summary());
    }
    Ok(())
}

fn show(ws: &Workspace, id: Option<u64>) -> Result<()> {
    let entry = match id {
        Some(id) => journal::load(ws, id)?,
        None => {
            let last = journal::list_ids(ws)?.last().copied().context("no recorded commands yet")?;
            journal::load(ws, last)?
        }
    };
    println!("#{} `{}`{}", entry.id, entry.command, if entry.undone { " [undone]" } else { "" });
    println!("in {}, {}, exit code {}", entry.cwd, age(entry.finished_ns), match entry.exit_code {
        Some(code) => code.to_string(),
        None => "unknown".to_string(),
    });
    for change in &entry.changes {
        let mark = match change.kind {
            Kind::Created => '+',
            Kind::Modified => '~',
            Kind::Deleted => '-',
        };
        println!("  {mark} {}", change.path);
    }
    for dir in &entry.dirs_created {
        println!("  + {dir}/");
    }
    for dir in &entry.dirs_deleted {
        println!("  - {dir}/");
    }
    Ok(())
}

fn hook(phase: Phase) -> Result<()> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    let input: serde_json::Value = serde_json::from_str(&input).context("parsing hook input")?;
    let cwd = match input["cwd"].as_str() {
        Some(cwd) => PathBuf::from(cwd),
        None => std::env::current_dir()?,
    };
    let Some(ws) = Workspace::find(&cwd) else { return Ok(()) };
    match phase {
        Phase::Pre => begin(&ws, &cwd, input["tool_input"]["command"].as_str().unwrap_or("")),
        Phase::Post => end(&ws, None),
    }
}
