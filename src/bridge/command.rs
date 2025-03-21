#[cfg(windows)]
use std::os::windows::process::CommandExt;

use std::process::{Command as StdCommand, Stdio};

use anyhow::{bail, Result};
use log::debug;
use regex::Regex;
use tokio::process::Command as TokioCommand;

use crate::{cmd_line::CmdLineSettings, settings::*};

pub fn create_nvim_command(settings: &Settings) -> Result<TokioCommand> {
    let mut cmd = build_nvim_cmd(settings)?;

    debug!("Starting neovim with: {:?}", cmd);

    #[cfg(not(debug_assertions))]
    cmd.stderr(Stdio::piped());

    #[cfg(debug_assertions)]
    cmd.stderr(Stdio::inherit());

    #[cfg(windows)]
    cmd.creation_flags(windows::Win32::System::Threading::CREATE_NO_WINDOW.0);

    Ok(cmd)
}

fn build_nvim_cmd(settings: &Settings) -> Result<TokioCommand> {
    if let Some(cmdline) = settings.get::<CmdLineSettings>().neovim_bin {
        if let Some((bin, args)) = lex_nvim_cmdline(&cmdline, settings)? {
            return Ok(build_nvim_cmd_with_args(bin, args, settings));
        }

        bail!("ERROR: NEOVIM_BIN='{}' was not found.", cmdline);
    } else if let Some(path) = platform_which("nvim", settings) {
        if neovim_ok(&path, &[], settings)? {
            return Ok(build_nvim_cmd_with_args(path, vec![], settings));
        }
    }

    bail!("ERROR: nvim not found!")
}

#[cfg(target_os = "macos")]
fn build_login_cmd_args(command: &str, args: &[&str]) -> (String, Vec<String>) {
    use std::env;

    use crate::error_handling::ResultPanicExplanation;

    // If $TERM is set, we assume user is running from a terminal, and we shouldn't
    // re-initialize the environment.
    // See https://github.com/neovide/neovide/issues/2584
    if env::var_os("TERM").is_some() {
        return (
            command.to_string(),
            args.iter().map(|s| s.to_string()).collect(),
        );
    }

    let user = env::var("USER").unwrap_or_explained_panic("USER environment variable not found");
    let shell = env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
    let shell_name = shell.split('/').next_back().unwrap_or("zsh");

    let args = match shlex::try_join(args.iter().map(|s| (*s) as &str)) {
        Ok(args) => args,
        Err(_) => panic!("Failed to join arguments"),
    };

    // Executes neovim as a login shell, so it will source the user's startup files.
    let exec = format!(
        "exec -a -{} {} -c '{} {}'",
        shell_name, shell, command, args
    );

    // See "man login". It sets up some important env vars like $PATH and $HOME.
    // On macOS, use the `login` command so it will appear as a tty session.
    let cmd_path = "/usr/bin/login";

    // We use a special flag to tell login not to prompt us for a password, because we're
    // going to spawn it as the current user anyway. The addition of "p",
    // preserves the environment.
    // -f: Bypasses authentication for the already-logged-in user.
    // -l: Skips changing directory to $HOME and prepending '-' to argv[0].
    // -p: Preserves the environment.
    // -q: Forces quiet logins, as if a .hushlogin is present.
    let cmd_args = vec!["-flpq", &user, "/bin/zsh", "-fc", &exec];

    (
        cmd_path.to_string(),
        cmd_args.into_iter().map(|s| s.to_string()).collect(),
    )
}

// Creates a shell command if needed on this platform
#[cfg(target_os = "macos")]
fn create_platform_shell_command(command: &str, args: &[&str], _settings: &Settings) -> StdCommand {
    let (cmd, cmd_args) = build_login_cmd_args(command, args);

    let mut result = StdCommand::new(cmd);
    result.args(cmd_args);

    result
}

// Creates a shell command if needed on this platform
#[cfg(target_os = "linux")]
fn create_platform_shell_command(command: &str, args: &[&str], _settings: &Settings) -> StdCommand {
    let mut result = StdCommand::new(command);
    result.args(args);

    result
}

// Creates a shell command if needed on this platform
#[cfg(target_os = "windows")]
fn create_platform_shell_command(command: &str, args: &[&str], settings: &Settings) -> StdCommand {
    if settings.get::<CmdLineSettings>().wsl {
        let mut result = StdCommand::new("wsl");
        result.args(["$SHELL", "-lc"]);
        result.arg(format!("{} {}", command, args.join(" ")));

        result.creation_flags(windows::Win32::System::Threading::CREATE_NO_WINDOW.0);

        result
    } else {
        let mut result = StdCommand::new(command);
        result.args(args);

        result.creation_flags(windows::Win32::System::Threading::CREATE_NO_WINDOW.0);

        result
    }
}

