//! Installing a Modrinth collection.
//!
//! A collection is a curated list of **projects**, with no versions pinned and no Minecraft
//! version or loader declared — Modrinth are explicit that collections are not modpacks. So
//! unlike a `.mrpack`, which tells us exactly what to install, a collection leaves every choice
//! to the client.
//!
//! The interesting problem is picking a `(Minecraft version, loader)` pair. Asking the operator
//! to supply both before they can see whether anything is compatible is a poor trade, so the
//! default is to solve for the pair covering the most projects and show the trade-off.

use std::collections::BTreeMap;

use crate::api::modrinth::WireVersion;
use crate::model::{LoaderKind, MinecraftVersion, ProjectId};

/// How well a candidate target covers a collection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Coverage {
    pub minecraft: MinecraftVersion,
    pub loader: LoaderKind,
    /// Projects with at least one version for this target.
    pub covered: usize,
    pub total: usize,
    /// Projects with nothing for this target, so the operator can judge whether it matters.
    pub missing: Vec<ProjectId>,
}

impl Coverage {
    pub fn is_complete(&self) -> bool {
        self.missing.is_empty()
    }

    pub fn fraction(&self) -> f64 {
        if self.total == 0 {
            return 1.0;
        }
        self.covered as f64 / self.total as f64
    }
}

/// Rank candidate targets by how much of the collection they support.
///
/// Ordering is coverage first, then newer Minecraft versions, using the order the caller
/// supplied `mc_candidates` in — version strings are opaque and must not be compared.
pub fn rank_targets(
    versions_by_project: &BTreeMap<ProjectId, Vec<WireVersion>>,
    mc_candidates: &[MinecraftVersion],
    loaders: &[LoaderKind],
) -> Vec<Coverage> {
    let total = versions_by_project.len();
    let mut out = Vec::new();

    for (mc_rank, mc) in mc_candidates.iter().enumerate() {
        for loader in loaders {
            let mut covered = 0;
            let mut missing = Vec::new();
            for (project, versions) in versions_by_project {
                if versions.iter().any(|v| supports(v, mc, *loader)) {
                    covered += 1;
                } else {
                    missing.push(project.clone());
                }
            }
            out.push((
                mc_rank,
                Coverage {
                    minecraft: mc.clone(),
                    loader: *loader,
                    covered,
                    total,
                    missing,
                },
            ));
        }
    }

    // Best coverage wins; ties go to the earlier (newer) Minecraft candidate.
    out.sort_by(|a, b| b.1.covered.cmp(&a.1.covered).then(a.0.cmp(&b.0)));
    out.into_iter().map(|(_, c)| c).collect()
}

fn supports(v: &WireVersion, mc: &MinecraftVersion, loader: LoaderKind) -> bool {
    let loader_ok =
        loader == LoaderKind::Vanilla || v.loaders.iter().any(|l| l == loader.api_name());
    loader_ok && v.game_versions.iter().any(|g| g == mc.as_str())
}

/// Every Minecraft version any project in the collection supports, newest first.
///
/// "Newest" is the order Modrinth returns versions in, not a parsed comparison: Minecraft
/// version strings cannot be ordered by parsing them.
pub fn candidate_minecraft_versions(
    versions_by_project: &BTreeMap<ProjectId, Vec<WireVersion>>,
) -> Vec<MinecraftVersion> {
    let mut seen = Vec::new();
    for versions in versions_by_project.values() {
        for v in versions {
            for g in &v.game_versions {
                let mc = MinecraftVersion::new(g.clone());
                if !seen.contains(&mc) {
                    seen.push(mc);
                }
            }
        }
    }
    seen
}

