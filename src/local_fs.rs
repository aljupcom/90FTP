use std::fs;
use std::path::Path;
use std::time::SystemTime;
use chrono::{DateTime, Utc};
use crate::models::FileItem;

pub fn get_initial_dir() -> String {
    if let Ok(profile) = std::env::var("USERPROFILE") {
        let desktop = Path::new(&profile).join("Desktop");
        if desktop.exists() {
            return desktop.to_string_lossy().to_string();
        }
    }
    std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| "C:\\".to_string())
}

#[cfg(target_os = "windows")]
unsafe extern "system" {
    fn GetLogicalDrives() -> u32;
}

pub fn list_drives() -> Vec<String> {
    let mut drives = Vec::new();
    #[cfg(target_os = "windows")]
    {
        let mask = unsafe { GetLogicalDrives() };
        for i in 0..26 {
            if (mask & (1 << i)) != 0 {
                let letter = (b'A' + i as u8) as char;
                drives.push(format!("{}:\\", letter));
            }
        }
    }
    if drives.is_empty() {
        drives.push("C:\\".to_string());
    }
    drives
}

pub fn list_dir(dir_path: &str) -> Result<Vec<FileItem>, String> {
    let path = Path::new(dir_path);
    if !path.exists() {
        return Err(format!("Path does not exist: {}", dir_path));
    }
    if !path.is_dir() {
        return Err(format!("Path is not a directory: {}", dir_path));
    }

    let entries = fs::read_dir(path).map_err(|e| format!("Failed to read directory: {}", e))?;
    let mut items = Vec::new();

    for entry in entries.flatten() {
        let entry_path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        let name_lower = name.to_lowercase();
        // Ignore application shortcuts (.lnk, .url) as requested
        if name_lower.ends_with(".lnk") || name_lower.ends_with(".url") {
            continue;
        }
        
        // Skip hidden files starting with . if needed or include
        let metadata = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };

        let is_dir = metadata.is_dir();
        let size = if is_dir { 0 } else { metadata.len() };
        let is_symlink = metadata.file_type().is_symlink();

        let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        let dt: DateTime<Utc> = modified.into();
        let modified_str = dt.format("%Y-%m-%d %H:%M").to_string();

        let permissions = if metadata.permissions().readonly() {
            "r--r--r--".to_string()
        } else {
            "rw-rw-rw-".to_string()
        };

        items.push(FileItem {
            name,
            path: entry_path.to_string_lossy().to_string(),
            is_dir,
            size,
            modified: modified_str,
            permissions,
            is_symlink,
        });
    }

    // Sort: directories first, then alphabetical case-insensitive
    items.sort_by(|a, b| {
        if a.is_dir != b.is_dir {
            b.is_dir.cmp(&a.is_dir)
        } else {
            a.name.to_lowercase().cmp(&b.name.to_lowercase())
        }
    });

    Ok(items)
}

pub fn read_file(path_str: &str) -> Result<String, String> {
    fs::read_to_string(path_str).map_err(|e| format!("Failed to read file: {}", e))
}

pub fn write_file(path_str: &str, content: &str) -> Result<(), String> {
    fs::write(path_str, content).map_err(|e| format!("Failed to write file: {}", e))
}

pub fn create_directory(path_str: &str) -> Result<(), String> {
    fs::create_dir_all(path_str).map_err(|e| format!("Failed to create folder: {}", e))
}

pub fn create_empty_file(path_str: &str) -> Result<(), String> {
    if Path::new(path_str).exists() {
        return Err("File already exists".to_string());
    }
    fs::write(path_str, "").map_err(|e| format!("Failed to create file: {}", e))
}

pub fn delete_path(path_str: &str) -> Result<(), String> {
    let path = Path::new(path_str);
    if path.is_dir() {
        fs::remove_dir_all(path).map_err(|e| format!("Failed to delete folder: {}", e))
    } else {
        fs::remove_file(path).map_err(|e| format!("Failed to delete file: {}", e))
    }
}

pub fn rename_path(old_str: &str, new_str: &str) -> Result<(), String> {
    fs::rename(old_str, new_str).map_err(|e| format!("Failed to rename: {}", e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_desktop_listing() {
        let dir = get_initial_dir();
        println!("Initial dir is: {}", dir);
        match list_dir(&dir) {
            Ok(files) => {
                println!("Desktop listing succeeded! Found {} files.", files.len());
                for f in files.iter().take(5) {
                    println!("File: {} (dir={})", f.name, f.is_dir);
                }
            }
            Err(e) => {
                panic!("Desktop listing FAILED: {}", e);
            }
        }
    }
}
