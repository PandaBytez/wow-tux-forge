use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::io::{self, BufReader, Read, Seek};
use std::path::{Component, Path, PathBuf};
use zip::ZipArchive;

#[derive(Debug, Clone)]
pub struct LocalAddon {
    pub folder: String,
    pub version: Option<String>,
    pub github_hint: Option<String>,
}

fn extract_github_repo(toc_content: &str) -> Option<String> {
    for line in toc_content.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with("##") {
            continue;
        }
        let Some(pos) = trimmed.find("github.com/") else {
            continue;
        };
        let rest = &trimmed[pos + "github.com/".len()..];
        let clean = rest
            .split(|c: char| c.is_whitespace() || c == '?' || c == '#')
            .next()
            .unwrap_or("");
        let mut parts = clean.split('/').filter(|s| !s.is_empty());
        let Some(owner) = parts.next() else {
            continue;
        };
        let Some(raw_repo) = parts.next() else {
            continue;
        };
        let repo = raw_repo.strip_suffix(".git").unwrap_or(raw_repo);
        if !owner.is_empty() && !repo.is_empty() && owner != "topics" && owner != "orgs" {
            return Some(format!("{owner}/{repo}"));
        }
    }
    None
}

pub fn scan_unmanaged_addons(addons_dir: &Path, managed_folders: &HashSet<String>) -> Vec<LocalAddon> {
    let Ok(entries) = fs::read_dir(addons_dir) else {
        return Vec::new();
    };

    let mut found = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(folder) = path.file_name().and_then(|n| n.to_str()).map(String::from) else {
            continue;
        };
        if managed_folders.contains(&folder) {
            continue;
        }

        let Ok(files) = fs::read_dir(&path) else {
            continue;
        };
        let toc_path = files
            .flatten()
            .map(|f| f.path())
            .find(|p| p.is_file() && p.extension().is_some_and(|e| e.eq_ignore_ascii_case("toc")));

        let Some(toc_path) = toc_path else {
            continue;
        };

        let toc_content = fs::read_to_string(&toc_path).ok();
        let version = toc_content.as_deref().and_then(|content| {
            content.lines().find_map(|line| {
                let rest = line.trim().strip_prefix("##")?;
                let (key, val) = rest.split_once(':')?;
                if key.trim().eq_ignore_ascii_case("Version") {
                    let v = val.trim();
                    (!v.is_empty()).then(|| v.to_string())
                } else {
                    None
                }
            })
        });
        let github_hint = toc_content.as_deref().and_then(extract_github_repo);

        found.push(LocalAddon {
            folder,
            version,
            github_hint,
        });
    }

    found.sort_by(|a, b| a.folder.to_lowercase().cmp(&b.folder.to_lowercase()));
    found
}

fn detect_strip_depth(paths: &[PathBuf]) -> Result<usize, String> {
    let min_toc_len = paths
        .iter()
        .filter(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("toc"))
        })
        .map(|p| p.components().count())
        .min()
        .ok_or_else(|| "Zip archive does not contain any .toc addon files".to_string())?;

    if min_toc_len < 2 {
        return Err("Invalid addon zip layout: .toc file is at archive root without an addon folder".to_string());
    }

    Ok(min_toc_len - 2)
}

pub fn extract_addon_zip_file(zip_path: &Path, addons_dir: &Path) -> Result<Vec<String>, String> {
    let file = fs::File::open(zip_path)
        .map_err(|e| format!("Failed opening zip {}: {e}", zip_path.display()))?;
    extract_addon_zip_reader(BufReader::new(file), addons_dir)
}

#[cfg(test)]
pub fn extract_addon_zip(zip_bytes: &[u8], addons_dir: &Path) -> Result<Vec<String>, String> {
    extract_addon_zip_reader(io::Cursor::new(zip_bytes), addons_dir)
}