fn create_error_message(bin: &str, stdout: &str, stderr: Vec<&str>, is_wsl: bool) -> String {
    let mut error_message = format!(
        concat!(
            "ERROR: Unexpected output from neovim binary:\n",
            "\t{bin} -v\n",
            "stdout: {stdout}\n",
            "stderr: {stderr}\n",
            "\t"
        ),
        bin = bin,
        stdout = stdout,
        stderr = stderr.join("\n")
    );

    if is_wsl {
        error_message.push_str("\n\nPlease check your WSL configuration.\n");
    } else {
        error_message.push_str("\n\nPlease check your shell configuration.\n");
    }

    error_message
}

fn neovim_ok(bin: &str, args: &[String], settings: &Settings) -> Result<bool> {
    let is_wsl = settings.get::<CmdLineSettings>().wsl;
    let mut args = args.iter().map(String::as_str).collect::<Vec<_>>();
    args.push("-v");

    let mut cmd = create_platform_shell_command(bin, &args, settings);
    let Ok(output) = cmd.output() else {
        return Ok(false);
    };

    // The output is not utf8 on Windows and can contain special characters.
    // But a lossy conversion is OK for our purposes
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    let error_regex = Regex::new(r"your \d+x\d+ screen size is bogus. expect trouble").unwrap();
    let (_, non_matching_stderr_lines): (Vec<_>, Vec<_>) =
        stderr.lines().partition(|line| error_regex.is_match(line));

    let unexpected_output = !output.status.success()
        || !stdout.starts_with("NVIM v")
        || !non_matching_stderr_lines.is_empty();

    if unexpected_output {
        let error_message = create_error_message(bin, &stdout, non_matching_stderr_lines, is_wsl);
        let command = if is_wsl {
            format!("wsl --shell-type login -- {bin} -v")
        } else {
            format!("$SHELL -lc '{bin} -v'")
        };

        bail!("{error_message}{command}")
    }

    Ok(true)
}

fn lex_nvim_cmdline(cmdline: &str, settings: &Settings) -> Result<Option<(String, Vec<String>)>> {
    let is_windows = cfg!(target_os = "windows") && !settings.get::<CmdLineSettings>().wsl;
    // shlex::split does not work with windows path separators, so pass the cmdline as it is
    // Note that on WSL we can still try to split it to support specifying neovim-bin as
    // /usr/bin/env nvim for example
    if is_windows {
        Some((cmdline.to_owned(), Vec::new()))
    } else {
        shlex::split(cmdline)
            .filter(|t| !t.is_empty())
            .map(|mut tokens| (tokens.remove(0), tokens))
    }
    .and_then(|(bin, args)| {
        // if neovim_bin contains a path separator, then try to launch it directly
        // otherwise use which to find the full path
        if !bin.contains('/') && !bin.contains('\\') {
            platform_which(&bin, settings).map(|bin| (bin, args))
        } else {
            Some((bin, args))
        }
    })
    .map_or(Ok(None), |(bin, args)| {
        neovim_ok(&bin, &args, settings).map(|res| res.then_some((bin, args)))
    })
}

fn platform_which(bin: &str, settings: &Settings) -> Option<String> {
    let is_wsl = settings.get::<CmdLineSettings>().wsl;

    // The which crate won't work in WSL, a shell always needs to be started
    // In all other cases always try which::which first to avoid shell specific problems
    if !is_wsl {
        if let Ok(path) = which::which(bin) {
            return path.into_os_string().into_string().ok();
        }
    }

    // But if that does not work, try the shell anyway
    let mut which_command = create_platform_shell_command("which", &[bin], settings);
    debug!("Running which command: {:?}", which_command);
    if let Ok(output) = which_command.output() {
        if output.status.success() {
            // The output is not utf8 on Windows and can contain special characters.
            // This might fail with special characters in the path, but that probably does
            // not matter, since which::which should handle almost all cases.
            let nvim_path = String::from_utf8_lossy(&output.stdout);
            return Some(nvim_path.trim().to_owned());
        }
    }
    None
}

#[cfg(target_os = "macos")]
fn nvim_cmd_impl(bin: String, args: Vec<String>, _settings: &Settings) -> TokioCommand {
    let (cmd, cmd_args) = build_login_cmd_args(
        &bin,
        &args.iter().map(|s| s.as_str()).collect::<Vec<&str>>(),
    );

    let mut cmd = TokioCommand::new(cmd);
    cmd.args(cmd_args);

    cmd
}

#[cfg(not(target_os = "macos"))]
fn nvim_cmd_impl(bin: String, args: Vec<String>, settings: &Settings) -> TokioCommand {
    if cfg!(target_os = "windows") && settings.get::<CmdLineSettings>().wsl {
        let mut cmd = TokioCommand::new("wsl");
        cmd.args(["--shell-type", "login"]);
        cmd.arg("--");
        cmd.arg(bin);
        cmd.args(args);
        cmd
    } else {
        let mut cmd = TokioCommand::new(bin);
        cmd.args(args);
        cmd
    }
}

fn build_nvim_cmd_with_args(
    bin: String,
    mut args: Vec<String>,
    settings: &Settings,
) -> TokioCommand {
    args.push("--embed".to_string());
    args.extend(settings.get::<CmdLineSettings>().neovim_args);
    nvim_cmd_impl(bin, args, settings)
}
