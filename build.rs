#[cfg(target_os = "windows")]
fn main() {
    println!("cargo:rerun-if-changed=assets/logo.ico");
    let mut resource = winresource::WindowsResource::new();
    resource.set_icon("assets/logo.ico");
    resource.set("FileDescription", "KemoLight");
    resource.set("ProductName", "KemoLight");
    resource.set("OriginalFilename", "kemolight.exe");

    if let Err(err) = resource.compile() {
        panic!("failed to compile Windows resources: {err}");
    }
}

#[cfg(not(target_os = "windows"))]
fn main() {}
