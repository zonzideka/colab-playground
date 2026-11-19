//! The copyright line for Settings → About (every target), and on Windows the executable's
//! resources: the icon and the version information Explorer, Task Manager and the file's
//! Properties show. Other targets get no resources from this script.
//!
//! The icon is embedded under resource id 1 because that is the id gpui's Windows platform
//! loads for the window class (window, taskbar button, Alt+Tab); Explorer also picks the
//! numerically lowest icon id for the file. The copyright line is read from the repository's
//! `LICENSE`, the version and description from the Cargo package, so each fact has one home.

use std::{env, fs, path::Path};

fn main() {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("set by Cargo");
    let root = Path::new(&manifest_dir).join("../..");
    let license = root.join("LICENSE");
    let icon = root.join("assets/icon/icon.ico");
    println!("cargo:rerun-if-changed={}", license.display());
    println!("cargo:rerun-if-changed={}", icon.display());
    let copyright = fs::read_to_string(&license)
        .ok()
        .and_then(|text| {
            text.lines()
                .map(str::trim)
                .find(|line| line.starts_with("Copyright"))
                .map(str::to_owned)
        })
        .expect("LICENSE has a copyright line");
    // Settings → About shows the same line on every platform.
    println!("cargo:rustc-env=COLAB_PLAYGROUND_COPYRIGHT={copyright}");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let version = env::var("CARGO_PKG_VERSION").expect("set by Cargo");
    let description = env::var("CARGO_PKG_DESCRIPTION").expect("set by Cargo");
    let mut resource = tauri_winres::WindowsResource::new();
    resource
        .set_icon_with_id(&icon.to_string_lossy(), "1")
        // Task Manager shows FileDescription as the program's name.
        .set("FileDescription", "Colab Playground")
        .set("ProductName", "Colab Playground")
        .set("CompanyName", "zonzideka")
        .set("LegalCopyright", &copyright)
        .set("FileVersion", &version)
        .set("ProductVersion", &version)
        .set("OriginalFilename", "colab-playground.exe")
        .set("InternalName", "colab-playground.exe")
        .set("Comments", &description);
    resource
        .compile()
        .expect("the Windows resource compiler (rc.exe from the Windows SDK) is available");
}
