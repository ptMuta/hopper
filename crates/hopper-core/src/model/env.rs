//! Which side of the game a mod belongs on.
//!
//! This is the module that stops client mods reaching a dedicated server. It is harder than it
//! looks, because every available signal is unreliable in a different way:
//!
//! * A `.mrpack`'s `env` block is optional and frequently absent or copy-pasted.
//! * Modrinth's legacy `client_side`/`server_side` pair was ambiguous enough that Modrinth
//!   themselves say it "led to many confused creators mislabeling their mods", and their
//!   validation once *forced* incorrect values for mods that were genuinely optional on a
//!   server.
//! * The newer per-version `environment` field is better, but carries `unknown` and is only as
//!   good as the author's care.
//! * Fabric jars self-declare a side that the loader actually enforces — but Forge and NeoForge
//!   deliberately removed whole-mod sidedness from their manifests, so no equivalent exists.
//!
//! So rather than ranking sources flatly, signals carry a weight keyed by **(source, claim)**.
//! The same source can be strong evidence in one direction and worthless in the other, and a
//! flat ranking cannot express that.

use serde::{Deserialize, Serialize};

/// Modrinth's per-version environment classification.
///
/// Values are the full set the API can return; the mapping to a server decision is the
/// interesting part and is spelled out per variant rather than inferred from the name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VersionEnvironment {
    /// Needed on both sides to work at all.
    ClientAndServer,
    /// Client-exclusive. Explicitly documented as compatible with vanilla servers, i.e. safe to
    /// omit rather than fatal to leave out.
    ClientOnly,
    /// Client needs it; a server copy adds enhanced behaviour but is not required.
    ClientOnlyServerOptional,
    /// Only works in the integrated server, so a dedicated server cannot use it.
    SingleplayerOnly,
    /// Server-exclusive (also works in singleplayer, which runs an internal server).
    ServerOnly,
    /// Server needs it; a client copy improves the experience.
    ServerOnlyClientOptional,
    /// Dedicated servers specifically, excluding singleplayer.
    DedicatedServerOnly,
    /// Fully functional on either side alone.
    ClientOrServer,
    /// Works on either side alone, better with both.
    ClientOrServerPrefersBoth,
    /// Not yet classified by the author. Proves nothing in either direction.
    Unknown,
}

/// What a signal asserts about installing on a dedicated server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvClaim {
    /// The server needs this.
    ServerRequired,
    /// The server can use this, but does not need it.
    ServerOptional,
    /// The server must not have this.
    ServerUnsupported,
    /// This signal says nothing useful. Crucially distinct from `ServerRequired`.
    Neutral,
}

impl VersionEnvironment {
    pub fn claim(self) -> EnvClaim {
        match self {
            Self::ClientAndServer
            | Self::ServerOnly
            | Self::ServerOnlyClientOptional
            | Self::DedicatedServerOnly
            | Self::ClientOrServer
            | Self::ClientOrServerPrefersBoth => EnvClaim::ServerRequired,
            Self::ClientOnlyServerOptional => EnvClaim::ServerOptional,
            // A dedicated server has no integrated server, so singleplayer-only code cannot run.
            Self::ClientOnly | Self::SingleplayerOnly => EnvClaim::ServerUnsupported,
            Self::Unknown => EnvClaim::Neutral,
        }
    }
}

/// Legacy per-project side support.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SideSupport {
    Required,
    Optional,
    Unsupported,
    Unknown,
}

impl SideSupport {
    /// Read the legacy pair. Only an explicit `unsupported` is treated as a negative; the rest
    /// is weak evidence at best, which the weight table reflects.
    pub fn claim_for_server(server: Self) -> EnvClaim {
        match server {
            Self::Required => EnvClaim::ServerRequired,
            Self::Optional => EnvClaim::ServerOptional,
            Self::Unsupported => EnvClaim::ServerUnsupported,
            Self::Unknown => EnvClaim::Neutral,
        }
    }
}

