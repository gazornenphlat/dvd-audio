fn main() {
    // Gives the .exe the DVD icon in Explorer and on the taskbar.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winres::WindowsResource::new();
        res.set_icon("icon/dvd_icon.ico");
        res.compile().expect("could not embed the program icon");
    }
}
