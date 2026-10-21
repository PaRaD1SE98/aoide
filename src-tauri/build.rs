fn main() {
    // macOS：whisper.cpp 的 Metal 后端用 @available 检查系统版本，编译后会调用 clang 运行时库里的
    // __isPlatformVersionAtLeast。rustc 链接时不带编译器的默认库，发布版构建会因此找不到这个符号，这里显式链接
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        let out = std::process::Command::new("clang").arg("--print-resource-dir").output();
        match out {
            Ok(out) if out.status.success() => {
                let dir = String::from_utf8_lossy(&out.stdout).trim().to_string();
                println!("cargo:rustc-link-search=native={dir}/lib/darwin");
                println!("cargo:rustc-link-lib=static=clang_rt.osx");
            }
            _ => println!("cargo:warning=clang --print-resource-dir failed; the Metal backend may not link"),
        }
    }
    tauri_build::build()
}
