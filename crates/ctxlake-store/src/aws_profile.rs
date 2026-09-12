//! Read AWS credentials from `~/.aws/credentials`, the way every other AWS tool does.
//!
//! `object_store` 0.14 resolves S3 credentials from environment variables, EC2
//! instance metadata, ECS task roles and web identity — and **not** from the shared
//! credentials file. So on an ordinary laptop with a working `aws` CLI, ctxlake
//! ignored the credentials sitting in `~/.aws/credentials`, fell through to instance
//! metadata, spent ten retries and about sixteen seconds failing to reach
//! `169.254.169.254`, and reported the store as unreachable. Measured, not guessed.
//!
//! That gap matters most exactly where it is hardest to notice: a **service** has no
//! shell, so a daemon installed by `ctxlake sync install` inherits no exported keys.
//! A setup that works interactively (because the user happened to export them) then
//! fails on every maintenance cycle once supervised.
//!
//! This module closes it with the smallest thing that can work: an INI reader over
//! one file. It is deliberately not a reimplementation of the AWS credential chain —
//! no SSO token cache, no `credential_process`, no assume-role. Those all resolve to
//! *temporary* credentials that expire under a long-running daemon, and silently
//! serving a daemon expiring credentials is worse than telling someone plainly that
//! their SSO profile is not supported. [`load`] returns `None` for anything it does
//! not genuinely understand.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Static credentials for one profile. Never `Debug`-derived: a secret that can be
/// printed eventually is.
pub struct ProfileCredentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
    pub region: Option<String>,
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Which profile to read: `AWS_PROFILE`, else `default` — the same precedence the
/// AWS CLI uses, so ctxlake and `aws s3 ls` agree about which identity is in play.
fn profile_name() -> String {
    std::env::var("AWS_PROFILE").unwrap_or_else(|_| "default".to_string())
}

/// Parse an AWS-style INI file into `section -> key -> value`.
///
/// Hand-rolled rather than a dependency: this is one file format, twenty lines, and
/// adding an INI crate to a workspace that has none in order to read four keys is a
/// worse trade than owning the parser.
fn parse_ini(text: &str) -> HashMap<String, HashMap<String, String>> {
    let mut out: HashMap<String, HashMap<String, String>> = HashMap::new();
    let mut section = String::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            // `~/.aws/config` writes `[profile foo]` while `~/.aws/credentials`
            // writes `[foo]`. Normalising here lets one lookup serve both files.
            section = name
                .trim()
                .strip_prefix("profile ")
                .unwrap_or(name)
                .trim()
                .to_string();
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            out.entry(section.clone())
                .or_default()
                .insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    out
}

fn read_ini(path: &Path) -> HashMap<String, HashMap<String, String>> {
    std::fs::read_to_string(path)
        .map(|t| parse_ini(&t))
        .unwrap_or_default()
}

/// Credentials for the active profile, or `None`.
///
/// `None` when: the environment already supplies a key (which must win, so an
/// explicit `AWS_ACCESS_KEY_ID` is never quietly overridden by a stale file), there
/// is no credentials file, the profile is absent, or the profile is one of the forms
/// this module deliberately does not implement.
pub fn load() -> Option<ProfileCredentials> {
    if std::env::var_os("AWS_ACCESS_KEY_ID").is_some() {
        return None;
    }
    let home = home()?;
    let profile = profile_name();

    let creds = read_ini(&home.join(".aws").join("credentials"));
    let config = read_ini(&home.join(".aws").join("config"));
    let section = creds.get(&profile);

    let access_key_id = section?.get("aws_access_key_id")?.clone();
    let secret_access_key = section?.get("aws_secret_access_key")?.clone();
    if access_key_id.is_empty() || secret_access_key.is_empty() {
        return None;
    }

    // Region can live in either file; `credentials` wins because it is the more
    // specific of the two, matching the CLI.
    let region = section
        .and_then(|s| s.get("region"))
        .or_else(|| config.get(&profile).and_then(|s| s.get("region")))
        .cloned();

    Some(ProfileCredentials {
        access_key_id,
        secret_access_key,
        session_token: section.and_then(|s| s.get("aws_session_token")).cloned(),
        region,
    })
}

/// Why an S3 store might be unreachable despite a working `aws` CLI — for `doctor`.
///
/// Returns a sentence naming the unsupported mechanism, or `None` when nothing
/// obviously explains it. SSO is the common case by a wide margin.
pub fn unsupported_profile_hint() -> Option<String> {
    let home = home()?;
    let profile = profile_name();
    let config = read_ini(&home.join(".aws").join("config"));
    let section = config.get(&profile)?;
    for (key, mechanism) in [
        ("sso_session", "AWS SSO"),
        ("sso_start_url", "AWS SSO"),
        ("credential_process", "credential_process"),
        ("role_arn", "assume-role"),
    ] {
        if section.contains_key(key) {
            return Some(format!(
                "profile {profile:?} uses {mechanism}, which ctxlake does not read — \
                 it resolves static keys from ~/.aws/credentials, environment variables, \
                 or an instance role. Export credentials into the environment, or use a \
                 profile with static keys."
            ));
        }
    }
    None
}

