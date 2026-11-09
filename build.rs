fn main() {
    // Embed the app icon into the exe (Explorer, taskbar, Alt+Tab).
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/app.ico");
        res.set("ProductName", "PNG Optimizer");
        res.set("FileDescription", "PNG Optimizer");
        res.compile().expect("failed to compile Windows resources");
    }
    println!("cargo:rerun-if-changed=assets/app.ico");
}
