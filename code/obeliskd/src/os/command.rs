//! Running the OS's own tools (`ip`, `docker`, `ifconfig`) and turning failures into errors.

use std::process::Command;

use crate::ObeliskdError;

/// What a finished command printed.
pub struct Output {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Runs `program args…`, capturing its output.
///
/// # Errors
///
/// [`ObeliskdError::Os`] if it can't be started at all (e.g. not installed).
pub fn run(program: &str, args: &[String]) -> Result<Output, ObeliskdError> {
    tracing::debug!(command = %describe(program, args), "running");
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|e| ObeliskdError::Os {
            command: describe(program, args),
            message: format!("couldn't run it: {e}"),
        })?;
    Ok(Output {
        success: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
    })
}

/// Runs `program args…` on this terminal (its output isn't captured), e.g. a log viewer.
///
/// # Errors
///
/// [`ObeliskdError::Os`] if it can't be started, or exits with a failure.
pub fn run_attached(program: &str, args: &[String]) -> Result<(), ObeliskdError> {
    let status = Command::new(program)
        .args(args)
        .status()
        .map_err(|e| ObeliskdError::Os {
            command: describe(program, args),
            message: format!("couldn't run it: {e}"),
        })?;
    if status.success() {
        Ok(())
    } else {
        Err(ObeliskdError::Os {
            command: describe(program, args),
            message: format!("exited with {status}"),
        })
    }
}

/// Runs it and requires success, treating stderr containing any of `ok_if` as success too (e.g.
/// "already gone" for a removal).
///
/// # Errors
///
/// [`ObeliskdError::Os`] with the command and its stderr.
pub fn run_ok(program: &str, args: &[String], ok_if: &[&str]) -> Result<(), ObeliskdError> {
    let output = run(program, args)?;
    if output.success || ok_if.iter().any(|s| output.stderr.contains(s)) {
        Ok(())
    } else {
        Err(failed(program, args, &output))
    }
}

#[must_use]
pub fn failed(program: &str, args: &[String], output: &Output) -> ObeliskdError {
    ObeliskdError::Os {
        command: describe(program, args),
        message: output.stderr.clone(),
    }
}

fn describe(program: &str, args: &[String]) -> String {
    std::iter::once(program)
        .chain(args.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(" ")
}

/// `["a", "b"]` → owned args.
#[must_use]
pub fn args(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| (*s).to_string()).collect()
}
