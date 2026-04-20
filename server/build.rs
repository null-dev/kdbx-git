use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    time::SystemTime,
};

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("missing manifest dir"));
    let workspace_root = manifest_dir
        .parent()
        .expect("server crate should have a workspace parent");
    let web_ui_dir = workspace_root.join("web-ui");

    emit_rerun_if_changed(&manifest_dir.join("build.rs"));
    emit_rerun_if_changed(&web_ui_dir.join("package.json"));
    emit_rerun_if_changed(&web_ui_dir.join("package-lock.json"));
    emit_rerun_if_changed(&web_ui_dir.join("svelte.config.js"));
    emit_rerun_if_changed(&web_ui_dir.join("vite.config.ts"));
    emit_rerun_if_changed(&web_ui_dir.join("tsconfig.json"));
    emit_rerun_if_changed(&web_ui_dir.join("src"));
    emit_rerun_if_changed(&web_ui_dir.join("static"));
    println!(
        "cargo:rerun-if-changed={}",
        web_ui_dir.join("build").join("index.html").display()
    );
    println!("cargo:rerun-if-env-changed=PATH");

    if env::var_os("KDBX_GIT_SKIP_WEB_UI_BUILD").is_some() {
        println!("cargo:warning=Skipping web UI build because KDBX_GIT_SKIP_WEB_UI_BUILD is set");
        return;
    }

    ensure_web_ui_dependencies(&web_ui_dir);
    run_command(
        &web_ui_dir,
        npm_command(),
        &["run", "build"],
        "failed to build web UI",
    );
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
    if path.is_file() {
        println!("cargo:rerun-if-changed={}", path.display());
        return;
    }

    if !path.exists() {
        return;
    }

    visit_files(path, &mut |entry| {
        println!("cargo:rerun-if-changed={}", entry.display());
    });
}

fn visit_files(path: &Path, visitor: &mut impl FnMut(&Path)) {
    let Ok(entries) = fs::read_dir(path) else {
        return;
    };

    for entry in entries.flatten() {
        let entry_path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };

        if file_type.is_dir() {
            visit_files(&entry_path, visitor);
        } else if file_type.is_file() {
            visitor(&entry_path);
        }
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
