//! Modrinth **shared instances** — experimental, and unofficial.
//!
//! Shared instances are what the Modrinth app calls its "share this instance" feature: an
//! instance with exact version ids pinned, shared by link or invite. That makes it the closest
//! thing to a private mini-modpack, which is why it is worth supporting.
//!
//! # Why this is behind a feature flag
//!
//! Unlike every other source hopper reads, this one has **no public API**. What is known comes
//! from reading the Modrinth app's own open-source client:
//!
//! * The base URL is supplied to the app at build time through an environment variable, so
//!   there is no documented endpoint to point at — hopper requires the operator to supply one.
//! * Authentication is a Modrinth *session* bearer token, not a personal access token. The PAT
//!   scope list does contain `SHARED_INSTANCE_*` entries, which suggests PATs may be intended
//!   to work, so hopper tries a PAT and reports precisely what came back.
//! * The request and response shapes below are reconstructed from the client's types. They are
//!   not a contract and can change without notice.
//!
//! Everything here is therefore built to fail loudly and legibly rather than to paper over a
//! protocol we do not control. Nothing in the default build touches it.

use serde::Deserialize;

use crate::model::{ProjectId, VersionId};

/// Environment variable naming the shared-instances backend.
///
/// There is no default: guessing a host and sending someone's token to it would be worse than
/// refusing, so hopper will not proceed without an explicit value.
pub const BASE_URL_ENV: &str = "HOPPER_SHARED_INSTANCES_URL";

/// Environment variable holding the Modrinth token.
pub const TOKEN_ENV: &str = "MODRINTH_TOKEN";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SharedError {
    #[error(
        "shared instances have no public API, so hopper cannot guess where to ask.\n\
         Set {BASE_URL_ENV} to the backend the Modrinth app uses."
    )]
    NoBaseUrl,
    #[error(
        "shared instances need a Modrinth token. Set {TOKEN_ENV}.\n\
         Note the app uses a session token; a personal access token may not be accepted."
    )]
    NoToken,
    #[error(
        "the shared instances backend rejected the token ({status}).\n\
         This feature is unofficial: the app authenticates with a session token, and a personal \
         access token may not work even with SHARED_INSTANCE scopes."
    )]
    Unauthorized { status: u16 },
    #[error("the shared instances backend returned an unexpected shape: {0}")]
    UnexpectedShape(String),
    #[error("shared instance {0:?} was not found, or is not shared with this account")]
    NotFound(String),
}

/// Configuration, read from the environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedConfig {
    pub base_url: String,
    pub token: String,
}

impl SharedConfig {
    pub fn from_env() -> Result<Self, SharedError> {
        Self::new(
            std::env::var(BASE_URL_ENV).ok().as_deref(),
            std::env::var(TOKEN_ENV).ok().as_deref(),
        )
    }

    /// The validation, as a pure function.
    ///
    /// Reading the environment is kept to the one-line wrapper above so the rules can be tested
    /// without mutating process state, which races under a parallel test runner.
    pub fn new(base_url: Option<&str>, token: Option<&str>) -> Result<Self, SharedError> {
        let base_url = base_url
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .ok_or(SharedError::NoBaseUrl)?;
        let token = token
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .ok_or(SharedError::NoToken)?;
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_owned(),
            token: token.to_owned(),
        })
    }

    /// The install-preview endpoint, from the app's client.
    pub fn install_url(&self, id: &str) -> String {
        format!("{}/v1/instance/{id}", self.base_url)
    }
}

/// Never let a token reach a log line or an error message.
pub struct Redacted<T>(pub T);

impl<T> std::fmt::Debug for Redacted<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

impl<T> std::fmt::Display for Redacted<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

/// One pinned file in a shared instance.
///
/// Version ids being pinned is the whole reason this is interesting: unlike a collection, the
/// sharer's exact choices come through.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SharedFile {
    pub project_id: ProjectId,
    pub version_id: VersionId,
    #[serde(default)]
    pub file_name: Option<String>,
}

/// What the backend says an instance contains.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SharedInstance {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub game_version: Option<String>,
    #[serde(default)]
    pub loader: Option<String>,
    #[serde(default)]
    pub loader_version: Option<String>,
    /// Pinned content. The field name is a reconstruction and may differ.
    #[serde(default, alias = "files", alias = "contents")]
    pub versions: Vec<SharedFile>,
}

/// Parse an install preview.
pub fn parse_instance(body: &[u8]) -> Result<SharedInstance, SharedError> {
    serde_json::from_slice(body).map_err(|e| SharedError::UnexpectedShape(e.to_string()))
}

/// Map an HTTP status to something that explains the situation.
pub fn interpret_status(status: u16, id: &str) -> Option<SharedError> {
    match status {
        200..=299 => None,
        401 | 403 => Some(SharedError::Unauthorized { status }),
        404 => Some(SharedError::NotFound(id.to_owned())),
        other => Some(SharedError::UnexpectedShape(format!("HTTP {other}"))),
    }
}

