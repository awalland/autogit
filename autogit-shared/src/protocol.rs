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
    fn test_command_trigger() {
        let cmd = Command::Trigger;
        let json = cmd.to_json().unwrap();
        assert!(json.contains("\"command\":\"trigger\""));
        assert!(json.ends_with('\n'));

        let parsed = Command::from_json(&json).unwrap();
        matches!(parsed, Command::Trigger);
    }

    #[test]
    fn test_command_status() {
        let cmd = Command::Status;
        let json = cmd.to_json().unwrap();
        assert!(json.contains("\"command\":\"status\""));

        let parsed = Command::from_json(&json).unwrap();
        matches!(parsed, Command::Status);
    }

    #[test]
    fn test_command_ping() {
        let cmd = Command::Ping;
        let json = cmd.to_json().unwrap();
        assert!(json.contains("\"command\":\"ping\""));

        let parsed = Command::from_json(&json).unwrap();
        matches!(parsed, Command::Ping);
    }

    #[test]
    fn test_command_with_whitespace() {
        let json = "  {\"command\":\"trigger\"}  \n";
        let parsed = Command::from_json(json).unwrap();
        matches!(parsed, Command::Trigger);
    }

    #[test]
    fn test_command_invalid_json() {
        let result = Command::from_json("not json");
        assert!(result.is_err());
    }

    #[test]
    fn test_command_unknown_command() {
        let json = "{\"command\":\"unknown\"}";
        let result = Command::from_json(json);
        assert!(result.is_err());
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
    fn test_response_ok() {
        let resp = Response::ok("Success");
        assert_eq!(resp.status, ResponseStatus::Ok);
        assert_eq!(resp.message, "Success");
        assert!(resp.data.is_none());
    }

    #[test]
    fn test_response_error() {
        let resp = Response::error("Something failed");
        assert_eq!(resp.status, ResponseStatus::Error);
        assert_eq!(resp.message, "Something failed");
        assert!(resp.data.is_none());
    }

    #[test]
    fn test_response_status_serialization() {
        let ok = ResponseStatus::Ok;
        let ok_json = serde_json::to_string(&ok).unwrap();
        assert_eq!(ok_json, "\"ok\"");

        let error = ResponseStatus::Error;
        let error_json = serde_json::to_string(&error).unwrap();
        assert_eq!(error_json, "\"error\"");
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

    #[test]
    fn test_response_trigger_data() {
        let data = ResponseData::Trigger {
            repos_checked: 3,
            repos_committed: 2,
            details: vec![
                RepoDetail {
                    path: PathBuf::from("/repo1"),
                    committed: true,
                    files_changed: Some(10),
                    error: None,
                },
                RepoDetail {
                    path: PathBuf::from("/repo2"),
                    committed: true,
                    files_changed: Some(5),
                    error: None,
                },
                RepoDetail {
                    path: PathBuf::from("/repo3"),
                    committed: false,
                    files_changed: None,
                    error: None,
                },
            ],
        };

        let resp = Response::ok_with_data("Done", data);
        let json = resp.to_json().unwrap();
        let parsed = Response::from_json(&json).unwrap();

        assert_eq!(parsed.status, ResponseStatus::Ok);
        if let Some(ResponseData::Trigger { repos_checked, repos_committed, details }) = parsed.data {
            assert_eq!(repos_checked, 3);
            assert_eq!(repos_committed, 2);
            assert_eq!(details.len(), 3);
            assert_eq!(details[0].committed, true);
            assert_eq!(details[0].files_changed, Some(10));
            assert_eq!(details[2].committed, false);
        } else {
            panic!("Expected Trigger data");
        }
    }

    #[test]
    fn test_response_status_data() {
        let data = ResponseData::Status {
            uptime_seconds: 3600,
            check_interval_seconds: 300,
            repositories_count: 5,
        };

        let resp = Response::ok_with_data("Status", data);
        let json = resp.to_json().unwrap();
        let parsed = Response::from_json(&json).unwrap();

        if let Some(ResponseData::Status { uptime_seconds, check_interval_seconds, repositories_count }) = parsed.data {
            assert_eq!(uptime_seconds, 3600);
            assert_eq!(check_interval_seconds, 300);
            assert_eq!(repositories_count, 5);
        } else {
            panic!("Expected Status data");
        }
    }

    #[test]
    fn test_repo_detail_with_error() {
        let detail = RepoDetail {
            path: PathBuf::from("/failed/repo"),
            committed: false,
            files_changed: None,
            error: Some("Authentication failed".to_owned()),
        };

        let json = serde_json::to_string(&detail).unwrap();
        let parsed: RepoDetail = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.committed, false);
        assert!(parsed.error.is_some());
        assert_eq!(parsed.error.unwrap(), "Authentication failed");
    }

    #[test]
    fn test_repo_detail_successful() {
        let detail = RepoDetail {
            path: PathBuf::from("/success/repo"),
            committed: true,
            files_changed: Some(3),
            error: None,
        };

        let json = serde_json::to_string(&detail).unwrap();
        let parsed: RepoDetail = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.committed, true);
        assert_eq!(parsed.files_changed, Some(3));
        assert!(parsed.error.is_none());
    }

    #[test]
    fn test_repo_detail_no_changes() {
        let detail = RepoDetail {
            path: PathBuf::from("/nochanges/repo"),
            committed: false,
            files_changed: None,
            error: None,
        };

        let json = serde_json::to_string(&detail).unwrap();
        let parsed: RepoDetail = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.committed, false);
        assert!(parsed.files_changed.is_none());
        assert!(parsed.error.is_none());
    }

    #[test]
    fn test_response_roundtrip() {
        let responses = vec![
            Response::ok("Success"),
            Response::error("Failed"),
            Response::ok_with_data(
                "Triggered",
                ResponseData::Trigger {
                    repos_checked: 0,
                    repos_committed: 0,
                    details: vec![],
                },
            ),
            Response::ok_with_data(
                "Status",
                ResponseData::Status {
                    uptime_seconds: 0,
                    check_interval_seconds: 60,
                    repositories_count: 0,
                },
            ),
        ];

        for original in responses {
            let json = original.to_json().unwrap();
            let parsed = Response::from_json(&json).unwrap();

            assert_eq!(parsed.status, original.status);
            assert_eq!(parsed.message, original.message);
        }
    }

    #[test]
    fn test_response_with_empty_details() {
        let data = ResponseData::Trigger {
            repos_checked: 0,
            repos_committed: 0,
            details: vec![],
        };

        let resp = Response::ok_with_data("No repos", data);
        let json = resp.to_json().unwrap();
        let parsed = Response::from_json(&json).unwrap();

        if let Some(ResponseData::Trigger { details, .. }) = parsed.data {
            assert_eq!(details.len(), 0);
        } else {
            panic!("Expected Trigger data");
        }
    }

    #[test]
    fn test_response_invalid_json() {
        let result = Response::from_json("invalid json");
        assert!(result.is_err());
    }

    #[test]
    fn test_response_with_newline() {
        let resp = Response::ok("Test");
        let json = resp.to_json().unwrap();
        assert!(json.ends_with('\n'));

        // Should still parse with newline
        let parsed = Response::from_json(&json).unwrap();
        assert_eq!(parsed.message, "Test");
    }

    #[test]
    fn test_command_all_variants() {
        let commands = vec![
            Command::Trigger,
            Command::Status,
            Command::Ping,
        ];

        for cmd in commands {
            let json = cmd.to_json().unwrap();
            let parsed = Command::from_json(&json).unwrap();

            // Verify round-trip works
            let json2 = parsed.to_json().unwrap();
            assert_eq!(json, json2);
        }
    }
}
