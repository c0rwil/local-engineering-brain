//! Skill packs: distribute agent-native skills (`<skill>/SKILL.md`
//! directories, the format Claude Code and Codex both auto-trigger by
//! description relevance) through Engram. A pack is a git repository or local
//! directory carrying one or more skills; `engram skills add` fetches it once
//! into `~/.engram/skillpacks/` and installs every skill into each detected
//! agent's user-level skills directory, so any provider on the machine uses
//! them natively when relevant — no per-repo setup, no injection cost.
//!
//! Installed skill directories carry a `.engram-skillpack` marker naming their
//! pack. Engram only ever overwrites or removes directories carrying a marker:
//! a hand-written skill with the same name is never clobbered.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Marker file naming the owning pack inside every installed skill dir.
const MARKER_FILE: &str = ".engram-skillpack";
/// Marker written by pre-rename installs; still recognized as ours.
const LEGACY_MARKER_FILE: &str = ".leb-skillpack";
/// Pack metadata file written into the pack root after `add`.
const PACK_META_FILE: &str = ".engram-pack.toml";
const LEGACY_PACK_META_FILE: &str = ".leb-pack.toml";
/// Bounded copy: a skill directory larger than this is refused.
const MAX_SKILL_ENTRIES: usize = 400;
const MAX_SKILL_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PackMeta {
    pub source: String,
}

#[derive(Clone, Debug)]
pub struct SkillPack {
    pub name: String,
    pub path: PathBuf,
    pub source: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
}

/// An agent whose user-level skills directory exists on this machine.
#[derive(Clone, Debug)]
pub struct AgentTarget {
    pub label: &'static str,
    pub skills_dir: PathBuf,
}

/// Where packs live.
#[must_use]
pub fn packs_dir(home: &Path) -> PathBuf {
    home.join(".engram").join("skillpacks")
}

/// Detect agent targets: any of `~/.claude` / `~/.codex` that exist get a
/// `skills` directory (created on demand). Detection is by the agent's own
/// config directory so Engram never invents an agent installation.
#[must_use]
pub fn detect_targets(home: &Path) -> Vec<AgentTarget> {
    let mut targets = Vec::new();
    for (label, dir) in [("claude", ".claude"), ("codex", ".codex")] {
        let base = home.join(dir);
        if base.is_dir() {
            targets.push(AgentTarget {
                label,
                skills_dir: base.join("skills"),
            });
        }
    }
    targets
}

/// Derive a pack name from its source (last path segment, `.git` stripped).
#[must_use]
pub fn pack_name_from_source(source: &str) -> String {
    let trimmed = source.trim_end_matches('/');
    let last = trimmed
        .rsplit(['/', ':'])
        .next()
        .unwrap_or(trimmed)
        .trim_end_matches(".git");
    let safe: String = last
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    if safe.is_empty() {
        "pack".to_owned()
    } else {
        safe
    }
}

