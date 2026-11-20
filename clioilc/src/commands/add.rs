use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

const REGISTRY_MARKER: &str = "// clioilc-add-mod:insert-here";
/// Names whose `clioilc-<name>` crate already exists in the workspace.
const RESERVED: &[&str] = &["core"];

pub fn add_mod(name: &str) -> Result<()> {
    validate_name(name)?;
    let root = workspace_root()?;
    let pkg_dir = root.join("readers").join(name);
    if pkg_dir.exists() {
        bail!("reader '{name}' already exists at {}", pkg_dir.display());
    }

    let struct_name = format!("{}Reader", camel_case(name));
    let crate_ident = format!("clioilc_{}", name.replace('-', "_"));

    // Prepare every edit before writing anything, so a failure leaves the
    // workspace untouched.
    let manifest = root.join("clioilc").join("Cargo.toml");
    let registry = root.join("clioilc").join("src").join("readers.rs");
    let manifest_new = with_dependency(&read(&manifest)?, name)
        .with_context(|| format!("updating {}", manifest.display()))?;
    let entry = format!("Box::new({crate_ident}::{struct_name}),");
    let registry_new = with_registry_entry(&read(&registry)?, &entry)
        .with_context(|| format!("updating {}", registry.display()))?;

    fs::create_dir_all(pkg_dir.join("src"))?;
    fs::write(pkg_dir.join("Cargo.toml"), cargo_toml(name))?;
    fs::write(
        pkg_dir.join("src").join("lib.rs"),
        lib_rs(name, &struct_name),
    )?;
    fs::write(&manifest, manifest_new)?;
    fs::write(&registry, registry_new)?;

    println!("created reader '{name}' at {}", pkg_dir.display());
    println!("registered in clioilc/Cargo.toml and clioilc/src/readers.rs");
    println!("next: edit readers/{name}/src/lib.rs, then run `cargo build`");
    Ok(())
}

fn read(path: &Path) -> Result<String> {
    fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))
}

fn validate_name(name: &str) -> Result<()> {
    let mut chars = name.chars();
    let valid = chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        && !name.ends_with(['-', '_']);
    if !valid {
        bail!(
            "invalid reader name '{name}': start with a lowercase letter, \
             then use lowercase letters, digits, '-' or '_'"
        );
    }
    if RESERVED.contains(&name) {
        bail!("reader name '{name}' is reserved (clioilc-{name} already exists)");
    }
    Ok(())
}

/// The nearest ancestor of the current directory that holds the workspace.
fn workspace_root() -> Result<PathBuf> {
    let cwd = std::env::current_dir()?;
    cwd.ancestors()
        .find(|dir| dir.join("Cargo.toml").is_file() && dir.join("clioilc").join("Cargo.toml").is_file())
        .map(Path::to_path_buf)
        .with_context(|| {
            format!(
                "run `clioilc add mod` inside the clioilc workspace (no clioilc/Cargo.toml found above {})",
                cwd.display()
            )
        })
}

fn camel_case(name: &str) -> String {
    name.split(['-', '_'])
        .map(|seg| {
            let mut chars = seg.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().chain(chars).collect::<String>(),
                None => String::new(),
            }
        })
        .collect()
}

fn cargo_toml(name: &str) -> String {
    format!(
        r#"[package]
name = "clioilc-{name}"
version.workspace = true
edition.workspace = true
description = "clioilc reader for ~/.{name}"

[dependencies]
anyhow.workspace = true
clioilc-core.workspace = true
"#
    )
}

fn lib_rs(name: &str, struct_name: &str) -> String {
    format!(
        r#"use anyhow::Result;
use clioilc_core::{{home_dir, Reader}};
use std::path::PathBuf;

/// Reader for files in `~/.{name}`.
#[derive(Debug, Default, Clone, Copy)]
pub struct {struct_name};

impl Reader for {struct_name} {{
    fn name(&self) -> &'static str {{
        "{name}"
    }}

    fn default_watch_dir(&self) -> Result<PathBuf> {{
        Ok(home_dir()?.join(".{name}"))
    }}

    fn extensions(&self) -> &'static [&'static str] {{
        &["json", "jsonl"]
    }}

    // Override `project_name(&self, relative: &Path)` to group events by
    // project; it receives the file path relative to the watched directory.
}}
"#
    )
}

fn with_dependency(manifest: &str, name: &str) -> Result<String> {
    let dep = format!("clioilc-{name} = {{ path = \"../readers/{name}\" }}");
    if manifest.contains(&dep) {
        return Ok(manifest.to_string());
    }
    insert_after_line(manifest, "[dependencies]", &dep).context("no [dependencies] section")
}

/// Insert `entry` on its own line just above the registry marker, using the
/// marker's indentation.
fn with_registry_entry(registry: &str, entry: &str) -> Result<String> {
    if registry.contains(entry) {
        return Ok(registry.to_string());
    }
    let marker = registry
        .find(REGISTRY_MARKER)
        .with_context(|| format!("registry marker `{REGISTRY_MARKER}` not found"))?;
    let line_start = registry[..marker].rfind('\n').map_or(0, |i| i + 1);
    let indent = &registry[line_start..marker];
    Ok(format!(
        "{}{indent}{entry}\n{}",
        &registry[..line_start],
        &registry[line_start..]
    ))
}

fn insert_after_line(content: &str, target: &str, line: &str) -> Option<String> {
    let mut out = String::with_capacity(content.len() + line.len() + 1);
    let mut inserted = false;
    for l in content.lines() {
        out.push_str(l);
        out.push('\n');
        if !inserted && l.trim() == target {
            out.push_str(line);
            out.push('\n');
            inserted = true;
        }
    }
    inserted.then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn camel_cases_names() {
        assert_eq!(camel_case("claude"), "Claude");
        assert_eq!(camel_case("my-tool_x"), "MyToolX");
    }

    #[test]
    fn validates_names() {
        for ok in ["claude", "my-tool", "tool_2"] {
            assert!(validate_name(ok).is_ok(), "{ok}");
        }
        for bad in ["", "-x", "1x", "Tool", "x-", "a b", "core"] {
            assert!(validate_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn adds_dependency_once() {
        let manifest = "[package]\nname = \"x\"\n\n[dependencies]\nanyhow = \"1\"\n";
        let once = with_dependency(manifest, "foo").unwrap();
        assert!(
            once.contains("[dependencies]\nclioilc-foo = { path = \"../readers/foo\" }\nanyhow")
        );
        assert_eq!(with_dependency(&once, "foo").unwrap(), once);
        assert!(with_dependency("[package]\n", "foo").is_err());
    }

    #[test]
    fn adds_registry_entry_above_marker() {
        let registry =
            "vec![\n        Box::new(a::A),\n        // clioilc-add-mod:insert-here\n    ]\n";
        let updated = with_registry_entry(registry, "Box::new(b::B),").unwrap();
        assert_eq!(
            updated,
            "vec![\n        Box::new(a::A),\n        Box::new(b::B),\n        // clioilc-add-mod:insert-here\n    ]\n"
        );
        assert_eq!(
            with_registry_entry(&updated, "Box::new(b::B),").unwrap(),
            updated
        );
        assert!(with_registry_entry("vec![]\n", "Box::new(b::B),").is_err());
    }
}
