//! Implements `warp edit`, a blocking editor shim suitable for use as `$EDITOR`
//! (or `$KUBE_EDITOR`, `$GIT_EDITOR`, ...).
//!
//! Tools like `kubectl edit` write a resource to a temporary file, spawn
//! `$EDITOR <file>`, wait for that process to exit, and then read the file back
//! to decide what to apply. To let Warp's built-in code editor play that role we
//! need a small process that:
//!
//! 1. asks the Warp instance that owns this PTY to open the file, and
//! 2. blocks until the user is done editing it.
//!
//! Step 1 is done by writing a Warp shell hook (an OSC escape sequence) to the
//! controlling terminal — the same channel the shell integration already uses,
//! which means it automatically reaches the right window, tab and pane.
//!
//! Step 2 is done with two marker files in the system temp directory, whose
//! paths are handed to Warp in the hook payload:
//!
//! * `<base>.ack` — written by Warp as soon as it has accepted the request. Its
//!   absence after [`ACK_TIMEOUT`] means nothing is listening (an old client, a
//!   plain `xterm`, output being piped somewhere), so we fall back to a real
//!   editor instead of hanging forever.
//! * `<base>.done` — written by Warp once the editor tab for the file is closed.
//!   Its contents are the exit code we should report to the calling tool.
//!
//! Marker files are used rather than a socket because they are the only IPC
//! primitive that behaves identically on macOS, Linux and Windows without
//! pulling a runtime into this code path, which runs before any of the app is
//! initialized.

use std::{
    ffi::OsString,
    fs,
    io::{IsTerminal as _, Read, Write as _},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context as _, Result, anyhow};
use instant::Instant;

/// Environment variable Warp sets on local shell sessions it spawned. Its
/// presence is what tells us the built-in editor is reachable over this PTY.
const LOCAL_SESSION_ENV: &str = "WARP_IS_LOCAL_SHELL_SESSION";

/// Editor to fall back to when the built-in editor is not reachable, defaulting
/// to [`DEFAULT_FALLBACK_EDITOR`]. Users who set `EDITOR="warp edit"` globally
/// can point this at their real editor so that SSH sessions, `tmux` on a remote
/// host, and non-Warp terminals keep working.
const FALLBACK_EDITOR_ENV: &str = "WARP_EDIT_FALLBACK_EDITOR";

/// Editor used when [`FALLBACK_EDITOR_ENV`] is not set.
#[cfg(not(windows))]
const DEFAULT_FALLBACK_EDITOR: &str = "vi";
#[cfg(windows)]
const DEFAULT_FALLBACK_EDITOR: &str = "notepad";

/// How long to wait for Warp to acknowledge the request before assuming nothing
/// is listening and falling back to [`FALLBACK_EDITOR_ENV`].
const ACK_TIMEOUT: Duration = Duration::from_secs(10);

/// How often the marker files are checked.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Path argument that means "the contents are piped in on stdin" rather than
/// naming a file, following the convention of `code -`, `vim -` and friends.
const STDIN_PATH: &str = "-";

#[derive(Debug, Clone, clap::Args)]
pub struct EditArgs {
    /// Path of the file to open in Warp's built-in editor, or `-` to open what
    /// is piped in on stdin (`kubectl get pods | warp edit -`).
    pub path: PathBuf,

    /// Return as soon as the file is opened instead of waiting for the editor
    /// to be closed.
    ///
    /// Tools that read the file back after the editor exits (`kubectl edit`,
    /// `git commit`, `crontab -e`, ...) need the default blocking behavior.
    #[arg(long)]
    pub no_wait: bool,
}

/// Runs `warp edit`.
///
/// Returns the exit code to terminate the process with; callers should not
/// assume `Ok(())` means the file was edited, since a non-zero code is how a
/// cancelled edit is reported to the calling tool.
pub fn run(args: &EditArgs) -> Result<i32> {
    let input = if args.path.as_os_str() == STDIN_PATH {
        EditInput::Stdin(stdin_to_temp_file()?)
    } else {
        EditInput::File(
            absolute_path(&args.path)
                .with_context(|| format!("failed to resolve path {}", args.path.display()))?,
        )
    };
    let path = input.path();

    if !in_warp_local_session() {
        return run_fallback_editor(path, &input);
    }

    // Piped input has no calling tool waiting to read the file back, and the
    // shell is holding up the rest of the pipeline until we exit, so there is
    // nothing to block for. `code -` behaves the same way.
    let wait = !args.no_wait && !input.is_stdin();

    let markers = MarkerPaths::new();
    let request = EditRequest {
        path: path.to_string_lossy().into_owned(),
        // Empty for now: only files on the machine running the Warp client can
        // be opened. The field is part of the wire format so that remote
        // sessions can be added without a protocol change.
        host: String::new(),
        ack_path: markers.ack.to_string_lossy().into_owned(),
        done_path: markers.done.to_string_lossy().into_owned(),
        wait,
    };

    if let Err(err) = write_hook_to_terminal(&request) {
        log_fallback(&format!("could not reach the terminal ({err:#})"));
        return run_fallback_editor(path, &input);
    }

    if !wait_for_marker(&markers.ack, Some(ACK_TIMEOUT)) {
        log_fallback("Warp did not respond");
        return run_fallback_editor(path, &input);
    }
    let _ = fs::remove_file(&markers.ack);

    if !wait {
        let _ = fs::remove_file(&markers.done);
        return Ok(0);
    }

    // Warp is holding the file open; wait for the editor tab to be closed. No
    // timeout here — the user may take arbitrarily long, and if Warp dies the
    // PTY dies with it, taking this process along.
    wait_for_marker(&markers.done, None);
    let exit_code = read_exit_code(&markers.done);
    let _ = fs::remove_file(&markers.done);

    Ok(exit_code)
}

