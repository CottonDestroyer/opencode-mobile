fn main() {
    slint_build::compile("ui/app.slint").expect("Slint build failed");
    if std::env::var("TARGET").unwrap_or_default().contains("android") {
        build_java_helper();
    }
}

/// Compiles `java/dev/opencodemobile/Helper.java` to `$OUT_DIR/classes.dex`, which `src/platform.rs`
/// embeds and loads at runtime. Slint's Android backend ships its own Java helper the same way.
fn build_java_helper() {
    use android_build::{Dexer, JavaBuild};
    use std::path::PathBuf;

    let src = "java/dev/opencodemobile/Helper.java";
    println!("cargo:rerun-if-changed={src}");
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    let classes = out_dir.join("java-classes");
    let _ = std::fs::remove_dir_all(&classes);
    std::fs::create_dir_all(&classes).unwrap();
    let android_jar = android_build::android_jar(None).expect("Android SDK platform (android.jar) not found; set ANDROID_HOME");

    let o = JavaBuild::new()
        .file(src)
        .class_path(&android_jar)
        .classes_out_dir(&classes)
        .java_source_version(8)
        .java_target_version(8)
        .command()
        .expect("could not build the javac command")
        .args(["-encoding", "UTF-8"])
        .output()
        .expect("could not run javac");
    if !o.status.success() {
        panic!("javac failed:\n{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    }

    let release = std::env::var("PROFILE").as_deref() == Ok("release");
    let o = Dexer::new()
        .android_jar(&android_jar)
        .class_path(&classes)
        .collect_classes(&classes)
        .unwrap()
        .release(release)
        .android_min_api(26)
        .out_dir(&out_dir)
        .command()
        .expect("could not build the d8 command")
        .output()
        .expect("could not run d8");
    if !o.status.success() {
        panic!("d8 failed:\n{}", String::from_utf8_lossy(&o.stderr));
    }
}
