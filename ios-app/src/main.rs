mod model;
#[cfg(target_os = "ios")]
mod objc_ffi;
#[cfg(target_os = "ios")]
mod ui;

fn main() {
    #[cfg(target_os = "ios")]
    {
        ui::run();
    }
    #[cfg(not(target_os = "ios"))]
    {
        eprintln!("Rebox UI runs only on iOS (aarch64-apple-ios).");
    }
}