/// Pick one version per project for a chosen target.
///
/// Prefers releases, so a collection does not quietly pull in alphas.
pub fn select_versions(
    versions_by_project: &BTreeMap<ProjectId, Vec<WireVersion>>,
    mc: &MinecraftVersion,
    loader: LoaderKind,
) -> BTreeMap<ProjectId, WireVersion> {
    use crate::api::modrinth::VersionType;

    let mut out = BTreeMap::new();
    for (project, versions) in versions_by_project {
        let compatible: Vec<&WireVersion> = versions
            .iter()
            .filter(|v| supports(v, mc, loader))
            .collect();
        let chosen = [
            Some(VersionType::Release),
            Some(VersionType::Beta),
            Some(VersionType::Alpha),
        ]
        .iter()
        .find_map(|kind| compatible.iter().find(|v| v.version_type == *kind).copied())
        .or_else(|| compatible.first().copied());

        if let Some(v) = chosen {
            out.insert(project.clone(), v.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(mc: &[&str], loaders: &[&str], kind: &str) -> WireVersion {
        serde_json::from_value(serde_json::json!({
            "id": format!("v-{kind}-{}", mc.join("_")),
            "project_id": "P",
            "version_number": "1.0.0",
            "version_type": kind,
            "game_versions": mc,
            "loaders": loaders,
            "files": [],
            "dependencies": [],
        }))
        .unwrap()
    }

    fn project(id: &str, versions: Vec<WireVersion>) -> (ProjectId, Vec<WireVersion>) {
        (ProjectId::new(id), versions)
    }

    #[test]
    fn ranks_the_target_covering_the_most_projects_first() {
        let map: BTreeMap<_, _> = [
            project("a", vec![version(&["26.3"], &["fabric"], "release")]),
            project("b", vec![version(&["26.3"], &["fabric"], "release")]),
            project("c", vec![version(&["26.2"], &["fabric"], "release")]),
        ]
        .into_iter()
        .collect();

        let ranked = rank_targets(
            &map,
            &[MinecraftVersion::new("26.3"), MinecraftVersion::new("26.2")],
            &[LoaderKind::Fabric],
        );
        assert_eq!(ranked[0].minecraft.as_str(), "26.3");
        assert_eq!(ranked[0].covered, 2);
        assert_eq!(ranked[0].total, 3);
        assert!(!ranked[0].is_complete());
    }

    #[test]
    fn a_complete_target_beats_a_newer_incomplete_one() {
        // Covering everything on an older version is usually what someone wants over covering
        // most things on the newest.
        let map: BTreeMap<_, _> = [
            project(
                "a",
                vec![
                    version(&["26.3"], &["fabric"], "release"),
                    version(&["26.2"], &["fabric"], "release"),
                ],
            ),
            project("b", vec![version(&["26.2"], &["fabric"], "release")]),
        ]
        .into_iter()
        .collect();

        let ranked = rank_targets(
            &map,
            &[MinecraftVersion::new("26.3"), MinecraftVersion::new("26.2")],
            &[LoaderKind::Fabric],
        );
        assert_eq!(ranked[0].minecraft.as_str(), "26.2");
        assert!(ranked[0].is_complete());
    }

    #[test]
    fn ties_prefer_the_newer_minecraft_version() {
        let map: BTreeMap<_, _> = [project(
            "a",
            vec![
                version(&["26.3"], &["fabric"], "release"),
                version(&["26.2"], &["fabric"], "release"),
            ],
        )]
        .into_iter()
        .collect();

        let ranked = rank_targets(
            &map,
            &[MinecraftVersion::new("26.3"), MinecraftVersion::new("26.2")],
            &[LoaderKind::Fabric],
        );
        assert_eq!(ranked[0].minecraft.as_str(), "26.3");
    }

    #[test]
    fn loaders_are_ranked_too() {
        let map: BTreeMap<_, _> = [
            project("a", vec![version(&["26.3"], &["fabric"], "release")]),
            project("b", vec![version(&["26.3"], &["fabric"], "release")]),
            project("c", vec![version(&["26.3"], &["neoforge"], "release")]),
        ]
        .into_iter()
        .collect();

        let ranked = rank_targets(
            &map,
            &[MinecraftVersion::new("26.3")],
            &[LoaderKind::Fabric, LoaderKind::NeoForge],
        );
        assert_eq!(ranked[0].loader, LoaderKind::Fabric);
        assert_eq!(ranked[0].covered, 2);
    }

    #[test]
    fn missing_projects_are_named_so_the_choice_is_informed() {
        let map: BTreeMap<_, _> = [
            project("keeps-up", vec![version(&["26.3"], &["fabric"], "release")]),
            project(
                "lags-behind",
                vec![version(&["26.1"], &["fabric"], "release")],
            ),
        ]
        .into_iter()
        .collect();

        let ranked = rank_targets(
            &map,
            &[MinecraftVersion::new("26.3")],
            &[LoaderKind::Fabric],
        );
        assert_eq!(ranked[0].missing, vec![ProjectId::new("lags-behind")]);
    }

    #[test]
    fn candidate_versions_come_from_the_collection_itself() {
        let map: BTreeMap<_, _> = [
            project(
                "a",
                vec![version(&["26.3", "26.2"], &["fabric"], "release")],
            ),
            project("b", vec![version(&["26.2"], &["fabric"], "release")]),
        ]
        .into_iter()
        .collect();
        let candidates = candidate_minecraft_versions(&map);
        assert!(candidates.contains(&MinecraftVersion::new("26.3")));
        assert!(candidates.contains(&MinecraftVersion::new("26.2")));
        assert_eq!(candidates.len(), 2, "deduplicated");
    }

    #[test]
    fn selection_prefers_releases_over_prereleases() {
        let map: BTreeMap<_, _> = [project(
            "a",
            vec![
                version(&["26.3"], &["fabric"], "alpha"),
                version(&["26.3"], &["fabric"], "release"),
            ],
        )]
        .into_iter()
        .collect();

        let chosen = select_versions(&map, &MinecraftVersion::new("26.3"), LoaderKind::Fabric);
        assert_eq!(
            chosen[&ProjectId::new("a")].version_type,
            Some(crate::api::modrinth::VersionType::Release)
        );
    }

    #[test]
    fn a_project_with_only_a_beta_is_still_selected() {
        let map: BTreeMap<_, _> = [project("a", vec![version(&["26.3"], &["fabric"], "beta")])]
            .into_iter()
            .collect();
        let chosen = select_versions(&map, &MinecraftVersion::new("26.3"), LoaderKind::Fabric);
        assert_eq!(chosen.len(), 1);
    }

    #[test]
    fn incompatible_projects_are_simply_absent_from_the_selection() {
        let map: BTreeMap<_, _> = [
            project("fits", vec![version(&["26.3"], &["fabric"], "release")]),
            project(
                "does-not",
                vec![version(&["1.16.5"], &["forge"], "release")],
            ),
        ]
        .into_iter()
        .collect();

        let chosen = select_versions(&map, &MinecraftVersion::new("26.3"), LoaderKind::Fabric);
        assert_eq!(chosen.len(), 1);
        assert!(chosen.contains_key(&ProjectId::new("fits")));
    }

    #[test]
    fn an_empty_collection_is_trivially_covered() {
        let ranked = rank_targets(
            &BTreeMap::new(),
            &[MinecraftVersion::new("26.3")],
            &[LoaderKind::Fabric],
        );
        assert_eq!(ranked[0].total, 0);
        assert!(ranked[0].is_complete());
        assert_eq!(ranked[0].fraction(), 1.0);
    }
}
