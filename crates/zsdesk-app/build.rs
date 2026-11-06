fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    println!("cargo:rerun-if-changed=resources/zsdesk.rc");
    println!("cargo:rerun-if-changed=resources/zsdesk.ico");
    println!("cargo:rerun-if-changed=resources/zsdesk.exe.manifest");
    embed_resource::compile("resources/zsdesk.rc", embed_resource::NONE)
        .manifest_required()
        .expect("failed to embed the Windows application manifest");
}
