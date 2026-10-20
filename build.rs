fn main() {
    #[cfg(target_os = "windows")]
    {
        let mut res = winres::WindowsResource::new();
        res.set_icon("assets/app_icon.ico");
        res.set("ProductName", "90FTP");
        res.set("FileDescription", "90FTP - Modern Site Manager");
        res.set("LegalCopyright", "Copyright (c) 2026 aljailane");
        let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.1".into());
        res.set("ProductVersion", &version);
        res.set("FileVersion", &version);
        let _ = res.compile();
    }
}
