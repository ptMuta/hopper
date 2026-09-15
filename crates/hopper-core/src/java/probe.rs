//! Identifying a JVM that is already installed.
//!
//! Detection runs `java -XshowSettings:properties -version` and reads the property table it
//! prints to stderr. The alternative — parsing the free-form first line of `java -version` —
//! varies by vendor and has broken repeatedly for other tools; the property table is stable and
//! machine-readable.

use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProbeError {
    #[error("could not determine the Java version from this JVM's output")]
    NoVersion,
    #[error("unrecognised Java version string {0:?}")]
    BadVersion(String),
}

/// A JVM found on the machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemJava {
    pub path: PathBuf,
    pub major: u32,
    pub version: String,
    pub vendor: String,
    pub arch: String,
}

/// Extract `key = value` from the property table.
fn property<'a>(output: &'a str, key: &str) -> Option<&'a str> {
    output.lines().find_map(|line| {
        let (k, v) = line.split_once('=')?;
        (k.trim() == key).then(|| v.trim())
    })
}

/// Turn a Java specification version into a major number.
///
/// Handles the pre-9 `1.8` form, where the major version is the *second* component.
pub fn major_from_spec(spec: &str) -> Result<u32, ProbeError> {
    let spec = spec.trim();
    if let Some(rest) = spec.strip_prefix("1.") {
        return rest
            .split('.')
            .next()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| ProbeError::BadVersion(spec.to_owned()));
    }
    spec.split(['.', '-', '+'])
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| ProbeError::BadVersion(spec.to_owned()))
}

/// Parse the output of `java -XshowSettings:properties -version`.
pub fn parse_probe_output(
    path: impl Into<PathBuf>,
    output: &str,
) -> Result<SystemJava, ProbeError> {
    // `java.specification.version` is the clean major number; `java.version` is the full
    // string and is only a fallback.
    let major = match property(output, "java.specification.version") {
        Some(spec) => major_from_spec(spec)?,
        None => major_from_spec(property(output, "java.version").ok_or(ProbeError::NoVersion)?)?,
    };

    Ok(SystemJava {
        path: path.into(),
        major,
        version: property(output, "java.version")
            .unwrap_or_default()
            .to_owned(),
        vendor: property(output, "java.vendor")
            .unwrap_or_default()
            .to_owned(),
        arch: property(output, "os.arch").unwrap_or_default().to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEMURIN_21: &str = r#"Property settings:
    java.class.version = 65.0
    java.home = /usr/lib/jvm/temurin-21-jdk-amd64
    java.specification.version = 21
    java.vendor = Eclipse Adoptium
    java.version = 21.0.5
    os.arch = amd64
    os.name = Linux

openjdk version "21.0.5" 2024-10-15 LTS
"#;

    const GRAALVM_25: &str = r#"Property settings:
    java.specification.version = 25
    java.vendor = Oracle Corporation
    java.version = 25.0.4
    java.vm.name = Java HotSpot(TM) 64-Bit Server VM
    os.arch = aarch64

java version "25.0.4" 2026-07-15
"#;

    const JAVA_8: &str = r#"Property settings:
    java.specification.version = 1.8
    java.vendor = Oracle Corporation
    java.version = 1.8.0_402
    os.arch = amd64
"#;

    #[test]
    fn reads_a_modern_jvm() {
        let j = parse_probe_output("/usr/bin/java", TEMURIN_21).unwrap();
        assert_eq!(j.major, 21);
        assert_eq!(j.version, "21.0.5");
        assert_eq!(j.vendor, "Eclipse Adoptium");
        assert_eq!(j.arch, "amd64");
    }

    #[test]
    fn reads_graalvm() {
        let j = parse_probe_output("/opt/graalvm/bin/java", GRAALVM_25).unwrap();
        assert_eq!(j.major, 25);
        assert_eq!(j.arch, "aarch64");
    }

    #[test]
    fn handles_the_legacy_one_dot_eight_scheme() {
        // Java 8 reports "1.8"; the major version is the second component, not the first.
        let j = parse_probe_output("/usr/bin/java", JAVA_8).unwrap();
        assert_eq!(j.major, 8);
    }

    #[test]
    fn spec_parsing_covers_the_shapes_jvms_actually_emit() {
        assert_eq!(major_from_spec("21").unwrap(), 21);
        assert_eq!(major_from_spec("25").unwrap(), 25);
        assert_eq!(major_from_spec("1.8").unwrap(), 8);
        assert_eq!(major_from_spec("1.7.0").unwrap(), 7);
        assert_eq!(major_from_spec("21.0.5").unwrap(), 21);
        assert_eq!(major_from_spec("17-ea").unwrap(), 17);
        assert_eq!(major_from_spec("21+35").unwrap(), 21);
        assert_eq!(major_from_spec("  21  ").unwrap(), 21);
    }

    #[test]
    fn rejects_unparseable_versions() {
        assert!(major_from_spec("").is_err());
        assert!(major_from_spec("garbage").is_err());
    }

    #[test]
    fn falls_back_to_java_version_when_the_spec_property_is_absent() {
        let out = "    java.version = 17.0.9\n    os.arch = amd64\n";
        assert_eq!(parse_probe_output("/x", out).unwrap().major, 17);
    }

    #[test]
    fn output_with_no_version_at_all_is_an_error_not_a_guess() {
        assert_eq!(
            parse_probe_output("/x", "nothing useful here").unwrap_err(),
            ProbeError::NoVersion
        );
    }

    #[test]
    fn missing_optional_properties_do_not_fail_the_probe() {
        let j = parse_probe_output("/x", "    java.specification.version = 21\n").unwrap();
        assert_eq!(j.major, 21);
        assert_eq!(j.vendor, "");
    }

    #[test]
    fn a_property_value_containing_an_equals_sign_survives() {
        let out = "    java.specification.version = 21\n    java.vendor = A=B Corp\n";
        assert_eq!(parse_probe_output("/x", out).unwrap().vendor, "A=B Corp");
    }
}
