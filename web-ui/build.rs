use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    time::SystemTime,
};

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("missing manifest dir"));

    emit_rerun_if_changed(&manifest_dir.join("build.rs"));
    emit_rerun_if_changed(&manifest_dir.join("Cargo.toml"));
    emit_rerun_if_changed(&manifest_dir.join("package.json"));
    emit_rerun_if_changed(&manifest_dir.join("package-lock.json"));
    emit_rerun_if_changed(&manifest_dir.join("svelte.config.js"));
    emit_rerun_if_changed(&manifest_dir.join("vite.config.ts"));
    emit_rerun_if_changed(&manifest_dir.join("tsconfig.json"));
    emit_rerun_if_changed(&manifest_dir.join("src"));
    emit_rerun_if_changed(&manifest_dir.join("static"));
    println!("cargo:rerun-if-env-changed=PATH");
    println!("cargo:rerun-if-env-changed=KDBX_GIT_SKIP_WEB_UI_BUILD");

    if env::var_os("KDBX_GIT_SKIP_WEB_UI_BUILD").is_some() {
        println!("cargo:warning=Skipping web UI build because KDBX_GIT_SKIP_WEB_UI_BUILD is set");
        return;
    }

    ensure_web_ui_dependencies(&manifest_dir);
    run_command(
        &manifest_dir,
        npm_command(),
        &["run", "build"],
        "failed to build web UI",
    );
    generate_embedded_assets(&manifest_dir);
}

fn ensure_web_ui_dependencies(web_ui_dir: &Path) {
    let lockfile = web_ui_dir.join("package-lock.json");
    let install_marker = web_ui_dir.join("node_modules").join(".package-lock.json");
    let package_json = web_ui_dir.join("package.json");

    let should_install = !install_marker.exists()
        || is_newer(&lockfile, &install_marker)
        || is_newer(&package_json, &install_marker);

    if should_install {
        let install_args: &[&str] = if lockfile.exists() { &["ci"] } else { &["install"] };
        run_command(
            web_ui_dir,
            npm_command(),
            install_args,
            "failed to install web UI dependencies",
        );
    }
}

fn npm_command() -> &'static str {
    if cfg!(windows) { "npm.cmd" } else { "npm" }
}

fn emit_rerun_if_changed(path: &Path) {
    if path.exists() {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}

fn generate_embedded_assets(web_ui_dir: &Path) {
    let build_dir = web_ui_dir.join("build");
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("missing OUT_DIR"));
    let generated_path = out_dir.join("generated_assets.rs");

    let mut asset_paths = Vec::new();
    collect_files(&build_dir, &build_dir, &mut asset_paths);
    asset_paths.sort();

    let mut source = String::new();
    source.push_str("fn generated_asset(path: &str) -> Option<EmbeddedAsset> {\n");
    source.push_str("    match path {\n");

    for asset_path in asset_paths {
        let full_path = build_dir.join(&asset_path);
        let content_type = content_type_for(&asset_path);
        source.push_str(&format!(
            "        {:?} => Some(EmbeddedAsset {{ bytes: include_bytes!(r#\"{}\"#), content_type: {:?} }}),\n",
            asset_path,
            full_path.display(),
            content_type
        ));
    }

    source.push_str("        _ => None,\n");
    source.push_str("    }\n");
    source.push_str("}\n");

    fs::write(&generated_path, source).expect("failed to write embedded asset manifest");
}

fn collect_files(root: &Path, dir: &Path, output: &mut Vec<String>) {
    let entries = fs::read_dir(dir).unwrap_or_else(|err| {
        panic!("failed to read embedded asset directory '{}': {err}", dir.display())
    });

    for entry in entries.flatten() {
        let path = entry.path();
        let file_type = entry.file_type().unwrap_or_else(|err| {
            panic!(
                "failed to read file type for embedded asset '{}': {err}",
                path.display()
            )
        });

        if file_type.is_dir() {
            collect_files(root, &path, output);
        } else if file_type.is_file() {
            let relative = path
                .strip_prefix(root)
                .expect("embedded asset should be under build root")
                .to_string_lossy()
                .replace('\\', "/");
            output.push(relative);
        }
    }
}

fn content_type_for(asset_path: &str) -> &'static str {
    match Path::new(asset_path)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
    {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "txt" => "text/plain; charset=utf-8",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    }
}

fn is_newer(source: &Path, target: &Path) -> bool {
    modified(source)
        .zip(modified(target))
        .map(|(source_time, target_time)| source_time > target_time)
        .unwrap_or(false)
}

fn modified(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).ok()?.modified().ok()
}

fn run_command(dir: &Path, program: &str, args: &[&str], context: &str) {
    let status = Command::new(program)
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap_or_else(|err| panic!("{context}: could not start `{program}`: {err}"));

    if !status.success() {
        panic!("{context}: `{program} {}` exited with {status}", args.join(" "));
    }
}
