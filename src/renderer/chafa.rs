use std::process::Command;

use anyhow::{Context, Result, anyhow};

pub fn is_available() -> bool {
    which::which("chafa").is_ok()
}

pub fn preflight() -> Result<String> {
    let path = which::which("chafa").context("chafa executable was not found")?;
    let output = Command::new(path)
        .arg("--version")
        .output()
        .context("failed to run chafa --version")?;
    if !output.status.success() {
        return Err(anyhow!("chafa --version exited with {}", output.status));
    }

    let version = String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .unwrap_or("chafa")
        .to_owned();
    Ok(version)
}
