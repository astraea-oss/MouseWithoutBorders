use tokio::process::Command;

use crate::{LinuxInputError, Result};

#[derive(Debug)]
pub struct HyprlandCursorController {
    original_invisible: bool,
    current_invisible: bool,
}

impl HyprlandCursorController {
    pub async fn connect() -> Result<Self> {
        let output = Command::new("hyprctl")
            .arg("getoption")
            .arg("cursor:invisible")
            .output()
            .await?;
        if !output.status.success() {
            return Err(command_error(
                "hyprctl getoption cursor:invisible",
                &output.stderr,
            ));
        }

        let text = String::from_utf8_lossy(&output.stdout);
        let original_invisible =
            parse_invisible_option(&text).ok_or_else(|| LinuxInputError::CommandFailed {
                program: "hyprctl getoption cursor:invisible".to_string(),
                message: format!("unexpected output: {text:?}"),
            })?;
        Ok(Self {
            original_invisible,
            current_invisible: original_invisible,
        })
    }

    pub async fn hide(&mut self) -> Result<()> {
        self.set_invisible(true).await
    }

    pub async fn show(&mut self) -> Result<()> {
        self.restore().await
    }

    pub async fn restore(&mut self) -> Result<()> {
        self.set_invisible(self.original_invisible).await
    }

    pub fn is_hidden(&self) -> bool {
        self.current_invisible
    }

    async fn set_invisible(&mut self, invisible: bool) -> Result<()> {
        if self.current_invisible == invisible {
            return Ok(());
        }

        let value = if invisible { "true" } else { "false" };
        let output = Command::new("hyprctl")
            .arg("keyword")
            .arg("cursor:invisible")
            .arg(value)
            .output()
            .await?;
        if !output.status.success() {
            return Err(command_error(
                "hyprctl keyword cursor:invisible",
                &output.stderr,
            ));
        }
        self.current_invisible = invisible;
        Ok(())
    }
}

impl Drop for HyprlandCursorController {
    fn drop(&mut self) {
        if self.current_invisible == self.original_invisible {
            return;
        }
        let value = if self.original_invisible {
            "true"
        } else {
            "false"
        };
        let _ = std::process::Command::new("hyprctl")
            .arg("keyword")
            .arg("cursor:invisible")
            .arg(value)
            .status();
    }
}

fn command_error(program: &str, stderr: &[u8]) -> LinuxInputError {
    LinuxInputError::CommandFailed {
        program: program.to_string(),
        message: String::from_utf8_lossy(stderr).trim().to_string(),
    }
}

fn parse_invisible_option(text: &str) -> Option<bool> {
    text.lines().find_map(|line| {
        let value = line.trim().strip_prefix("int:")?.trim();
        match value {
            "0" => Some(false),
            "1" => Some(true),
            _ => None,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hyprland_integer_option() {
        assert_eq!(parse_invisible_option("int: 0\nset: false\n"), Some(false));
        assert_eq!(parse_invisible_option("int: 1\nset: true\n"), Some(true));
        assert_eq!(parse_invisible_option("set: false\n"), None);
    }
}
