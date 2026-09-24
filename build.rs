use std::fs::File;
use std::path::Path;
use std::process::Command;

/// AGPL-3.0 §13: a user interacting with a modified version over a network
/// must be offered the Corresponding Source. The running binary therefore
/// has to know *which* source it was built from. Two values are baked in at
/// compile time so the footer link is correct for upstream builds, CI
/// builds, and downstream forks alike:
///
/// * `SM_SOURCE_COMMIT` — explicit env override (CI sets `$GITHUB_SHA`),
///   else `git rev-parse`, else `"unknown"` (source-tarball builds).
/// * `SM_SOURCE_URL` — explicit env override (a fork points this at its
///   own repo to stay §13-compliant), else the upstream repository.
fn emit_source_identity() {
    println!("cargo::rerun-if-env-changed=SM_SOURCE_COMMIT");
    println!("cargo::rerun-if-env-changed=SM_SOURCE_URL");
    watch_git_head();

    let commit = std::env::var("SM_SOURCE_COMMIT")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            Command::new("git")
                .args(["rev-parse", "--short=12", "HEAD"])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .filter(|s| !s.is_empty())
        })
        // Empty (not a placeholder string) is the "no git context" signal —
        // the render side keys off emptiness, so no sentinel literal has to
        // stay in sync across the build/runtime boundary.
        .unwrap_or_default();

    let url = std::env::var("SM_SOURCE_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "https://github.com/uptimepage/uptimepage".to_string());

    println!("cargo::rustc-env=SM_SOURCE_COMMIT={commit}");
    println!("cargo::rustc-env=SM_SOURCE_URL={url}");
}

/// Re-bake the commit whenever HEAD moves. `.git/HEAD` alone is not enough:
/// a commit on the *current* branch rewrites `.git/refs/heads/<branch>` (or
/// `.git/packed-refs`), not `.git/HEAD`, so the symref is resolved and the
/// underlying ref watched too. Only existing paths are emitted — a missing
/// `rerun-if-changed` target forces a rebuild every time, and non-git
/// builds (source tarball, worktree gitdir file) must just skip cleanly.
fn watch_git_head() {
    let git = Path::new(".git");
    let head = git.join("HEAD");
    if !head.exists() {
        return;
    }
    println!("cargo::rerun-if-changed=.git/HEAD");
    if git.join("packed-refs").exists() {
        println!("cargo::rerun-if-changed=.git/packed-refs");
    }
    if let Ok(h) = std::fs::read_to_string(&head)
        && let Some(rf) = h.strip_prefix("ref:").map(str::trim)
        && git.join(rf).exists()
    {
        println!("cargo::rerun-if-changed=.git/{rf}");
    }
}

/// Bundle the authored JS via the shared `scripts/build-js.sh`. Release hashes
/// names + emits a manifest (immutable caching) and sets the `assets_manifest`
/// cfg so `assets.rs` resolves through it; debug uses stable names served via
/// the per-request fingerprint, letting `just watch-js` re-bundle into a running
/// dev server with no rebuild. The single PROFILE check keeps the build output
/// and the runtime resolver on the same side. (Both layouts share static/js, so
/// a local `--release` build leaves it in release layout until the next debug.)
fn build_js() {
    println!("cargo::rustc-check-cfg=cfg(assets_manifest)");

    let status = Command::new("scripts/build-js.sh")
        .arg("build")
        .status()
        .expect("failed to invoke scripts/build-js.sh");
    assert!(status.success(), "build-js.sh exited non-zero");

    if std::env::var("PROFILE").as_deref() == Ok("release") {
        write_js_manifest("static/js/.esbuild-meta.json", "static/js/manifest.json");
        println!("cargo::rustc-cfg=assets_manifest");
    }
}

/// Map esbuild's metafile to `logical → hashed` for the asset filter, e.g.
/// `js/ui/api_form.js` → `js/ui/api_form-NBH4BHCR.js`.
fn write_js_manifest(meta_path: &str, manifest_path: &str) {
    let meta: serde_json::Value =
        serde_json::from_slice(&std::fs::read(meta_path).expect("read esbuild metafile"))
            .expect("parse esbuild metafile");

    let mut manifest = serde_json::Map::new();
    for (output, info) in meta["outputs"].as_object().expect("metafile outputs") {
        let Some(entry) = info["entryPoint"].as_str() else {
            continue;
        };
        let logical = entry.strip_prefix("assets/").unwrap_or(entry);
        let served = output.strip_prefix("static/").unwrap_or(output);
        manifest.insert(
            logical.to_string(),
            serde_json::Value::String(served.to_string()),
        );
    }

    std::fs::write(
        manifest_path,
        serde_json::to_vec_pretty(&serde_json::Value::Object(manifest))
            .expect("serialize manifest"),
    )
    .expect("write JS manifest");
}