/// Fetch a pack: git-clone a URL (or local git repo) shallowly into the
/// packs directory. An existing pack of the same name is refused — use
/// `update` or `remove` first.
///
/// # Errors
///
/// Returns errors for existing packs, failed clones, or filesystem failures.
pub fn add(home: &Path, source: &str, name: Option<&str>) -> Result<SkillPack> {
    let name = name.map_or_else(|| pack_name_from_source(source), str::to_owned);
    let destination = packs_dir(home).join(&name);
    if destination.exists() {
        return Err(Error::Config(format!(
            "skill pack '{name}' already exists; `engram skills update` refreshes it, \
             `engram skills remove {name}` deletes it"
        )));
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(|error| Error::Config(format!("packs dir: {error}")))?;
    }
    let output = Command::new("git")
        .args(["clone", "--depth", "1", source])
        .arg(&destination)
        .output()
        .map_err(|error| Error::Config(format!("git clone: {error}")))?;
    if !output.status.success() {
        let _ = fs::remove_dir_all(&destination);
        return Err(Error::Config(format!(
            "git clone of {source} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let meta = PackMeta {
        source: source.to_owned(),
    };
    fs::write(
        destination.join(PACK_META_FILE),
        toml::to_string(&meta).unwrap_or_default(),
    )
    .map_err(|error| Error::Config(format!("pack meta: {error}")))?;
    let pack = SkillPack {
        name,
        path: destination,
        source: Some(meta.source),
    };
    if discover_skills(&pack.path)?.is_empty() {
        let path = pack.path.clone();
        let _ = fs::remove_dir_all(&path);
        return Err(Error::Config(format!(
            "{source} contains no skills (expected `skills/<name>/SKILL.md` or a root SKILL.md)"
        )));
    }
    Ok(pack)
}

/// List fetched packs.
///
/// # Errors
///
/// Returns filesystem errors (a missing packs directory is an empty list).
pub fn list(home: &Path) -> Result<Vec<SkillPack>> {
    let dir = packs_dir(home);
    let mut packs = Vec::new();
    let Ok(entries) = fs::read_dir(&dir) else {
        return Ok(packs);
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let source = fs::read_to_string(path.join(PACK_META_FILE))
            .or_else(|_| fs::read_to_string(path.join(LEGACY_PACK_META_FILE)))
            .ok()
            .and_then(|content| toml::from_str::<PackMeta>(&content).ok())
            .map(|meta| meta.source);
        packs.push(SkillPack {
            name: entry.file_name().to_string_lossy().into_owned(),
            path,
            source,
        });
    }
    packs.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(packs)
}

/// `git pull` a fetched pack.
///
/// # Errors
///
/// Returns an error when the pull fails.
pub fn update(pack: &SkillPack) -> Result<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(&pack.path)
        .args(["pull", "--ff-only"])
        .output()
        .map_err(|error| Error::Config(format!("git pull: {error}")))?;
    if !output.status.success() {
        return Err(Error::Config(format!(
            "git pull for pack '{}' failed: {}",
            pack.name,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

/// Discover the skills a pack carries: `skills/*/SKILL.md`, or a root
/// `SKILL.md` (skill named after the pack directory).
///
/// # Errors
///
/// Returns filesystem errors.
pub fn discover_skills(pack_path: &Path) -> Result<Vec<Skill>> {
    let mut skills = Vec::new();
    let skills_dir = pack_path.join("skills");
    if let Ok(entries) = fs::read_dir(&skills_dir) {
        for entry in entries.flatten() {
            let dir = entry.path();
            let manifest = dir.join("SKILL.md");
            if manifest.is_file() {
                if let Some(skill) = read_skill(&dir, &manifest) {
                    skills.push(skill);
                }
            }
        }
    }
    if skills.is_empty() {
        let manifest = pack_path.join("SKILL.md");
        if manifest.is_file() {
            if let Some(skill) = read_skill(pack_path, &manifest) {
                skills.push(skill);
            }
        }
    }
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(skills)
}

fn read_skill(dir: &Path, manifest: &Path) -> Option<Skill> {
    let content = fs::read_to_string(manifest).ok()?;
    let (name, description) = parse_frontmatter(&content);
    let fallback = dir.file_name()?.to_string_lossy().into_owned();
    Some(Skill {
        name: name.unwrap_or(fallback),
        description: description.unwrap_or_default(),
        path: dir.to_path_buf(),
    })
}

/// Minimal YAML-frontmatter reader for the two keys skills actually use.
/// Handles plain values and `>`/`>-` folded blocks.
fn parse_frontmatter(content: &str) -> (Option<String>, Option<String>) {
    let mut lines = content.lines();
    if lines.next().map(str::trim) != Some("---") {
        return (None, None);
    }
    let mut name = None;
    let mut description = None;
    let mut folding: Option<(&str, Vec<String>)> = None;
    for line in lines {
        if line.trim() == "---" {
            break;
        }
        if let Some((key, collected)) = &mut folding {
            if line.starts_with(' ') || line.trim().is_empty() {
                if !line.trim().is_empty() {
                    collected.push(line.trim().to_owned());
                }
                continue;
            }
            let value = collected.join(" ");
            match *key {
                "name" => name = Some(value),
                _ => description = Some(value),
            }
            folding = None;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        if key != "name" && key != "description" {
            continue;
        }
        let value = value.trim();
        if value == ">" || value == ">-" || value == "|" || value == "|-" {
            folding = Some((
                if key == "name" { "name" } else { "description" },
                Vec::new(),
            ));
        } else {
            let value = value.trim_matches(['"', '\'']).to_owned();
            if key == "name" {
                name = Some(value);
            } else {
                description = Some(value);
            }
        }
    }
    if let Some((key, collected)) = folding {
        let value = collected.join(" ");
        match key {
            "name" => name = Some(value),
            _ => description = Some(value),
        }
    }
    (name, description)
}

/// What happened to one skill during installation into one target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstallOutcome {
    Installed,
    Updated,
    /// A directory of the same name exists WITHOUT a Engram marker: it belongs
    /// to the user (or another tool) and is left untouched.
    SkippedForeign,
}

/// Install every skill of a pack into one agent target. Existing Engram-managed
/// directories are replaced; foreign directories are skipped.
///
/// # Errors
///
/// Returns filesystem errors and size-bound violations.
pub fn install_into(
    pack: &SkillPack,
    target: &AgentTarget,
) -> Result<Vec<(String, InstallOutcome)>> {
    let skills = discover_skills(&pack.path)?;
    fs::create_dir_all(&target.skills_dir)
        .map_err(|error| Error::Config(format!("skills dir: {error}")))?;
    let mut outcomes = Vec::new();
    for skill in skills {
        let destination = target.skills_dir.join(&skill.name);
        let outcome = if destination.exists() {
            if destination.join(MARKER_FILE).is_file()
                || destination.join(LEGACY_MARKER_FILE).is_file()
            {
                fs::remove_dir_all(&destination)
                    .map_err(|error| Error::Config(format!("replace skill: {error}")))?;
                copy_skill_dir(&skill.path, &destination)?;
                InstallOutcome::Updated
            } else {
                InstallOutcome::SkippedForeign
            }
        } else {
            copy_skill_dir(&skill.path, &destination)?;
            InstallOutcome::Installed
        };
        if outcome != InstallOutcome::SkippedForeign {
            fs::write(destination.join(MARKER_FILE), &pack.name)
                .map_err(|error| Error::Config(format!("marker: {error}")))?;
        }
        outcomes.push((skill.name, outcome));
    }
    Ok(outcomes)
}

/// Remove every installed skill belonging to `pack_name` from one target.
///
/// # Errors
///
/// Returns filesystem errors.
pub fn uninstall_from(pack_name: &str, target: &AgentTarget) -> Result<Vec<String>> {
    let mut removed = Vec::new();
    let Ok(entries) = fs::read_dir(&target.skills_dir) else {
        return Ok(removed);
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        let owner = fs::read_to_string(dir.join(MARKER_FILE))
            .or_else(|_| fs::read_to_string(dir.join(LEGACY_MARKER_FILE)))
            .unwrap_or_default();
        if owner.trim() == pack_name {
            fs::remove_dir_all(&dir)
                .map_err(|error| Error::Config(format!("remove skill: {error}")))?;
            removed.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    Ok(removed)
}

/// Bounded recursive copy of one skill directory (skips `.git`).
fn copy_skill_dir(source: &Path, destination: &Path) -> Result<()> {
    let mut entries = 0_usize;
    let mut bytes = 0_u64;
    copy_recursive(source, destination, &mut entries, &mut bytes)
}

fn copy_recursive(
    source: &Path,
    destination: &Path,
    entries: &mut usize,
    bytes: &mut u64,
) -> Result<()> {
    fs::create_dir_all(destination).map_err(|error| Error::Config(format!("copy: {error}")))?;
    let listing = fs::read_dir(source).map_err(|error| Error::Config(format!("copy: {error}")))?;
    for entry in listing.flatten() {
        let name = entry.file_name();
        if name == ".git" {
            continue;
        }
        *entries += 1;
        if *entries > MAX_SKILL_ENTRIES {
            return Err(Error::Config(format!(
                "skill directory {} exceeds {MAX_SKILL_ENTRIES} entries",
                source.display()
            )));
        }
        let from = entry.path();
        let to = destination.join(&name);
        if from.is_dir() {
            copy_recursive(&from, &to, entries, bytes)?;
        } else {
            let size = entry.metadata().map_or(0, |metadata| metadata.len());
            *bytes += size;
            if *bytes > MAX_SKILL_BYTES {
                return Err(Error::Config(format!(
                    "skill directory {} exceeds {MAX_SKILL_BYTES} bytes",
                    source.display()
                )));
            }
            fs::copy(&from, &to).map_err(|error| Error::Config(format!("copy: {error}")))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn write_skill(pack: &Path, name: &str, description: &str) {
        let dir = pack.join("skills").join(name);
        fs::create_dir_all(&dir).expect("skill dir");
        fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: >\n  {description}\n---\n\nBody of {name}.\n"),
        )
        .expect("skill manifest");
    }

    #[test]
    fn frontmatter_parses_plain_and_folded_values() {
        let (name, description) = parse_frontmatter(
            "---\nname: ponytail\ndescription: >\n  Forces the laziest solution\n  that works.\n---\nbody",
        );
        assert_eq!(name.as_deref(), Some("ponytail"));
        assert_eq!(
            description.as_deref(),
            Some("Forces the laziest solution that works.")
        );
        assert_eq!(parse_frontmatter("no frontmatter"), (None, None));
    }

    #[test]
    fn pack_names_derive_from_sources() {
        assert_eq!(
            pack_name_from_source("https://github.com/DietrichGebert/ponytail"),
            "ponytail"
        );
        assert_eq!(pack_name_from_source("git@host:team/tools.git"), "tools");
    }

    #[test]
    fn install_respects_foreign_skills_and_updates_owned_ones() {
        let temp = TempDir::new().expect("temp");
        let pack_path = temp.path().join("pack");
        write_skill(&pack_path, "alpha", "First skill.");
        write_skill(&pack_path, "beta", "Second skill.");
        let pack = SkillPack {
            name: "pack".to_owned(),
            path: pack_path.clone(),
            source: None,
        };
        let target = AgentTarget {
            label: "claude",
            skills_dir: temp.path().join("agent/skills"),
        };

        // A pre-existing hand-written skill named `beta` must survive.
        let foreign = target.skills_dir.join("beta");
        fs::create_dir_all(&foreign).expect("foreign");
        fs::write(foreign.join("SKILL.md"), "hand-written").expect("foreign manifest");

        let outcomes = install_into(&pack, &target).expect("install");
        assert!(
            outcomes
                .iter()
                .any(|(name, o)| name == "alpha" && *o == InstallOutcome::Installed)
        );
        assert!(
            outcomes
                .iter()
                .any(|(name, o)| name == "beta" && *o == InstallOutcome::SkippedForeign)
        );
        assert_eq!(
            fs::read_to_string(foreign.join("SKILL.md")).expect("survives"),
            "hand-written"
        );

        // Re-install updates the owned skill in place.
        let outcomes = install_into(&pack, &target).expect("reinstall");
        assert!(
            outcomes
                .iter()
                .any(|(name, o)| name == "alpha" && *o == InstallOutcome::Updated)
        );

        // Uninstall removes only marker-owned directories.
        let removed = uninstall_from("pack", &target).expect("uninstall");
        assert_eq!(removed, vec!["alpha".to_owned()]);
        assert!(foreign.exists());
    }

    #[test]
    fn discovery_reads_skills_dir_then_root_manifest() {
        let temp = TempDir::new().expect("temp");
        let pack = temp.path().join("pack");
        write_skill(&pack, "one", "Skill one.");
        let skills = discover_skills(&pack).expect("discover");
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "one");

        let root_only = temp.path().join("root-only");
        fs::create_dir_all(&root_only).expect("root");
        fs::write(
            root_only.join("SKILL.md"),
            "---\nname: solo\ndescription: Root skill.\n---\n",
        )
        .expect("root manifest");
        let skills = discover_skills(&root_only).expect("discover root");
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "solo");
    }
}
