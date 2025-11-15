use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use anyhow::{Context, Result};

/// Get the path to the daemon Unix domain socket
pub fn socket_path() -> Result<PathBuf> {
    let config_dir = dirs::config_dir()
        .context("Could not determine config directory")?;

    Ok(config_dir.join("autogit").join("daemon.sock"))
}

/// Commands that can be sent to the daemon via socket
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Command {
    /// Request the daemon to immediately check and commit all repositories
    Trigger,
    /// Request daemon status information
    Status,
    /// Ping the daemon to check if it's alive
    Ping,
}

/// Response from the daemon
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    /// Whether the command succeeded
    pub status: ResponseStatus,
    /// Human-readable message
    pub message: String,
    /// Optional detailed data (for trigger command)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<ResponseData>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ResponseStatus {
    Ok,
    Error,
}

/// Additional data returned with specific commands
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseData {
    /// Data from a trigger command
    Trigger {
        repos_checked: usize,
        repos_committed: usize,
        details: Vec<RepoDetail>,
    },
    /// Data from a status command
    Status {
        uptime_seconds: u64,
        check_interval_seconds: u64,
        repositories_count: usize,
    },
}

/// Details about a single repository check
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoDetail {
    pub path: PathBuf,
    pub committed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files_changed: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Response {
    /// Create a successful response
    pub fn ok(message: impl Into<String>) -> Self {
        Self {
            status: ResponseStatus::Ok,
            message: message.into(),
            data: None,
        }
    }

    /// Create a successful response with data
    pub fn ok_with_data(message: impl Into<String>, data: ResponseData) -> Self {
        Self {
            status: ResponseStatus::Ok,
            message: message.into(),
            data: Some(data),
        }
    }

    /// Create an error response
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            status: ResponseStatus::Error,
            message: message.into(),
            data: None,
        }
    }

    /// Convert response to JSON string (with newline for line-delimited protocol)
    pub fn to_json(&self) -> Result<String> {
        let mut json = serde_json::to_string(self)
            .context("Failed to serialize response")?;
        json.push('\n');
        Ok(json)
    }

    /// Parse response from JSON line
    pub fn from_json(line: &str) -> Result<Self> {
        serde_json::from_str(line.trim())
            .context("Failed to parse response")
    }
}

impl Command {
    /// Convert command to JSON string (with newline for line-delimited protocol)
    pub fn to_json(&self) -> Result<String> {
        let mut json = serde_json::to_string(self)
            .context("Failed to serialize command")?;
        json.push('\n');
        Ok(json)
    }

    /// Parse command from JSON line
    pub fn from_json(line: &str) -> Result<Self> {
        serde_json::from_str(line.trim())
            .context("Failed to parse command")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_command_serialization() {
        let cmd = Command::Trigger;
        let json = cmd.to_json().unwrap();
        assert_eq!(json, "{\"command\":\"trigger\"}\n");

        let parsed = Command::from_json(&json).unwrap();
        match parsed {
            Command::Trigger => {},
            _ => panic!("Expected Trigger command"),
        }
    }

    #[test]
    fn test_response_serialization() {
        let resp = Response::ok("Test message");
        let json = resp.to_json().unwrap();

        let parsed = Response::from_json(&json).unwrap();
        assert_eq!(parsed.status, ResponseStatus::Ok);
        assert_eq!(parsed.message, "Test message");
        assert!(parsed.data.is_none());
    }

    #[test]
    fn test_response_with_data() {
        let resp = Response::ok_with_data(
            "Triggered check cycle",
            ResponseData::Trigger {
                repos_checked: 2,
                repos_committed: 1,
                details: vec![
                    RepoDetail {
                        path: PathBuf::from("/test/repo1"),
                        committed: true,
                        files_changed: Some(5),
                        error: None,
                    },
                    RepoDetail {
                        path: PathBuf::from("/test/repo2"),
                        committed: false,
                        files_changed: None,
                        error: None,
                    },
                ],
            },
        );

        let json = resp.to_json().unwrap();
        let parsed = Response::from_json(&json).unwrap();

        assert_eq!(parsed.status, ResponseStatus::Ok);
        assert!(parsed.data.is_some());
    }
}