fn main() {
    emit_source_identity();

    // Tailwind v4 scans `templates/**/*.html`, `src/**/*.rs`, AND
    // `assets/js/**/*.js` for class names (see @source directives in
    // static/css/input.css), so the CSS must rebuild whenever any of those
    // trees changes — including Rust files and JS modals that emit class
    // strings. The ~40ms overhead is the cost of co-locating class names with
    // the code that uses them.
    println!("cargo::rerun-if-changed=templates");
    println!("cargo::rerun-if-changed=static/css/input.css");
    println!("cargo::rerun-if-changed=static/css/_marketing.css");
    println!("cargo::rerun-if-changed=assets/js");
    println!("cargo::rerun-if-changed=src");
    println!("cargo::rerun-if-changed=scripts/fetch-tailwind.sh");
    println!("cargo::rerun-if-changed=scripts/fetch-esbuild.sh");
    println!("cargo::rerun-if-changed=scripts/build-js.sh");
    // Vendored libs live committed under static/js (esbuild only writes
    // subdirs), so watch them explicitly — they're outside the assets/js tree.
    for lib in [
        "echarts.min.js",
        "htmx.min.js",
        "qrcode.min.js",
        "json-enc.js",
    ] {
        println!("cargo::rerun-if-changed=static/js/{lib}");
    }
    // `include_dir!` does NOT emit a watcher of its own. Without this
    // an added blog post lands in main, a warm `cargo build` reuses the
    // cached binary, and the new post 404s on a route that was already
    // merged. Belt-and-braces — `src` above already covers it, but
    // pinning the content tree makes the dependency explicit.
    println!("cargo::rerun-if-changed=src/marketing/content");

    build_js();

    if !Path::new("bin/tailwindcss").exists() {
        let fetch = Command::new("scripts/fetch-tailwind.sh")
            .status()
            .expect("failed to invoke scripts/fetch-tailwind.sh");
        assert!(fetch.success(), "fetch-tailwind.sh exited non-zero");
    }

    // Serialize the shared app.css write; concurrent tailwind writers wedge.
    let lock = File::create("static/css/.tailwind.lock").expect("create tailwind lock");
    lock.lock().expect("acquire tailwind lock");

    let status = Command::new("./bin/tailwindcss")
        .args([
            "--input",
            "static/css/input.css",
            "--output",
            "static/css/app.css",
            "--minify",
        ])
        .status()
        .expect("tailwind build failed — is ./bin/tailwindcss present?");
    assert!(status.success(), "tailwind exited non-zero");

    precompress_assets();
}

/// Release writes a quality-11 brotli sibling (`app.css.br`) for every CSS and
/// JS asset, which `assets::serve` hands to clients that accept `br`. The
/// on-the-fly compression layer runs at a fast level and ships about a third
/// more bytes.
fn precompress_assets() {
    if std::env::var("PROFILE").as_deref() != Ok("release") {
        return;
    }
    let mut files = Vec::new();
    for dir in ["static/css", "static/js"] {
        collect_files(Path::new(dir), &mut files);
    }
    for file in files {
        if matches!(
            file.extension().and_then(|e| e.to_str()),
            Some("css" | "js")
        ) {
            write_brotli(&file);
        }
    }
}

fn collect_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read static dir") {
        let path = entry.expect("static dir entry").path();
        if path.is_dir() {
            collect_files(&path, out);
        } else {
            out.push(path);
        }
    }
}

fn write_brotli(src: &Path) {
    let mut dst = src.as_os_str().to_owned();
    dst.push(".br");
    let dst = std::path::PathBuf::from(dst);
    let modified = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    if matches!((modified(src), modified(&dst)), (Some(s), Some(d)) if d >= s) {
        return;
    }
    let data = std::fs::read(src).expect("read asset to compress");
    let mut out = Vec::new();
    let params = brotli::enc::BrotliEncoderParams {
        quality: 11,
        ..Default::default()
    };
    brotli::BrotliCompress(&mut data.as_slice(), &mut out, &params).expect("brotli compress");
    std::fs::write(&dst, out).expect("write brotli asset");
}
