use std::path::Path;
use zsdesk_platform::shell::{CancellationToken, DirectoryLister};
use zsdesk_windows::enumerate::Win32DirectoryLister;

fn main() {
    let lister = Win32DirectoryLister;
    let cancel = CancellationToken::new();
    for p in [
        "C://",
        "C://Users",
        "C://Program Files",
        "C://Windows//System32",
    ] {
        let r = lister.list(
            Path::new(p),
            zsdesk_core::model::view::RequestId(1),
            0,
            100,
            &cancel,
        );
        match r {
            Ok(b) => println!(
                "{p}: OK, total={}, 首项={:?}",
                b.total,
                b.entries.first().map(|e| &e.name)
            ),
            Err(e) => println!("{p}: ERROR -> {e}"),
        }
    }
}
