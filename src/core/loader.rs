//! Detecting the mod loader installed in a server root.
//!
//! packctl does not install mod loaders. When a pack declares the loader it
//! needs, the plan compares it with what the server has, so a mismatch is
//! visible before the server is restarted on the wrong loader.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::core::planner::PlanNotice;
use crate::providers::{LoaderKind, PackRequirements};

/// A loader found in a server root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedLoader {
    pub kind: LoaderKind,
    /// The installed version, when the server layout shows it.
    pub version: Option<String>,
}

impl DetectedLoader {
    /// `NeoForge 21.1.77`, or `Fabric (unknown version)`.
    pub fn describe(&self) -> String {
        match &self.version {
            Some(version) => format!("{} {version}", self.kind),
            None => format!("{} (unknown version)", self.kind),
        }
    }
}

/// How a library folder names its version subdirectories.
#[derive(Clone, Copy)]
enum VersionDirs {
    /// `<loader version>`.
    Plain,
    /// `<minecraft>-<loader version>`.
    MinecraftPrefixed,
}

/// Library folders whose subdirectories are loader versions, in detection
/// order. NeoForge for Minecraft 1.20.1 still used Forge's layout under the
/// `net.neoforged` group (`libraries/net/neoforged/forge/1.20.1-47.1.106/`).
const LIBRARY_DIRS: &[(LoaderKind, &[&str], VersionDirs)] = &[
    (
        LoaderKind::NeoForge,
        &["libraries", "net", "neoforged", "neoforge"],
        VersionDirs::Plain,
    ),
    (
        LoaderKind::NeoForge,
        &["libraries", "net", "neoforged", "forge"],
        VersionDirs::MinecraftPrefixed,
    ),
    (
        LoaderKind::Forge,
        &["libraries", "net", "minecraftforge", "forge"],
        VersionDirs::MinecraftPrefixed,
    ),
    (
        LoaderKind::Quilt,
        &["libraries", "org", "quiltmc", "quilt-loader"],
        VersionDirs::Plain,
    ),
    (
        LoaderKind::Fabric,
        &["libraries", "net", "fabricmc", "fabric-loader"],
        VersionDirs::Plain,
    ),
];

/// Finds the loader installed in `server_root`.
///
/// NeoForge, Forge, Quilt and Fabric are recognised by their library folders
/// (`libraries/net/neoforged/neoforge/<v>/`, for Minecraft 1.20.1
/// `libraries/net/neoforged/forge/<mc>-<v>/`, `libraries/net/minecraftforge/
/// forge/<mc>-<v>/`, `libraries/org/quiltmc/quilt-loader/<v>/`,
/// `libraries/net/fabricmc/fabric-loader/<v>/`); when several versions are
/// present, the most recently modified wins. Without libraries, a
/// `quilt-server-launch.jar`, or a `.fabric/` folder or
/// `fabric-server-launch.jar`, identifies the loader without a version.
pub fn detect_installed_loader(server_root: &Path) -> Option<DetectedLoader> {
    for (kind, parts, dirs) in LIBRARY_DIRS {
        let dir: PathBuf = parts
            .iter()
            .fold(server_root.to_path_buf(), |p, s| p.join(s));
        if let Some(name) = newest_subdir(&dir) {
            let version = match dirs {
                VersionDirs::MinecraftPrefixed => name
                    .split_once('-')
                    .map(|(_, loader)| loader.to_string())
                    .unwrap_or(name),
                VersionDirs::Plain => name,
            };
            return Some(DetectedLoader {
                kind: *kind,
                version: Some(version),
            });
        }
    }
    if server_root.join("quilt-server-launch.jar").is_file() {
        return Some(DetectedLoader {
            kind: LoaderKind::Quilt,
            version: None,
        });
    }
    if server_root.join(".fabric").is_dir()
        || server_root.join("fabric-server-launch.jar").is_file()
    {
        return Some(DetectedLoader {
            kind: LoaderKind::Fabric,
            version: None,
        });
    }
    None
}

