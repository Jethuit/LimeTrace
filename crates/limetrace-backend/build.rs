#[cfg(windows)]
fn main() {
    if let Err(err) = compile_windows_resources() {
        panic!("failed to compile LimeTrace Backend resources: {err}");
    }
}

#[cfg(not(windows))]
fn main() {}

#[cfg(windows)]
fn compile_windows_resources() -> Result<(), Box<dyn std::error::Error>> {
    use ico::{IconDir, IconDirEntry, IconImage, ResourceType};
    use std::env;
    use std::fs::File;
    use std::path::PathBuf;

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR")?);
    let png_path = manifest_dir.join("../../LimeTrace.png");
    let out_dir = PathBuf::from(env::var("OUT_DIR")?);
    let ico_path = out_dir.join("limetrace-backend.ico");

    let image = image::open(&png_path)?.into_rgba8();
    let (width, height) = image.dimensions();
    let icon_image = IconImage::from_rgba_data(width, height, image.into_raw());

    let mut icon_dir = IconDir::new(ResourceType::Icon);
    icon_dir.add_entry(IconDirEntry::encode(&icon_image)?);
    let mut icon_file = File::create(&ico_path)?;
    icon_dir.write(&mut icon_file)?;

    let mut res = winres::WindowsResource::new();
    res.set_icon(ico_path.to_string_lossy().as_ref());
    res.set("ProductName", "LimeTrace Backend");
    res.set("FileDescription", "LimeTrace Backend");
    res.set("OriginalFilename", "limetrace-backend.exe");
    res.set("InternalName", "limetrace-backend.exe");
    res.compile()?;

    println!("cargo:rerun-if-changed={}", png_path.display());
    Ok(())
}