pub fn extract_addon_zip_reader<R: Read + Seek>(
    reader: R,
    addons_dir: &Path,
) -> Result<Vec<String>, String> {
    if !addons_dir.is_dir() {
        return Err(format!("AddOns directory does not exist: {}", addons_dir.display()));
    }

    let mut archive = ZipArchive::new(reader).map_err(|e| format!("Invalid zip archive: {e}"))?;

    let mut enclosed_paths = Vec::with_capacity(archive.len());
    for i in 0..archive.len() {
        let file = archive.by_index(i).map_err(|e| format!("Zip read error: {e}"))?;
        let path = file
            .enclosed_name()
            .ok_or_else(|| format!("Unsafe path in zip archive: {}", file.name()))?;
        if !file.is_dir() {
            enclosed_paths.push(path);
        }
    }

    let strip_depth = detect_strip_depth(&enclosed_paths)?;

    // Handle GitHub source zipballs like `Jaliborc-Scrap-a1b2c3d/Scrap.toc` where
    // the repo root itself is the addon folder: rename the wrapper to match the .toc stem.
    let root_rename: Option<(String, String)> = if strip_depth == 0 {
        let mut top_dirs = BTreeSet::new();
        let mut root_toc_stems = BTreeSet::new();
        for p in &enclosed_paths {
            let comps: Vec<Component<'_>> = p.components().collect();
            if let Some(Component::Normal(first)) = comps.first() {
                let first_str = first.to_string_lossy().to_string();
                top_dirs.insert(first_str);
                if comps.len() == 2
                    && p.extension()
                        .and_then(|e| e.to_str())
                        .is_some_and(|e| e.eq_ignore_ascii_case("toc"))
                    && let Some(stem) = p.file_stem().and_then(|s| s.to_str())
                {
                    root_toc_stems.insert(stem.to_string());
                }
            }
        }
        if top_dirs.len() == 1 && !root_toc_stems.is_empty() {
            let only_dir = top_dirs.into_iter().next().unwrap();
            if !root_toc_stems.contains(&only_dir) {
                let primary = root_toc_stems
                    .into_iter()
                    .min_by_key(|s| s.len())
                    .unwrap();
                Some((only_dir, primary))
            } else {
                None
            }
        } else {
            None
        }
    } else {
        None
    };

    let mut top_folders = BTreeSet::new();

    for i in 0..archive.len() {
        let mut file = archive.by_index(i).map_err(|e| format!("Zip read error: {e}"))?;
        let Some(rel_path) = file.enclosed_name() else {
            continue;
        };

        let comps: Vec<Component<'_>> = rel_path.components().collect();
        if comps.len() <= strip_depth {
            continue;
        }

        let trimmed: PathBuf = comps[strip_depth..].iter().collect();
        if trimmed.as_os_str().is_empty() {
            continue;
        }

        // Only extract files/dirs that belong to a top-level addon folder.
        let mut trimmed_comps = trimmed.components();
        let Some(Component::Normal(first)) = trimmed_comps.next() else {
            continue;
        };
        if !file.is_dir() && trimmed_comps.clone().next().is_none() {
            // Skip stray root files like README.md outside any addon folder.
            continue;
        }

        let mut folder_name = first.to_string_lossy().to_string();
        let final_rel: PathBuf = if let Some((from, to)) = &root_rename
            && &folder_name == from
        {
            folder_name = to.clone();
            let mut rebuilt = PathBuf::from(to);
            for c in trimmed_comps {
                rebuilt.push(c);
            }
            rebuilt
        } else {
            trimmed
        };

        top_folders.insert(folder_name);

        let dest = addons_dir.join(&final_rel);
        if file.is_dir() {
            fs::create_dir_all(&dest).map_err(|e| format!("Failed creating dir {}: {e}", dest.display()))?;
        } else {
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)
                    .map_err(|e| format!("Failed creating dir {}: {e}", parent.display()))?;
            }
            let mut out = fs::File::create(&dest)
                .map_err(|e| format!("Failed creating file {}: {e}", dest.display()))?;
            io::copy(&mut file, &mut out)
                .map_err(|e| format!("Failed writing file {}: {e}", dest.display()))?;
        }
    }

    if top_folders.is_empty() {
        return Err("No addon folders were extracted from the zip".to_string());
    }

    Ok(top_folders.into_iter().collect())
}

