// Embeds the ShellX icon (resource id 1, which GPUI loads as the window
// icon) and the application manifest (per-monitor v2 DPI awareness) into
// the Windows executable. Nothing to do on other targets.
fn main() {
    println!("cargo:rerun-if-changed=resources/shellx-term.rc");
    println!("cargo:rerun-if-changed=resources/shellx-term.manifest");
    println!("cargo:rerun-if-changed=resources/app_icon.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile("resources/shellx-term.rc", embed_resource::NONE)
            .manifest_required()
            .expect("failed to embed the Windows icon/manifest resource");
    }
}
