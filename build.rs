fn main() {
    #[cfg(target_os = "windows")]
    {
        let mut res = winres::WindowsResource::new();
        res.set_icon("assets/app_icon.ico");
        res.set("ProductName", "90FTP");
        res.set("FileDescription", "90FTP - Modern Site Manager");
        res.set("LegalCopyright", "Copyright (c) 2026 aljailane");
        let _ = res.compile();
    }
}