/// Where the contents being edited came from.
enum EditInput {
    /// A file the caller named, which is what tools using us as `$EDITOR` do.
    File(PathBuf),
    /// Data piped in on stdin, spooled to this temporary file.
    Stdin(PathBuf),
}

impl EditInput {
    fn path(&self) -> &Path {
        match self {
            Self::File(path) | Self::Stdin(path) => path,
        }
    }

    fn is_stdin(&self) -> bool {
        matches!(self, Self::Stdin(_))
    }
}

/// Spools stdin into a temporary file, since the editor opens paths rather than
/// streams.
///
/// The file is deliberately left behind. Nothing waits for the editor to be
/// closed in this mode, so there is no point at which deleting it would be safe,
/// and the buffer stays attached to it for saving. It goes in the system temp
/// directory, which the OS cleans on its own schedule.
fn stdin_to_temp_file() -> Result<PathBuf> {
    if std::io::stdin().is_terminal() {
        return Err(anyhow!(
            "'-' means read the contents from stdin, but stdin is a terminal; \
             pipe something in, e.g. `kubectl get pods | warp edit -`"
        ));
    }

    spool_to_temp_file(&mut std::io::stdin().lock())
}

/// Reads `reader` to the end and writes it to a fresh file in the temp
/// directory, returning that path.
fn spool_to_temp_file(reader: &mut impl Read) -> Result<PathBuf> {
    let mut contents = Vec::new();
    reader
        .read_to_end(&mut contents)
        .context("failed to read stdin")?;

    let path = std::env::temp_dir().join(format!("warp-stdin-{}.txt", uuid::Uuid::new_v4()));
    fs::write(&path, contents)
        .with_context(|| format!("failed to write stdin to {}", path.display()))?;

    Ok(path)
}

/// The payload of the `EditFile` shell hook.
///
/// Field names must stay in sync with `EditFileValue` in
/// `app/src/terminal/model/ansi/dcs_hooks.rs`; the client side has a test that
/// parses [`Self::escape_sequence`] to keep the two honest.
#[derive(Debug, Clone, serde::Serialize)]
pub struct EditRequest {
    /// Absolute path of the file to edit.
    pub path: String,
    /// Host the file lives on, or empty for the machine running the client.
    pub host: String,
    /// Marker file the client creates once it has accepted the request.
    pub ack_path: String,
    /// Marker file the client creates once the user is done editing.
    pub done_path: String,
    /// Whether the caller is blocked waiting for the edit to finish.
    pub wait: bool,
}

impl EditRequest {
    /// Builds the escape sequence carrying this request to the terminal.
    ///
    /// The payload is hex-encoded, as Warp requires for every hook other than
    /// the handful emitted from user-visible RC file snippets: it keeps
    /// arbitrary file paths (which may contain `;`, newlines or non-ASCII
    /// bytes) from corrupting the escape sequence.
    pub fn escape_sequence(&self) -> Result<String> {
        let json = serde_json::to_string(&serde_json::json!({
            "hook": "EditFile",
            "value": self,
        }))?;

        Ok(format!("\x1b]9278;d;{}\x07", hex::encode(json)))
    }
}

/// Paths of the two marker files used to synchronize with Warp.
struct MarkerPaths {
    ack: PathBuf,
    done: PathBuf,
}

impl MarkerPaths {
    fn new() -> Self {
        let base = std::env::temp_dir().join(format!("warp-edit-{}", uuid::Uuid::new_v4()));
        Self {
            ack: base.with_extension("ack"),
            done: base.with_extension("done"),
        }
    }
}

/// Whether this process is running inside a local shell session spawned by Warp.
///
/// SSH does not forward environment variables by default, so this is false on a
/// remote host even when the outer terminal is Warp — which is what we want,
/// since the built-in editor cannot reach files on the remote filesystem.
fn in_warp_local_session() -> bool {
    std::env::var(LOCAL_SESSION_ENV).is_ok_and(|value| value == "1")
}

