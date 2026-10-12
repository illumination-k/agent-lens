//! Writing a finished result to stdout.
//!
//! stdout carries protocol data and analyzer results only; every
//! subcommand under [`super`] ends here, which is why this sits below
//! them rather than in the `cli` root that dispatches to them.

use std::io::{self, Write as _};

pub(super) fn write_stdout_line(report: &str) -> Result<(), Box<dyn std::error::Error>> {
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    stdout.write_all(report.as_bytes())?;
    if !report.ends_with('\n') {
        stdout.write_all(b"\n")?;
    }
    Ok(())
}

pub(super) fn write_stdout_json<T: serde::Serialize>(
    value: &T,
) -> Result<(), Box<dyn std::error::Error>> {
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    serde_json::to_writer(&mut stdout, value)?;
    stdout.write_all(b"\n")?;
    Ok(())
}
