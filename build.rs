fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    println!("cargo:rerun-if-changed=icon.ico");

    let out_dir = std::env::var("OUT_DIR").unwrap();
    let rc_path  = format!("{}/icon.rc",  out_dir);
    let res_path = format!("{}/icon.res", out_dir);

    // Write a minimal .rc file referencing our icon
    std::fs::write(&rc_path, "1 ICON \"icon.ico\"\n")
        .expect("Could not write icon.rc");

    // Copy icon.ico into OUT_DIR so windres can find it
    let ico_dst = format!("{}/icon.ico", out_dir);
    std::fs::copy("icon.ico", &ico_dst)
        .expect("Could not find icon.ico -- place it in the project root next to Cargo.toml");

    // Cross-compiling from macOS/Linux uses the MinGW windres
    let windres = if cfg!(target_os = "windows") {
        "windres".to_string()
    } else {
        "x86_64-w64-mingw32-windres".to_string()
    };

    let status = std::process::Command::new(&windres)
        .args([&rc_path, "-O", "coff", "-o", &res_path])
        .status()
        .unwrap_or_else(|e| panic!("Failed to run {}: {}", windres, e));

    if !status.success() {
        panic!("windres failed -- is mingw-w64 installed? (brew install mingw-w64)");
    }

    // Tell the linker to include the compiled resource
    println!("cargo:rustc-link-arg={}", res_path);
}
