fn main() {
    // Fluent style follows the Windows light/dark setting
    slint_build::compile_with_config("ui/settings.slint", slint_build::CompilerConfiguration::new().with_style("fluent".into())).unwrap();
    // exe icon (resource id 1, also used for the tray) + version info
    winresource::WindowsResource::new().set_icon("ui/icon.ico").compile().unwrap();
    println!("cargo:rerun-if-changed=ui");
}