/// Which credential source ctxlake will actually use, in words, for `doctor`.
///
/// The single most useful line in a failed S3 diagnosis, and the one that was
/// missing: a 403 tells you the request was signed and rejected, but not *whose*
/// identity signed it. On a machine with more than one AWS account configured —
/// which is most machines — "Access Denied" and "you are using the wrong profile"
/// look identical.
pub fn describe_source() -> String {
    if std::env::var_os("AWS_ACCESS_KEY_ID").is_some() {
        return "environment variables (AWS_ACCESS_KEY_ID)".to_string();
    }
    let profile = profile_name();
    let via = if std::env::var_os("AWS_PROFILE").is_some() {
        "AWS_PROFILE"
    } else {
        "the default, since AWS_PROFILE is not set"
    };
    if load().is_some() {
        format!("profile {profile:?} from ~/.aws/credentials ({via})")
    } else {
        format!(
            "no static credentials found for profile {profile:?} ({via}) — \
             falling back to an instance or container role"
        )
    }
}

/// Turn a store error into an actionable sentence when it is recognisably about
/// credentials rather than the network.
///
/// Returns `None` for anything it cannot confidently classify: a wrong guess here
/// sends someone to fix the thing that was not broken, which is worse than the raw
/// error they would otherwise have read.
pub fn diagnose(detail: &str) -> Option<String> {
    let source = describe_source();
    if detail.contains("403") || detail.contains("AccessDenied") {
        return Some(format!(
            "this is an authorization failure, not a network one — the request was \
             signed and refused.\n      Signed with: {source}\n      Either that \
             identity lacks s3:PutObject/GetObject/ListBucket on this bucket, or it is \
             the wrong account. Credentials resolve the way the AWS CLI resolves them: \
             environment first, then ~/.aws/credentials, then an instance role."
        ));
    }
    if detail.contains("169.254.169.254") {
        return Some(format!(
            "no credentials were found, so it tried the EC2 instance metadata service \
             and timed out.\n      Looked for: {source}"
        ));
    }
    if detail.contains("NoSuchBucket") || detail.contains("404") {
        return Some(
            "the bucket does not exist, or is in a different region than the \
                     one resolved."
                .to_string(),
        );
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_section_spellings_parse_to_the_same_profile_name() {
        // `~/.aws/config` uses `[profile foo]`, `~/.aws/credentials` uses `[foo]`.
        // Reading them into different keys is how a region silently goes missing.
        let c = parse_ini("[profile foo]\nregion = us-west-2\n");
        assert_eq!(c["foo"]["region"], "us-west-2");
        let d = parse_ini("[foo]\naws_access_key_id = AKIAEXAMPLE\n");
        assert_eq!(d["foo"]["aws_access_key_id"], "AKIAEXAMPLE");
    }

    #[test]
    fn comments_blank_lines_and_padding_are_tolerated() {
        let c = parse_ini(
            "; a comment\n\n[default]\n  aws_access_key_id  =  AKIAEXAMPLE  \n# another\n",
        );
        assert_eq!(c["default"]["aws_access_key_id"], "AKIAEXAMPLE");
    }

    #[test]
    fn keys_are_matched_case_insensitively() {
        let c = parse_ini("[default]\nAWS_Access_Key_Id = AKIAEXAMPLE\n");
        assert_eq!(c["default"]["aws_access_key_id"], "AKIAEXAMPLE");
    }

    #[test]
    fn a_value_containing_an_equals_sign_survives() {
        // Secret access keys are base64 and routinely end in '='. Splitting on every
        // '=' instead of the first would truncate the secret and produce a signature
        // error that looks nothing like its cause.
        let c = parse_ini("[default]\naws_secret_access_key = abc/def+ghi=\n");
        assert_eq!(c["default"]["aws_secret_access_key"], "abc/def+ghi=");
    }

    #[test]
    fn a_403_is_diagnosed_as_authorization_not_as_a_network_problem() {
        let d = diagnose("Server returned non-2xx status code: 403 Forbidden: AccessDenied")
            .expect("a 403 must be classified");
        assert!(d.contains("authorization failure"), "{d}");
        assert!(
            d.contains("~/.aws/credentials"),
            "it must name where credentials come from: {d}"
        );
    }

    #[test]
    fn an_imds_timeout_is_diagnosed_as_missing_credentials() {
        let d = diagnose("Error performing PUT http://169.254.169.254/latest/api/token")
            .expect("an IMDS timeout must be classified");
        assert!(d.contains("no credentials were found"), "{d}");
    }

    #[test]
    fn an_unrecognised_error_is_left_alone() {
        // Guessing sends someone to fix what was not broken.
        assert_eq!(diagnose("connection reset by peer"), None);
        assert_eq!(diagnose(""), None);
    }

    #[test]
    fn an_sso_profile_is_reported_rather_than_half_read() {
        // An SSO profile has no static keys, so `load` must decline it — and the
        // hint has to name SSO, because "store unreachable" sends someone hunting
        // for a network problem they do not have.
        let c = parse_ini("[profile work]\nsso_session = corp\nregion = us-west-2\n");
        assert!(c["work"].contains_key("sso_session"));
        assert!(!c["work"].contains_key("aws_access_key_id"));
    }
}
