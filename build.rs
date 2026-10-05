use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

// Only build inputs: never watch target, node_modules or generated TypeScript caches.
const INPUTS: &[&str] = &[
    "build.rs", "Cargo.toml", "Cargo.lock", "src", "web/src", "web/public", "web/dist",
    "web/index.html", "web/package.json", "web/bun.lock", "web/vite.config.ts",
    "web/tsconfig.json", "web/tsconfig.app.json", "web/tsconfig.node.json",
];

fn git(root: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git").env("GIT_OPTIONAL_LOCKS", "0").current_dir(root).args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn watch(path: &Path) {
    // Cargo treats a nonexistent watched file as perpetually changed.
    if path.exists() {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}

fn collect(path: &Path, files: &mut Vec<PathBuf>) {
    watch(path);
    if !path.exists() {
        if let Some(parent) = path.parent() {
            watch(parent);
        }
    }
    if path.is_dir() {
        let mut entries: Vec<_> = fs::read_dir(path).expect("reading build inputs")
            .map(|entry| entry.expect("reading build input").path()).collect();
        entries.sort();
        for entry in entries {
            collect(&entry, files);
        }
    } else if path.is_file() {
        files.push(path.to_owned());
    }
}

fn main() {
    let root = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let mut hash = Sha256::new();
    for input in INPUTS {
        let mut files = Vec::new();
        collect(&root.join(input), &mut files);
        for file in files {
            let relative = file.strip_prefix(&root).unwrap().to_string_lossy();
            let bytes = fs::read(&file).expect("reading build source");
            hash.update((relative.len() as u64).to_le_bytes());
            hash.update(relative.as_bytes());
            hash.update((bytes.len() as u64).to_le_bytes());
            hash.update(bytes);
        }
    }

    // An extracted archive may be nested in some unrelated checkout. That is not its revision.
    let checkout = git(&root, &["rev-parse", "--show-toplevel"])
        .and_then(|path| fs::canonicalize(path).ok()) == fs::canonicalize(&root).ok();
    let commit = if checkout { git(&root, &["rev-parse", "--verify", "HEAD"]) } else { None };
    let dirty = if checkout {
        let mut args = vec!["status", "--porcelain", "--untracked-files=all", "--"];
        args.extend_from_slice(INPUTS);
        // If inspection fails we cannot claim a clean source tree.
        git(&root, &args).is_none_or(|status| !status.is_empty())
    } else {
        false
    };
    if checkout {
        for item in ["HEAD", "index", "packed-refs"] {
            if let Some(path) = git(&root, &["rev-parse", "--git-path", item]) {
                watch(&root.join(path));
            }
        }
        if let Some(reference) = git(&root, &["symbolic-ref", "-q", "HEAD"]) {
            if let Some(path) = git(&root, &["rev-parse", "--git-path", &reference]) {
                watch(&root.join(path));
            }
        }
    }
    let now = SystemTime::now().duration_since(UNIX_EPOCH).expect("build clock predates UNIX epoch");
    hash.update(commit.as_deref().unwrap_or("unknown").as_bytes());
    hash.update([u8::from(dirty)]);
    hash.update(now.as_nanos().to_le_bytes());
    let id = format!("{:x}", hash.finalize());
    let revision = commit.as_deref().map(|value| &value[..12.min(value.len())]).unwrap_or("unknown");
    println!("cargo:rustc-env=SCIPI_BUILD_ID={id}");
    println!("cargo:rustc-env=SCIPI_BUILD_COMMIT={}", commit.as_deref().unwrap_or(""));
    println!("cargo:rustc-env=SCIPI_BUILD_DIRTY={dirty}");
    println!("cargo:rustc-env=SCIPI_BUILD_TIMESTAMP={}", now.as_secs());
    println!("cargo:rustc-env=SCIPI_BUILD_VERSION={} ({revision}{}, build {})",
        std::env::var("CARGO_PKG_VERSION").unwrap(), if dirty { "+dirty" } else { "" }, &id[..12]);
}
