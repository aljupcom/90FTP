use std::fs;
use std::path::Path;
use std::time::SystemTime;
use chrono::{DateTime, Utc};
use crate::models::FileItem;

pub fn get_desktop_dir() -> String {
    if let Ok(profile) = std::env::var("USERPROFILE") {
        let desktop = Path::new(&profile).join("Desktop");
        if desktop.exists() {
            return desktop.to_string_lossy().to_string();
        }
    }
    "C:\\".to_string()
}

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

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DriveInfo {
    pub path: String,
    pub letter: String,
    pub label: String,
    pub display: String,
    pub is_system: bool,
}

#[cfg(target_os = "windows")]
unsafe extern "system" {
    fn GetLogicalDrives() -> u32;
    fn GetVolumeInformationW(
        lpRootPathName: *const u16,
        lpVolumeNameBuffer: *mut u16,
        nVolumeNameSize: u32,
        lpVolumeSerialNumber: *mut u32,
        lpMaximumComponentLength: *mut u32,
        lpFileSystemFlags: *mut u32,
        lpFileSystemNameBuffer: *mut u16,
        nFileSystemNameSize: u32,
    ) -> i32;
    fn SetFileAttributesW(lpFileName: *const u16, dwFileAttributes: u32) -> i32;
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

pub fn list_drives_info() -> Vec<DriveInfo> {
    let mut drives = Vec::new();
    #[cfg(target_os = "windows")]
    {
        let mask = unsafe { GetLogicalDrives() };
        for i in 0..26 {
            if (mask & (1 << i)) != 0 {
                let letter = (b'A' + i as u8) as char;
                let root = format!("{}:\\", letter);
                let wide_root: Vec<u16> = root.encode_utf16().chain(std::iter::once(0)).collect();
                let mut vol_name = [0u16; 260];
                let success = unsafe {
                    GetVolumeInformationW(
                        wide_root.as_ptr(),
                        vol_name.as_mut_ptr(),
                        vol_name.len() as u32,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        0,
                    )
                };
                let raw_label = if success != 0 {
                    let len = vol_name.iter().position(|&c| c == 0).unwrap_or(vol_name.len());
                    String::from_utf16_lossy(&vol_name[..len]).trim().to_string()
                } else {
                    String::new()
                };

                let label = if !raw_label.is_empty() {
                    raw_label
                } else if letter == 'C' {
                    "Local Disk".to_string()
                } else {
                    "Drive".to_string()
                };

                let is_system = letter == 'C';
                let display = format!("{} ({}:)", label, letter);

                drives.push(DriveInfo {
                    path: root,
                    letter: format!("{}:", letter),
                    label,
                    display,
                    is_system,
                });
            }
        }
    }
    if drives.is_empty() {
        drives.push(DriveInfo {
            path: "C:\\".to_string(),
            letter: "C:".to_string(),
            label: "Local Disk".to_string(),
            display: "Local Disk (C:)".to_string(),
            is_system: true,
        });
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

fn clear_windows_special_attrs(path: &Path) {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::ffi::OsStrExt;
        const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
        let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        wide.push(0);
        unsafe {
            let _ = SetFileAttributesW(wide.as_ptr(), FILE_ATTRIBUTE_NORMAL);
        }
    }
    if let Ok(meta) = fs::symlink_metadata(path) {
        let mut perms = meta.permissions();
        if perms.readonly() {
            perms.set_readonly(false);
            let _ = fs::set_permissions(path, perms);
        }
    }
}

fn clear_windows_special_attrs_recursive(path: &Path) {
    clear_windows_special_attrs(path);
    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.flatten() {
            let child = entry.path();
            if let Ok(meta) = fs::symlink_metadata(&child) {
                if meta.file_type().is_dir() && !meta.file_type().is_symlink() {
                    clear_windows_special_attrs_recursive(&child);
                } else {
                    clear_windows_special_attrs(&child);
                }
            } else {
                clear_windows_special_attrs(&child);
            }
        }
    }
}

pub fn delete_path(path_str: &str) -> Result<(), String> {
    let path = Path::new(path_str);
    let sym_meta = fs::symlink_metadata(path)
        .map_err(|e| format!("Path not found or inaccessible ({}): {}", path_str, e))?;

    let is_real_dir = sym_meta.file_type().is_dir() && !sym_meta.file_type().is_symlink();
    if is_real_dir {
        if fs::remove_dir_all(path).is_err() {
            clear_windows_special_attrs_recursive(path);
            fs::remove_dir_all(path)
                .map_err(|e| format!("Failed to delete folder {}: {}", path_str, e))?;
        }
        Ok(())
    } else {
        clear_windows_special_attrs(path);
        if let Err(file_err) = fs::remove_file(path) {
            // On Windows, directory junctions/symlinks may require remove_dir
            if fs::remove_dir(path).is_ok() {
                return Ok(());
            }
            return Err(format!("Failed to delete file {}: {}", path_str, file_err));
        }
        Ok(())
    }
}

pub fn rename_path(old_str: &str, new_str: &str) -> Result<(), String> {
    fs::rename(old_str, new_str).map_err(|e| format!("Failed to rename: {}", e))
}

/// True when the OS error means "source and destination live on different
/// volumes". Windows reports ERROR_NOT_SAME_DEVICE (17), Unix reports EXDEV (18).
fn is_cross_device_error(e: &std::io::Error) -> bool {
    matches!(e.raw_os_error(), Some(17) | Some(18))
}

/// Recursively copy a file or a directory tree. Symlinks are copied as plain
/// files/directories (the app does not need to preserve link semantics).
pub fn copy_path(src_str: &str, dst_str: &str) -> Result<(), String> {
    let src = Path::new(src_str);
    let dst = Path::new(dst_str);

    let meta = fs::symlink_metadata(src)
        .map_err(|e| format!("Cannot read source {}: {}", src_str, e))?;

    if meta.file_type().is_dir() {
        fs::create_dir_all(dst)
            .map_err(|e| format!("Failed to create folder {}: {}", dst_str, e))?;
        let entries = fs::read_dir(src)
            .map_err(|e| format!("Failed to read folder {}: {}", src_str, e))?;
        for entry in entries.flatten() {
            let child_src = entry.path();
            let child_dst = dst.join(entry.file_name());
            copy_path(&child_src.to_string_lossy(), &child_dst.to_string_lossy())?;
        }
        Ok(())
    } else {
        if let Some(parent) = dst.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)
                    .map_err(|e| format!("Failed to create folder: {}", e))?;
            }
        }
        fs::copy(src, dst)
            .map(|_| ())
            .map_err(|e| format!("Failed to copy {}: {}", src_str, e))
    }
}