/// Writes `request` to the controlling terminal as a Warp shell hook.
fn write_hook_to_terminal(request: &EditRequest) -> Result<()> {
    let sequence = request.escape_sequence()?;

    // Write to the controlling terminal rather than stdout: the calling tool
    // may have redirected our stdout, and the hook is only meaningful to the
    // terminal emulator.
    let mut terminal = open_controlling_terminal()?;
    terminal.write_all(sequence.as_bytes())?;
    terminal.flush()?;
    Ok(())
}

#[cfg(unix)]
fn open_controlling_terminal() -> Result<fs::File> {
    fs::OpenOptions::new()
        .write(true)
        .open("/dev/tty")
        .context("failed to open /dev/tty")
}

#[cfg(windows)]
fn open_controlling_terminal() -> Result<fs::File> {
    fs::OpenOptions::new()
        .write(true)
        .open("CONOUT$")
        .context("failed to open CONOUT$")
}

#[cfg(not(any(unix, windows)))]
fn open_controlling_terminal() -> Result<fs::File> {
    Err(anyhow!("no controlling terminal on this platform"))
}

/// Opens the controlling terminal for reading, to give a fallback editor a
/// usable stdin when ours was a pipe.
#[cfg(unix)]
fn open_controlling_terminal_for_read() -> Result<fs::File> {
    fs::File::open("/dev/tty").context("failed to open /dev/tty")
}

#[cfg(windows)]
fn open_controlling_terminal_for_read() -> Result<fs::File> {
    fs::File::open("CONIN$").context("failed to open CONIN$")
}

#[cfg(not(any(unix, windows)))]
fn open_controlling_terminal_for_read() -> Result<fs::File> {
    Err(anyhow!("no controlling terminal on this platform"))
}

/// Blocks until `marker` exists, giving up after `timeout` if one is given.
///
/// Returns whether the marker appeared.
fn wait_for_marker(marker: &Path, timeout: Option<Duration>) -> bool {
    let started_at = Instant::now();
    loop {
        if marker.exists() {
            return true;
        }
        if timeout.is_some_and(|timeout| started_at.elapsed() >= timeout) {
            return false;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Reads the exit code Warp wrote into the done marker.
///
/// A marker that is empty or malformed is treated as success, so that a
/// truncated write can never turn a completed edit into a failure.
fn read_exit_code(marker: &Path) -> i32 {
    fs::read_to_string(marker)
        .ok()
        .and_then(|contents| contents.trim().parse::<i32>().ok())
        .unwrap_or(0)
}

/// Resolves `path` against the current directory without requiring it to exist.
fn absolute_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    Ok(std::env::current_dir()?.join(path))
}

/// Runs the editor configured in [`FALLBACK_EDITOR_ENV`] against `path`.
///
/// This is what makes `EDITOR="warp edit"` safe to set unconditionally: outside
/// a Warp session we behave like whatever editor the user actually wanted.
fn run_fallback_editor(path: &Path, input: &EditInput) -> Result<i32> {
    let editor = fallback_editor();

    // The variable holds a command line, not a bare program name ("code -w",
    // "emacsclient -nw"), matching how $EDITOR itself is interpreted.
    let mut parts = editor.split_whitespace();
    let program = parts
        .next()
        .ok_or_else(|| anyhow!("no fallback editor to run; check ${FALLBACK_EDITOR_ENV}"))?;
    let program_args: Vec<OsString> = parts.map(OsString::from).collect();

    let mut command = command::blocking::Command::new(program);
    command.args(&program_args).arg(path);

    // Our stdin is the pipe the contents came from, and it is at EOF. Handing
    // that to a terminal editor would leave it unable to read the user's
    // keystrokes, so point the child at the terminal instead.
    if input.is_stdin() {
        match open_controlling_terminal_for_read() {
            Ok(terminal) => {
                command.stdin(terminal);
            }
            Err(err) => {
                eprintln!("warp edit: could not reattach stdin to the terminal ({err:#})");
            }
        }
    }

    let status = command
        .status()
        .with_context(|| format!("failed to run fallback editor {program:?}"))?;

    Ok(status.code().unwrap_or(1))
}

/// Returns the fallback editor command line.
fn fallback_editor() -> String {
    // $EDITOR and $VISUAL are deliberately not consulted: they are the
    // variables most likely to point back at us, and following them would loop.
    std::env::var(FALLBACK_EDITOR_ENV)
        .ok()
        .filter(|editor| !editor.trim().is_empty())
        // Same last resort git uses when it cannot work out an editor. Better
        // to drop the user into an editor they did not ask for than to fail the
        // command and lose whatever they were about to write.
        .unwrap_or_else(|| DEFAULT_FALLBACK_EDITOR.to_owned())
}

fn log_fallback(reason: &str) {
    eprintln!("warp edit: {reason}; falling back to {}", fallback_editor());
}

#[cfg(test)]
#[path = "edit_tests.rs"]
mod tests;
