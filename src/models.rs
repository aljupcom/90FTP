use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileItem {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified: String,
    pub permissions: String,
    pub is_symlink: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SiteProfile {
    pub id: String,
    pub name: String,
    pub protocol: String, // "ftp", "ftps", "sftp"
    pub host: String,
    pub port: u16,
    pub username: String,
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub auth_type: Option<String>, // "password" or "key"
    #[serde(default)]
    pub key_path: Option<String>,
    #[serde(default)]
    pub passphrase: Option<String>,
    #[serde(default)]
    pub remote_dir: String,
    #[serde(default)]
    pub local_dir: String,
    #[serde(default)]
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcRequest {
    pub id: String,
    pub action: String,
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcResponse {
    pub id: String,
    pub success: bool,
    pub action: String,
    pub data: Option<serde_json::Value>,
    pub error: Option<String>,
}

impl IpcResponse {
    pub fn ok(id: &str, action: &str, data: serde_json::Value) -> Self {
        Self {
            id: id.to_string(),
            success: true,
            action: action.to_string(),
            data: Some(data),
            error: None,
        }
    }

    pub fn err(id: &str, action: &str, error: &str) -> Self {
        Self {
            id: id.to_string(),
            success: false,
            action: action.to_string(),
            data: None,
            error: Some(error.to_string()),
        }
    }
}
