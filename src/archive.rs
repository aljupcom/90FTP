use std::fs::File;
use std::io::Write;
use std::path::Path;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

pub fn compress_to_zip(source_paths: &[String], dest_zip_path: &str) -> Result<usize, String> {
    if source_paths.is_empty() {
        return Err("No source files or folders selected to compress".to_string());
    }

    let dest_path = Path::new(dest_zip_path);
    if let Some(parent) = dest_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let file = File::create(dest_path)
        .map_err(|e| format!("Failed to create destination zip file {:?}: {}", dest_path, e))?;
    let mut zip = ZipWriter::new(file);
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .unix_permissions(0o644);

    let mut count = 0;

    for src_str in source_paths {
        let src_path = Path::new(src_str);
        if !src_path.exists() {
            continue;
        }

        let base_name = match src_path.file_name() {
            Some(n) => n.to_string_lossy().to_string(),
            None => continue,
        };

        if src_path.is_file() {
            zip.start_file(&base_name, options)
                .map_err(|e| format!("Failed to add file {} to zip: {}", base_name, e))?;
            let mut f = File::open(src_path)
                .map_err(|e| format!("Failed to open file {:?}: {}", src_path, e))?;
            std::io::copy(&mut f, &mut zip)
                .map_err(|e| format!("Failed to write file {:?} into zip: {}", src_path, e))?;
            count += 1;
        } else if src_path.is_dir() {
            add_dir_recursive(&mut zip, src_path, &base_name, &mut count, options)?;
        }
    }

    zip.finish()
        .map_err(|e| format!("Failed to finalize zip archive: {}", e))?;

    Ok(count)
}

fn add_dir_recursive<W: Write + std::io::Seek>(
    zip: &mut ZipWriter<W>,
    current_dir: &Path,
    current_prefix: &str,
    count: &mut usize,
    options: SimpleFileOptions,
) -> Result<(), String> {
    let dir_name = if current_prefix.ends_with('/') {
        current_prefix.to_string()
    } else {
        format!("{}/", current_prefix)
    };

    let dir_options = options.unix_permissions(0o755);
    let _ = zip.add_directory(&dir_name, dir_options);

    let entries = std::fs::read_dir(current_dir)
        .map_err(|e| format!("Failed to read directory {:?}: {}", current_dir, e))?;

    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        let next_prefix = format!("{}{}", dir_name, name);

        if path.is_file() {
            zip.start_file(&next_prefix, options)
                .map_err(|e| format!("Failed to add entry {} to zip: {}", next_prefix, e))?;
            let mut f = File::open(&path)
                .map_err(|e| format!("Failed to read entry {:?}: {}", path, e))?;
            std::io::copy(&mut f, zip)
                .map_err(|e| format!("Failed to write entry {:?} into zip: {}", path, e))?;
            *count += 1;
        } else if path.is_dir() {
            add_dir_recursive(zip, &path, &next_prefix, count, options)?;
        }
    }

    Ok(())
}

pub fn extract_zip(zip_path: &str, dest_dir: &str) -> Result<usize, String> {
    let zip_file = Path::new(zip_path);
    if !zip_file.exists() {
        return Err(format!("Archive file not found: {}", zip_path));
    }

    let file = File::open(zip_file)
        .map_err(|e| format!("Failed to open zip archive {:?}: {}", zip_file, e))?;
    let mut archive = ZipArchive::new(file)
        .map_err(|e| format!("Failed to parse zip archive: {}", e))?;

    let dest = Path::new(dest_dir);
    std::fs::create_dir_all(dest)
        .map_err(|e| format!("Failed to create destination directory {:?}: {}", dest, e))?;

    let mut count = 0;

    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)
            .map_err(|e| format!("Failed to read zip entry #{}: {}", i, e))?;

        // Guard against Zip Slip directory traversal vulnerabilities
        let enclosed_name = match entry.enclosed_name() {
            Some(path) => path.to_owned(),
            None => continue,
        };

        let out_path = dest.join(enclosed_name);

        if entry.is_dir() {
            std::fs::create_dir_all(&out_path)
                .map_err(|e| format!("Failed to create directory {:?}: {}", out_path, e))?;
        } else {
            if let Some(parent) = out_path.parent() {
                if !parent.exists() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| format!("Failed to create directory {:?}: {}", parent, e))?;
                }
            }

            let mut out_file = File::create(&out_path)
                .map_err(|e| format!("Failed to create extracted file {:?}: {}", out_path, e))?;
            std::io::copy(&mut entry, &mut out_file)
                .map_err(|e| format!("Failed to write extracted file {:?}: {}", out_path, e))?;
            count += 1;
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Some(mode) = entry.unix_mode() {
                let _ = std::fs::set_permissions(&out_path, std::fs::Permissions::from_mode(mode));
            }
        }
    }

    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_zip_roundtrip() {
        let temp_dir = std::env::temp_dir().join(format!("90ftp_zip_test_{}", rand::random::<u32>()));
        let src_dir = temp_dir.join("src");
        let dest_dir = temp_dir.join("dest");
        let zip_file = temp_dir.join("test_archive.zip");

        std::fs::create_dir_all(&src_dir).unwrap();
        let sub_dir = src_dir.join("nested");
        std::fs::create_dir_all(&sub_dir).unwrap();

        std::fs::write(src_dir.join("hello.txt"), "Hello, 90FTP!").unwrap();
        std::fs::write(sub_dir.join("inner.json"), "{\"status\": \"ok\"}").unwrap();

        let sources = vec![
            src_dir.join("hello.txt").to_string_lossy().to_string(),
            sub_dir.to_string_lossy().to_string(),
        ];

        let count = compress_to_zip(&sources, &zip_file.to_string_lossy()).unwrap();
        assert!(count >= 2);
        assert!(zip_file.exists());

        let extracted_count = extract_zip(&zip_file.to_string_lossy(), &dest_dir.to_string_lossy()).unwrap();
        assert!(extracted_count >= 2);

        let read_hello = std::fs::read_to_string(dest_dir.join("hello.txt")).unwrap();
        assert_eq!(read_hello, "Hello, 90FTP!");

        let read_inner = std::fs::read_to_string(dest_dir.join("nested").join("inner.json")).unwrap();
        assert_eq!(read_inner, "{\"status\": \"ok\"}");

        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}