/// The warning shown before any shared-instance request.
///
/// Stated every time, deliberately: someone whose install breaks in six months should be able
/// to tell immediately that they were relying on something unofficial.
pub const EXPERIMENTAL_WARNING: &str = "warning: shared instances are experimental. Modrinth publishes no API for them, so \
     this is reconstructed from their app's source and may stop working without notice.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_base_url_refuses_rather_than_guessing_a_host() {
        // Guessing would mean posting someone's token to a host we invented.
        assert_eq!(
            SharedConfig::new(None, Some("mrp_test")).unwrap_err(),
            SharedError::NoBaseUrl
        );
        assert_eq!(
            SharedConfig::new(Some("   "), Some("mrp_test")).unwrap_err(),
            SharedError::NoBaseUrl
        );
    }

    #[test]
    fn a_missing_token_is_refused_too() {
        assert_eq!(
            SharedConfig::new(Some("https://example.test"), None).unwrap_err(),
            SharedError::NoToken
        );
    }

    #[test]
    fn the_error_says_what_to_set() {
        let e = SharedError::NoBaseUrl;
        assert!(e.to_string().contains(BASE_URL_ENV));
        assert!(e.to_string().contains("no public API"));
    }

    #[test]
    fn an_auth_failure_explains_the_session_versus_pat_distinction() {
        // The most likely failure, and the least obvious one.
        let e = SharedError::Unauthorized { status: 401 };
        assert!(e.to_string().contains("session token"));
        assert!(e.to_string().contains("personal access token"));
    }

    #[test]
    fn urls_are_built_without_doubling_slashes() {
        let c = SharedConfig {
            base_url: "https://example.test".into(),
            token: "t".into(),
        };
        assert_eq!(c.install_url("abc"), "https://example.test/v1/instance/abc");
    }

    #[test]
    fn a_trailing_slash_on_the_base_url_is_tolerated() {
        let c = SharedConfig::new(Some("https://example.test/"), Some("mrp_test")).unwrap();
        assert_eq!(c.install_url("x"), "https://example.test/v1/instance/x");
    }

    #[test]
    fn tokens_never_appear_in_debug_or_display_output() {
        let r = Redacted("mrp_supersecret");
        assert_eq!(format!("{r:?}"), "<redacted>");
        assert_eq!(format!("{r}"), "<redacted>");
        assert!(!format!("{r:?}{r}").contains("supersecret"));
    }

    #[test]
    fn parses_the_reconstructed_instance_shape() {
        let body = serde_json::json!({
            "id": "abc123",
            "name": "My SMP",
            "game_version": "26.3",
            "loader": "fabric",
            "loader_version": "0.17.2",
            "versions": [
                { "project_id": "AANobbMI", "version_id": "SMxNOGZ6", "file_name": "sodium.jar" }
            ]
        });
        let i = parse_instance(body.to_string().as_bytes()).unwrap();
        assert_eq!(i.name, "My SMP");
        assert_eq!(i.game_version.as_deref(), Some("26.3"));
        // Exact version ids are pinned, which is what distinguishes this from a collection.
        assert_eq!(i.versions[0].version_id.as_str(), "SMxNOGZ6");
    }

    #[test]
    fn tolerates_the_field_name_being_different_than_we_guessed() {
        // The shape is reconstructed, so the likely names are accepted rather than one guess.
        for key in ["versions", "files", "contents"] {
            let body = serde_json::json!({
                "id": "x", "name": "n",
                key: [{ "project_id": "P", "version_id": "V" }]
            });
            let i = parse_instance(body.to_string().as_bytes()).unwrap();
            assert_eq!(i.versions.len(), 1, "{key} should be accepted");
        }
    }

    #[test]
    fn an_unrecognised_shape_is_reported_not_silently_empty() {
        // Returning an empty instance would look like a successful install of nothing.
        assert!(matches!(
            parse_instance(b"[1,2,3]").unwrap_err(),
            SharedError::UnexpectedShape(_)
        ));
    }

    #[test]
    fn statuses_map_to_actionable_errors() {
        assert!(interpret_status(200, "x").is_none());
        assert!(matches!(
            interpret_status(401, "x"),
            Some(SharedError::Unauthorized { .. })
        ));
        assert!(matches!(
            interpret_status(403, "x"),
            Some(SharedError::Unauthorized { .. })
        ));
        assert!(matches!(
            interpret_status(404, "abc"),
            Some(SharedError::NotFound(id)) if id == "abc"
        ));
        assert!(interpret_status(500, "x").is_some());
    }

    #[test]
    fn the_experimental_warning_says_it_may_break() {
        assert!(EXPERIMENTAL_WARNING.contains("experimental"));
        assert!(EXPERIMENTAL_WARNING.contains("without notice"));
    }
}