/// A jar's own declaration of its side.
///
/// Fabric puts `"environment": "client" | "server" | "*"` in `fabric.mod.json` at the jar root,
/// and Fabric Loader *enforces* it — a `client` jar simply does not load on a dedicated server.
/// That makes it the strongest automated signal available, but only for Fabric and Quilt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JarSide {
    Client,
    Server,
    /// `"*"` — the template default emitted by every mod generator.
    Both,
}

impl JarSide {
    /// The single most important rule in this module.
    ///
    /// `"*"` is what you get when nobody thought about it, so it is `Neutral`, **not**
    /// `ServerRequired`. Treating the default as a positive assertion is precisely how client
    /// mods slip onto servers — and because `Neutral` never causes a skip either, a pack where
    /// two hundred mods declare `"*"` is not gutted: they fall through to registry metadata.
    pub fn claim(self) -> EnvClaim {
        match self {
            Self::Client => EnvClaim::ServerUnsupported,
            Self::Server => EnvClaim::ServerRequired,
            Self::Both => EnvClaim::Neutral,
        }
    }
}

/// The shape of a Fabric mod's entrypoints.
///
/// A signal nobody else uses, and a good one: a mod whose only entrypoints are `client` (or
/// `modmenu`) has no server-side code to run, regardless of what `environment` claims. Strong
/// evidence against, weak evidence for — having a `main` entrypoint does not prove a mod is
/// server-safe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntrypointShape {
    /// Only client-side entrypoints.
    ClientOnly,
    /// Has `main` or `server` entrypoints.
    ServerOrMain,
    Mixed,
    None,
}

impl EntrypointShape {
    pub fn claim(self) -> EnvClaim {
        match self {
            Self::ClientOnly => EnvClaim::ServerUnsupported,
            Self::ServerOrMain | Self::Mixed => EnvClaim::ServerRequired,
            Self::None => EnvClaim::Neutral,
        }
    }
}

/// Where a signal came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvSource {
    /// `--force-include` / `--force-exclude`, or the operator's config.
    UserRule,
    /// The jar's own `fabric.mod.json` / `quilt.mod.json` declaration.
    JarManifest,
    /// The jar's entrypoint shape.
    JarEntrypoints,
    /// Curated correction data for known-mislabelled mods.
    KnownList,
    /// Registry per-version `environment`.
    RegistryVersionEnv,
    /// Registry legacy `client_side` / `server_side`.
    RegistryProjectSide,
    /// The pack's own `files[].env.server`.
    PackEnv,
    /// The destination path (`shaderpacks/`, `options.txt`).
    PathRule,
    /// An included server mod requires this one.
    DependencyPressure,
    /// Nothing said anything.
    Default,
}

/// One piece of evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvSignal {
    pub source: EnvSource,
    pub claim: EnvClaim,
    pub weight: u16,
    pub note: String,
}

/// How much each `(source, claim)` pair is worth.
///
/// Two asymmetries carry most of the value:
///
/// * **`PackEnv`.** `env.server: "unsupported"` is a deliberate statement about *this* pack and
///   authors rarely mark a working server mod unsupported by accident, so it scores above the
///   project-level registry fields. `required`/`optional`, by contrast, is the copy-paste
///   default present in nearly every generated pack, so it barely outranks nothing at all.
/// * **`JarEntrypoints`.** Strong against, weak for, for the reason given on its type.
pub fn weight_for(source: EnvSource, claim: EnvClaim) -> u16 {
    use EnvClaim::*;
    use EnvSource::*;
    match (source, claim) {
        // An escape hatch that can be overruled is not an escape hatch.
        (UserRule, _) => 250,

        // Loader-enforced ground truth beats any claim made about the mod elsewhere.
        (JarManifest, ServerUnsupported) => 200,
        (JarManifest, _) => 190,

        (KnownList, ServerUnsupported) => 180,
        (KnownList, _) => 175,

        (DependencyPressure, ServerRequired) => 160,
        (DependencyPressure, _) => 0,

        (JarEntrypoints, ServerUnsupported) => 150,
        (JarEntrypoints, _) => 60,

        (RegistryVersionEnv, ServerUnsupported) => 140,
        (RegistryVersionEnv, _) => 130,

        (PackEnv, ServerUnsupported) => 135,
        (PackEnv, _) => 40,

        (PathRule, ServerUnsupported) => 100,
        (PathRule, _) => 0,

        (RegistryProjectSide, ServerUnsupported) => 90,
        (RegistryProjectSide, _) => 80,

        (Default, _) => 10,
    }
}