/// The name of the most recently modified subdirectory of `dir`.
fn newest_subdir(dir: &Path) -> Option<String> {
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|entry| {
            let name = entry.file_name().to_str()?.to_string();
            let modified = entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            Some((modified, name))
        })
        .max()
        .map(|(_, name)| name)
}

/// The plan notice comparing the loader a pack needs with the one found.
///
/// A different loader, or the same loader at a different known version, is
/// a mismatch; no loader at all is "not detected". The same loader whose
/// version cannot be read is no mismatch, since packctl cannot tell: it
/// yields a "Loader version unknown" notice shown only with `--verbose`.
pub fn loader_notice(
    req: &PackRequirements,
    detected: Option<&DetectedLoader>,
) -> Option<PlanNotice> {
    let wanted = req.loader.as_ref()?;
    let Some(found) = detected else {
        return Some(PlanNotice::general(format!(
            "Loader not detected\n  Pack requires {} {}; packctl could not find it in the server root.\n  packctl does not install mod loaders.",
            wanted.kind, wanted.version
        )));
    };
    if found.kind == wanted.kind {
        match found.version.as_deref() {
            Some(version) if version == wanted.version => return None,
            Some(_) => {}
            None => {
                return Some(PlanNotice {
                    verbose_only: true,
                    ..PlanNotice::general(format!(
                        "Loader version unknown\n  Pack requires {} {}; server has {}, but packctl could not read its version.",
                        wanted.kind, wanted.version, found.kind
                    ))
                });
            }
        }
    }
    Some(PlanNotice::general(format!(
        "Loader mismatch\n  Pack requires {} {}; server has {}.\n  packctl does not install mod loaders. Update the loader (for example in your panel) before starting the server.",
        wanted.kind,
        wanted.version,
        found.describe()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::LoaderRequirement;

    fn mkdir(root: &Path, rel: &str) {
        std::fs::create_dir_all(root.join(rel)).unwrap();
    }

    fn requires(kind: LoaderKind, version: &str) -> PackRequirements {
        PackRequirements {
            minecraft: Some("1.21.1".to_string()),
            loader: Some(LoaderRequirement {
                kind,
                version: version.to_string(),
            }),
        }
    }

    #[test]
    fn detects_neoforge_forge_fabric_quilt_from_fixture_trees() {
        let neoforge = tempfile::tempdir().unwrap();
        mkdir(neoforge.path(), "libraries/net/neoforged/neoforge/21.1.77");
        assert_eq!(
            detect_installed_loader(neoforge.path()),
            Some(DetectedLoader {
                kind: LoaderKind::NeoForge,
                version: Some("21.1.77".to_string())
            })
        );

        let forge = tempfile::tempdir().unwrap();
        mkdir(
            forge.path(),
            "libraries/net/minecraftforge/forge/1.20.1-47.3.0",
        );
        assert_eq!(
            detect_installed_loader(forge.path()),
            Some(DetectedLoader {
                kind: LoaderKind::Forge,
                version: Some("47.3.0".to_string())
            })
        );

        let fabric = tempfile::tempdir().unwrap();
        mkdir(
            fabric.path(),
            "libraries/net/fabricmc/fabric-loader/0.16.10",
        );
        assert_eq!(
            detect_installed_loader(fabric.path()),
            Some(DetectedLoader {
                kind: LoaderKind::Fabric,
                version: Some("0.16.10".to_string())
            })
        );

        let fabric_launcher = tempfile::tempdir().unwrap();
        std::fs::write(fabric_launcher.path().join("fabric-server-launch.jar"), b"").unwrap();
        assert_eq!(
            detect_installed_loader(fabric_launcher.path()),
            Some(DetectedLoader {
                kind: LoaderKind::Fabric,
                version: None
            })
        );

        let fabric_dir = tempfile::tempdir().unwrap();
        mkdir(fabric_dir.path(), ".fabric/remappedJars");
        assert_eq!(
            detect_installed_loader(fabric_dir.path()).map(|d| d.kind),
            Some(LoaderKind::Fabric)
        );

        let quilt = tempfile::tempdir().unwrap();
        mkdir(quilt.path(), "libraries/org/quiltmc/quilt-loader/0.27.1");
        assert_eq!(
            detect_installed_loader(quilt.path()),
            Some(DetectedLoader {
                kind: LoaderKind::Quilt,
                version: Some("0.27.1".to_string())
            })
        );

        let quilt_launcher = tempfile::tempdir().unwrap();
        std::fs::write(quilt_launcher.path().join("quilt-server-launch.jar"), b"").unwrap();
        assert_eq!(
            detect_installed_loader(quilt_launcher.path()),
            Some(DetectedLoader {
                kind: LoaderKind::Quilt,
                version: None
            })
        );

        let vanilla = tempfile::tempdir().unwrap();
        mkdir(vanilla.path(), "libraries/com/mojang");
        assert_eq!(detect_installed_loader(vanilla.path()), None);
    }

    #[test]
    fn newest_version_directory_wins() {
        let root = tempfile::tempdir().unwrap();
        mkdir(root.path(), "libraries/net/neoforged/neoforge/21.1.50");
        std::thread::sleep(std::time::Duration::from_millis(50));
        mkdir(root.path(), "libraries/net/neoforged/neoforge/21.1.77");
        assert_eq!(
            detect_installed_loader(root.path())
                .unwrap()
                .version
                .as_deref(),
            Some("21.1.77")
        );
    }

    #[test]
    fn notice_on_mismatch_and_missing() {
        let req = requires(LoaderKind::NeoForge, "21.1.77");
        let older = DetectedLoader {
            kind: LoaderKind::NeoForge,
            version: Some("21.1.50".to_string()),
        };
        let notice = loader_notice(&req, Some(&older)).unwrap();
        assert_eq!(
            notice.message,
            "Loader mismatch\n  Pack requires NeoForge 21.1.77; server has NeoForge 21.1.50.\n  packctl does not install mod loaders. Update the loader (for example in your panel) before starting the server."
        );
        assert!(!notice.verbose_only);

        let fabric = DetectedLoader {
            kind: LoaderKind::Fabric,
            version: None,
        };
        assert!(
            loader_notice(&req, Some(&fabric))
                .unwrap()
                .message
                .contains("server has Fabric (unknown version).")
        );

        let missing = loader_notice(&req, None).unwrap();
        assert_eq!(
            missing.message,
            "Loader not detected\n  Pack requires NeoForge 21.1.77; packctl could not find it in the server root.\n  packctl does not install mod loaders."
        );
        assert!(!missing.verbose_only);
    }

    #[test]
    fn same_loader_with_unknown_version_is_a_verbose_note() {
        let req = requires(LoaderKind::Fabric, "0.16.10");
        let unknown = DetectedLoader {
            kind: LoaderKind::Fabric,
            version: None,
        };
        let notice = loader_notice(&req, Some(&unknown)).unwrap();
        assert!(notice.verbose_only);
        assert_eq!(
            notice.message,
            "Loader version unknown\n  Pack requires Fabric 0.16.10; server has Fabric, but packctl could not read its version."
        );
    }

    #[test]
    fn detects_neoforge_for_minecraft_1_20_1() {
        let root = tempfile::tempdir().unwrap();
        mkdir(root.path(), "libraries/net/neoforged/forge/1.20.1-47.1.106");
        assert_eq!(
            detect_installed_loader(root.path()),
            Some(DetectedLoader {
                kind: LoaderKind::NeoForge,
                version: Some("47.1.106".to_string())
            })
        );
        let req = requires(LoaderKind::NeoForge, "47.1.106");
        let detected = detect_installed_loader(root.path());
        assert_eq!(loader_notice(&req, detected.as_ref()), None);
    }

    #[test]
    fn none_when_matching() {
        let req = requires(LoaderKind::Fabric, "0.16.10");
        let exact = DetectedLoader {
            kind: LoaderKind::Fabric,
            version: Some("0.16.10".to_string()),
        };
        assert_eq!(loader_notice(&req, Some(&exact)), None);
        // No loader requirement: nothing to compare.
        let no_loader = PackRequirements {
            minecraft: Some("1.21.1".to_string()),
            loader: None,
        };
        assert_eq!(loader_notice(&no_loader, None), None);
    }
}
