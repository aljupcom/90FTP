fn main() {
    #[cfg(target_os = "windows")]
    {
        let mut res = winres::WindowsResource::new();
        res.set_icon("assets/app_icon.ico");
        res.set("ProductName", "90FTP");
        res.set("FileDescription", "90FTP - Modern Site Manager");
        res.set("LegalCopyright", "Copyright (c) 2026 aljailane");
        let cargo_ver = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.2".into());
        let last_num = cargo_ver.split('.').last().unwrap_or("2");
        let display_ver = format!("0.0.0.{}", last_num);
        res.set("ProductVersion", &display_ver);
        res.set("FileVersion", &display_ver);
        let _ = res.compile();
    }
}