impl EnvSignal {
    pub fn new(source: EnvSource, claim: EnvClaim, note: impl Into<String>) -> Self {
        Self {
            source,
            claim,
            weight: weight_for(source, claim),
            note: note.into(),
        }
    }
}

/// What to do with a candidate file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvDecision {
    Install,
    Skip,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvOutcome {
    pub decision: EnvDecision,
    pub winner: EnvSignal,
    pub conflicting: Vec<EnvSignal>,
    pub confidence: Confidence,
}

/// Weigh the evidence.
///
/// Highest weight wins. On a tie between conflicting claims, `ServerUnsupported` wins and
/// confidence drops to `Low`, because the failure costs are asymmetric: a wrongly included
/// client mod crashes the server at boot, while a wrongly excluded server mod leaves a running
/// server missing a feature, shows up in the skip report, and is one flag away from being
/// fixed.
///
/// That bias is only safe because `Neutral` never causes a skip — the operator's decision was
/// "warn, don't skip", so only an explicit negative claim can remove a file.
pub fn decide(signals: &[EnvSignal]) -> EnvOutcome {
    let fallback = EnvSignal::new(EnvSource::Default, EnvClaim::ServerRequired, "no signal");
    if signals.is_empty() {
        return EnvOutcome {
            decision: EnvDecision::Install,
            winner: fallback,
            conflicting: Vec::new(),
            confidence: Confidence::Low,
        };
    }

    // Neutral signals are evidence of nothing and must not outvote a real claim.
    let meaningful: Vec<&EnvSignal> = signals
        .iter()
        .filter(|s| s.claim != EnvClaim::Neutral)
        .collect();
    if meaningful.is_empty() {
        return EnvOutcome {
            decision: EnvDecision::Install,
            winner: fallback,
            conflicting: Vec::new(),
            confidence: Confidence::Low,
        };
    }

    let top = meaningful.iter().map(|s| s.weight).max().unwrap_or(0);
    let tied: Vec<&&EnvSignal> = meaningful.iter().filter(|s| s.weight == top).collect();

    let any_negative = tied.iter().any(|s| s.claim == EnvClaim::ServerUnsupported);
    let winner: &EnvSignal = if any_negative {
        tied.iter()
            .find(|s| s.claim == EnvClaim::ServerUnsupported)
            .unwrap()
    } else {
        tied[0]
    };

    let was_tie_between_claims = tied.iter().any(|s| s.claim != winner.claim);
    let conflicting: Vec<EnvSignal> = meaningful
        .iter()
        .filter(|s| s.claim != winner.claim)
        .map(|s| (*s).clone())
        .collect();

    let decision = match winner.claim {
        EnvClaim::ServerUnsupported => EnvDecision::Skip,
        _ => EnvDecision::Install,
    };

    let confidence = if was_tie_between_claims {
        Confidence::Low
    } else if winner.weight >= 140 && conflicting.is_empty() {
        Confidence::High
    } else if winner.weight >= 100 {
        Confidence::Medium
    } else {
        Confidence::Low
    };

    EnvOutcome {
        decision,
        winner: winner.clone(),
        conflicting,
        confidence,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_environment_value_maps_to_a_deliberate_claim() {
        use VersionEnvironment::*;
        assert_eq!(ClientOnly.claim(), EnvClaim::ServerUnsupported);
        assert_eq!(SingleplayerOnly.claim(), EnvClaim::ServerUnsupported);
        assert_eq!(ClientAndServer.claim(), EnvClaim::ServerRequired);
        assert_eq!(ServerOnly.claim(), EnvClaim::ServerRequired);
        assert_eq!(DedicatedServerOnly.claim(), EnvClaim::ServerRequired);
        assert_eq!(ClientOrServer.claim(), EnvClaim::ServerRequired);
        assert_eq!(ClientOnlyServerOptional.claim(), EnvClaim::ServerOptional);
        // `unknown` proves nothing; it must not read as a yes.
        assert_eq!(Unknown.claim(), EnvClaim::Neutral);
    }

    #[test]
    fn a_fabric_wildcard_environment_is_neutral_not_a_yes() {
        // The highest-leverage rule here: `"*"` is the generator default, not an assertion.
        assert_eq!(JarSide::Both.claim(), EnvClaim::Neutral);
        assert_eq!(JarSide::Client.claim(), EnvClaim::ServerUnsupported);
        assert_eq!(JarSide::Server.claim(), EnvClaim::ServerRequired);
    }

    #[test]
    fn pack_env_is_weighted_asymmetrically() {
        // "unsupported" is deliberate; "required" is boilerplate.
        assert!(
            weight_for(EnvSource::PackEnv, EnvClaim::ServerUnsupported)
                > weight_for(EnvSource::RegistryProjectSide, EnvClaim::ServerUnsupported)
        );
        assert!(
            weight_for(EnvSource::PackEnv, EnvClaim::ServerRequired)
                < weight_for(EnvSource::RegistryProjectSide, EnvClaim::ServerRequired)
        );
    }

    #[test]
    fn jar_entrypoints_are_strong_against_and_weak_for() {
        assert!(
            weight_for(EnvSource::JarEntrypoints, EnvClaim::ServerUnsupported)
                > weight_for(EnvSource::JarEntrypoints, EnvClaim::ServerRequired)
        );
    }

    #[test]
    fn nothing_outranks_a_user_rule() {
        let top = [
            EnvSource::JarManifest,
            EnvSource::KnownList,
            EnvSource::RegistryVersionEnv,
            EnvSource::PackEnv,
        ]
        .into_iter()
        .map(|s| weight_for(s, EnvClaim::ServerUnsupported))
        .max()
        .unwrap();
        assert!(weight_for(EnvSource::UserRule, EnvClaim::ServerRequired) > top);
    }

    #[test]
    fn no_signals_means_install() {
        // The operator chose warn-don't-skip: silence is not grounds for removing a file.
        let out = decide(&[]);
        assert_eq!(out.decision, EnvDecision::Install);
        assert_eq!(out.confidence, Confidence::Low);
    }

    #[test]
    fn only_neutral_signals_still_means_install() {
        // A pack where every mod declares `"*"` must not be gutted.
        let out = decide(&[
            EnvSignal::new(EnvSource::JarManifest, EnvClaim::Neutral, "environment: *"),
            EnvSignal::new(EnvSource::RegistryVersionEnv, EnvClaim::Neutral, "unknown"),
        ]);
        assert_eq!(out.decision, EnvDecision::Install);
    }

    #[test]
    fn a_jar_declaring_client_beats_registry_metadata_claiming_otherwise() {
        // The jar is what the loader acts on; the API is a claim about the jar.
        let out = decide(&[
            EnvSignal::new(
                EnvSource::JarManifest,
                EnvClaim::ServerUnsupported,
                "client",
            ),
            EnvSignal::new(
                EnvSource::RegistryVersionEnv,
                EnvClaim::ServerRequired,
                "client_and_server",
            ),
        ]);
        assert_eq!(out.decision, EnvDecision::Skip);
        assert_eq!(out.winner.source, EnvSource::JarManifest);
        assert_eq!(out.conflicting.len(), 1);
        // Disagreement means we are less sure, even though we acted.
        assert_eq!(out.confidence, Confidence::Medium);
    }

    #[test]
    fn a_user_rule_overrides_even_the_jar() {
        let out = decide(&[
            EnvSignal::new(
                EnvSource::JarManifest,
                EnvClaim::ServerUnsupported,
                "client",
            ),
            EnvSignal::new(
                EnvSource::UserRule,
                EnvClaim::ServerRequired,
                "--force-include",
            ),
        ]);
        assert_eq!(out.decision, EnvDecision::Install);
        assert_eq!(out.winner.source, EnvSource::UserRule);
    }

    #[test]
    fn a_tie_between_opposing_claims_resolves_against_installing() {
        // Asymmetric costs: a client mod on a server crashes it at boot, while a missing server
        // mod is visible in the report and one flag from being fixed.
        let out = decide(&[
            EnvSignal::new(EnvSource::KnownList, EnvClaim::ServerUnsupported, "a"),
            EnvSignal {
                weight: weight_for(EnvSource::KnownList, EnvClaim::ServerUnsupported),
                ..EnvSignal::new(EnvSource::KnownList, EnvClaim::ServerRequired, "b")
            },
        ]);
        assert_eq!(out.decision, EnvDecision::Skip);
        assert_eq!(out.confidence, Confidence::Low, "a tie is never confident");
    }

    #[test]
    fn unanimous_strong_evidence_is_high_confidence() {
        let out = decide(&[EnvSignal::new(
            EnvSource::RegistryVersionEnv,
            EnvClaim::ServerUnsupported,
            "client_only",
        )]);
        assert_eq!(out.decision, EnvDecision::Skip);
        assert_eq!(out.confidence, Confidence::High);
    }

    #[test]
    fn weak_lone_evidence_is_not_high_confidence() {
        let out = decide(&[EnvSignal::new(
            EnvSource::RegistryProjectSide,
            EnvClaim::ServerUnsupported,
            "server_side: unsupported",
        )]);
        assert_eq!(out.decision, EnvDecision::Skip);
        assert!(out.confidence < Confidence::High);
    }

    #[test]
    fn dependency_pressure_can_rescue_a_weak_skip_but_not_a_jar_declaration() {
        // A server mod hard-depending on this is good evidence it loads server-side...
        let rescued = decide(&[
            EnvSignal::new(
                EnvSource::RegistryProjectSide,
                EnvClaim::ServerUnsupported,
                "legacy metadata",
            ),
            EnvSignal::new(
                EnvSource::DependencyPressure,
                EnvClaim::ServerRequired,
                "required by an installed server mod",
            ),
        ]);
        assert_eq!(rescued.decision, EnvDecision::Install);

        // ...but it cannot overrule the jar's own enforced declaration.
        let not_rescued = decide(&[
            EnvSignal::new(
                EnvSource::JarManifest,
                EnvClaim::ServerUnsupported,
                "client",
            ),
            EnvSignal::new(
                EnvSource::DependencyPressure,
                EnvClaim::ServerRequired,
                "required by an installed server mod",
            ),
        ]);
        assert_eq!(not_rescued.decision, EnvDecision::Skip);
    }

    #[test]
    fn legacy_side_fields_only_treat_unsupported_as_negative() {
        assert_eq!(
            SideSupport::claim_for_server(SideSupport::Unsupported),
            EnvClaim::ServerUnsupported
        );
        assert_eq!(
            SideSupport::claim_for_server(SideSupport::Unknown),
            EnvClaim::Neutral
        );
        assert_eq!(
            SideSupport::claim_for_server(SideSupport::Optional),
            EnvClaim::ServerOptional
        );
    }
}