pub fn remove_folders(addons_dir: &Path, folders: &[String]) -> Result<(), String> {
    for folder in folders {
        if folder.is_empty()
            || folder == "."
            || folder == ".."
            || folder.contains('/')
            || folder.contains('\\')
        {
            return Err(format!("Refusing to delete unsafe folder name: {folder}"));
        }
        let target = addons_dir.join(folder);
        if target.exists() {
            fs::remove_dir_all(&target)
                .map_err(|e| format!("Failed removing {}: {e}", target.display()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    fn build_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = io::Cursor::new(Vec::new());
        {
            let mut zw = zip::ZipWriter::new(&mut buf);
            let opts = SimpleFileOptions::default();
            for (name, data) in entries {
                zw.start_file(name, opts).unwrap();
                zw.write_all(data).unwrap();
            }
            zw.finish().unwrap();
        }
        buf.into_inner()
    }

    #[test]
    fn extracts_standard_and_wrapper_zips() {
        let tmp = std::env::temp_dir().join(format!("wow-tux-forge-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        // Standard layout
        let z1 = build_zip(&[
            ("MyAddon/MyAddon.toc", b"## Title: MyAddon\n"),
            ("MyAddon/Core.lua", b"print('hi')\n"),
        ]);
        let folders1 = extract_addon_zip(&z1, &tmp).unwrap();
        assert_eq!(folders1, vec!["MyAddon"]);
        assert!(tmp.join("MyAddon/MyAddon.toc").is_file());

        // Wrapper folder layout
        let z2 = build_zip(&[
            ("MyAddon-v1.2/README.md", b"docs"),
            ("MyAddon-v1.2/WrappedAddon/WrappedAddon.toc", b"## Title: Wrapped\n"),
            ("MyAddon-v1.2/WrappedAddon/Init.lua", b"return\n"),
        ]);
        let folders2 = extract_addon_zip(&z2, &tmp).unwrap();
        assert_eq!(folders2, vec!["WrappedAddon"]);
        assert!(tmp.join("WrappedAddon/WrappedAddon.toc").is_file());
        assert!(!tmp.join("MyAddon-v1.2").exists());

        remove_folders(&tmp, &folders1).unwrap();
        remove_folders(&tmp, &folders2).unwrap();
        assert!(!tmp.join("MyAddon").exists());
        assert!(!tmp.join("WrappedAddon").exists());

        // GitHub source zipball with .toc at repo root
        let z3 = build_zip(&[
            ("Jaliborc-Scrap-a1b2c3d/Scrap.toc", b"## Title: Scrap\n"),
            ("Jaliborc-Scrap-a1b2c3d/Scrap_Mainline.toc", b"## Title: Scrap\n"),
            ("Jaliborc-Scrap-a1b2c3d/core/main.lua", b"return\n"),
        ]);
        let folders3 = extract_addon_zip(&z3, &tmp).unwrap();
        assert_eq!(folders3, vec!["Scrap"]);
        assert!(tmp.join("Scrap/Scrap.toc").is_file());
        assert!(tmp.join("Scrap/core/main.lua").is_file());
        assert!(!tmp.join("Jaliborc-Scrap-a1b2c3d").exists());

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn rejects_zip_without_toc() {
        let tmp = std::env::temp_dir().join(format!("wow-tux-forge-test-notoc-{}", std::process::id()));
        fs::create_dir_all(&tmp).unwrap();

        let z = build_zip(&[("SomeFolder/readme.txt", b"hello")]);
        let err = extract_addon_zip(&z, &tmp).unwrap_err();
        assert!(err.contains(".toc"));

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn scans_unmanaged_addons_and_skips_managed() {
        let tmp = std::env::temp_dir().join(format!("wow-tux-forge-scan-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join("ManagedAddon")).unwrap();
        fs::write(tmp.join("ManagedAddon/ManagedAddon.toc"), "## Version: 1.0\n").unwrap();

        fs::create_dir_all(tmp.join("ExistingAddon")).unwrap();
        fs::write(
            tmp.join("ExistingAddon/ExistingAddon.toc"),
            "## Version: 2.3.4\n## X-Website: https://github.com/DeadlyBossMods/DeadlyBossMods.git\n",
        )
        .unwrap();

        fs::create_dir_all(tmp.join("NotAnAddon")).unwrap();
        fs::write(tmp.join("NotAnAddon/notes.txt"), "hi").unwrap();

        let managed = HashSet::from(["ManagedAddon".to_string()]);
        let found = scan_unmanaged_addons(&tmp, &managed);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].folder, "ExistingAddon");
        assert_eq!(found[0].version.as_deref(), Some("2.3.4"));
        assert_eq!(
            found[0].github_hint.as_deref(),
            Some("DeadlyBossMods/DeadlyBossMods")
        );

        let _ = fs::remove_dir_all(&tmp);
    }
}
