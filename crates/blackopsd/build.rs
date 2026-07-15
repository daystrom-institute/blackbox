use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

// Catalog discovery runs only inside Cargo's synchronous build-script process,
// never on an application Tokio worker.
#[allow(clippy::disallowed_methods)]
fn collect_json_files(root: &Path, directory: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_json_files(root, &path, files);
        } else if path
            .extension()
            .is_some_and(|extension| extension == "json")
            && path.file_name().is_none_or(|name| {
                let name = name.to_string_lossy();
                !name.starts_with('_') && name != "metadata.json"
            })
            && path.strip_prefix(root).is_ok()
        {
            files.push(path);
        }
    }
}

fn write_embedded_sources(out: &mut String, constant: &str, root: &Path) {
    let mut files = Vec::new();
    collect_json_files(root, root, &mut files);
    files.sort();
    out.push_str(&format!("pub const {constant}: &[(&str, &str)] = &[\n"));
    for path in files {
        let relative = path
            .strip_prefix(root)
            .expect("collected catalog source must remain below its root")
            .to_string_lossy();
        out.push_str(&format!(
            "    ({relative:?}, include_str!({path:?})),\n",
            path = path.to_string_lossy()
        ));
    }
    out.push_str("];\n");
}

#[allow(clippy::disallowed_methods)]
fn main() {
    println!("cargo:rerun-if-env-changed=BLACKOPSD_BUILD_ID");
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    let build_id = std::env::var("BLACKOPSD_BUILD_ID").unwrap_or_else(|_| {
        let revision = Command::new("git")
            .args(["rev-parse", "--short=12", "HEAD"])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "unknown".into());
        format!(
            "blackopsd-{}-{revision}",
            std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "unknown".into())
        )
    });
    println!("cargo:rustc-env=BLACKOPSD_BUILD_ID={build_id}");

    let workspace = PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").expect("Cargo sets CARGO_MANIFEST_DIR"),
    )
    .join("../..");
    let defaults = workspace.join("system-defaults");
    let atoms = defaults.join("atoms");
    let brofiles = defaults.join("brofiles");
    let workflows = defaults.join("workflows");
    println!("cargo:rerun-if-changed={}", atoms.display());
    println!("cargo:rerun-if-changed={}", brofiles.display());
    println!("cargo:rerun-if-changed={}", workflows.display());

    let mut embedded = String::new();
    write_embedded_sources(&mut embedded, "SHIPPED_ATOM_SOURCES", &atoms);
    write_embedded_sources(&mut embedded, "SHIPPED_BROFILE_SOURCES", &brofiles);
    write_embedded_sources(&mut embedded, "SHIPPED_WORKFLOW_SOURCES", &workflows);
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("Cargo sets OUT_DIR"));
    fs::write(out_dir.join("shipped_catalog.rs"), embedded)
        .expect("write embedded blackops catalog source");
}