/// Move a file or directory, falling back to copy + delete when the two paths
/// are on different volumes.
///
/// `fs::rename` cannot cross volumes — dragging a folder from `C:` to `D:`
/// used to fail with a bare "Failed to rename". The fallback keeps the move
/// semantics (source disappears) instead of degrading to a silent copy.
pub fn move_path(old_str: &str, new_str: &str) -> Result<(), String> {
    match fs::rename(old_str, new_str) {
        Ok(()) => Ok(()),
        Err(e) if is_cross_device_error(&e) => {
            copy_path(old_str, new_str)?;
            delete_path(old_str).map_err(|del| {
                format!(
                    "Copied to the destination but the original could not be removed ({}). Please delete it manually.",
                    del
                )
            })
        }
        Err(e) => Err(format!("Failed to move: {}", e)),
    }
}

/// Return `path_str` if it is free, otherwise append " (2)", " (3)" … until an
/// unused name is found. Used so batch move/copy never silently overwrites.
pub fn unique_destination(path_str: &str) -> String {
    let path = Path::new(path_str);
    if !path.exists() {
        return path_str.to_string();
    }

    let parent = path.parent();
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let ext = path
        .extension()
        .map(|s| s.to_string_lossy().to_string())
        .filter(|e| !e.is_empty());

    for n in 2..=999u32 {
        let name = match &ext {
            Some(e) => format!("{} ({}).{}", stem, n, e),
            None => format!("{} ({})", stem, n),
        };
        let candidate = match parent {
            Some(p) if !p.as_os_str().is_empty() => p.join(&name),
            _ => Path::new(&name).to_path_buf(),
        };
        if !candidate.exists() {
            return candidate.to_string_lossy().to_string();
        }
    }
    path_str.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_move_path_moves_files_and_folders() {
        let root = std::env::temp_dir().join(format!("90ftp_move_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let src = root.join("src");
        let dst = root.join("dst");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst).unwrap();
        fs::write(src.join("a.txt"), b"hello").unwrap();
        fs::create_dir_all(src.join("nested")).unwrap();
        fs::write(src.join("nested").join("b.txt"), b"world").unwrap();

        let folder = src.join("nested");
        let moved_folder = dst.join("nested");
        move_path(&folder.to_string_lossy(), &moved_folder.to_string_lossy()).unwrap();
        assert!(!folder.exists(), "source folder should be gone after a move");
        assert_eq!(fs::read_to_string(moved_folder.join("b.txt")).unwrap(), "world");

        let file = src.join("a.txt");
        let moved_file = dst.join("a.txt");
        move_path(&file.to_string_lossy(), &moved_file.to_string_lossy()).unwrap();
        assert!(!file.exists(), "source file should be gone after a move");
        assert_eq!(fs::read_to_string(&moved_file).unwrap(), "hello");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn test_copy_path_leaves_source_in_place() {
        let root = std::env::temp_dir().join(format!("90ftp_copy_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let src = root.join("tree");
        fs::create_dir_all(src.join("inner")).unwrap();
        fs::write(src.join("inner").join("c.txt"), b"deep").unwrap();

        let dst = root.join("tree_copy");
        copy_path(&src.to_string_lossy(), &dst.to_string_lossy()).unwrap();
        assert!(src.join("inner").join("c.txt").exists(), "copy must not remove the source");
        assert_eq!(fs::read_to_string(dst.join("inner").join("c.txt")).unwrap(), "deep");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn test_unique_destination_avoids_overwrite() {
        let root = std::env::temp_dir().join(format!("90ftp_unique_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();

        let taken = root.join("report.txt");
        fs::write(&taken, b"1").unwrap();

        let first = unique_destination(&taken.to_string_lossy());
        assert_eq!(
            Path::new(&first).file_name().unwrap().to_string_lossy(),
            "report (2).txt"
        );

        fs::write(&first, b"2").unwrap();
        let second = unique_destination(&taken.to_string_lossy());
        assert_eq!(
            Path::new(&second).file_name().unwrap().to_string_lossy(),
            "report (3).txt"
        );

        let free = root.join("fresh.txt");
        assert_eq!(unique_destination(&free.to_string_lossy()), free.to_string_lossy());

        let _ = fs::remove_dir_all(&root);
    }

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
