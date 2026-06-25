//! SemVer pin classification and selection helpers.
//!
//! A `[deps.<name>] tag = "..."` value in `mlua-pkg.toml` is interpreted in
//! one of two ways:
//!
//! - **Exact** — a full SemVer string (`v1.2.3`, `1.0.0-rc1`).  The fetcher
//!   resolves it literally; `mlua-pkg update` leaves it alone unless
//!   `--force` is passed.
//! - **Prefix** — a partial version (`v1.0`, `v1`).  Both fetch and update
//!   resolve it to the SemVer-max matching release tag on the remote, so
//!   the manifest acts as a "follow latest patch / minor" specifier akin to
//!   Cargo `^X.Y` ranges.  The manifest is **not** rewritten on update.

/// Pin classification of a manifest `tag` value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TagPin {
    /// Full `MAJOR.MINOR.PATCH` SemVer, optionally with `v` prefix and/or
    /// pre-release/build metadata (e.g. `v1.2.3-rc1`).
    Exact,
    /// Partial SemVer (`v1.0`, `1`, `v2`).  The numeric components are
    /// returned in left-to-right order; e.g. `"v1.0"` → `[1, 0]`.
    Prefix(Vec<u64>),
}

/// Strip a single leading `v` or `V` from `s`.
pub fn strip_v_prefix(s: &str) -> &str {
    s.strip_prefix('v')
        .or_else(|| s.strip_prefix('V'))
        .unwrap_or(s)
}

/// Classify a manifest tag string.  Returns `None` for values that are
/// neither full SemVer nor pure numeric prefixes (e.g. `"latest"`,
/// `"feature-x"`); such pins are skipped on update and rejected for
/// auto-resolution on install.
pub fn classify_tag_pin(tag: &str) -> Option<TagPin> {
    let stripped = strip_v_prefix(tag);
    if semver::Version::parse(stripped).is_ok() {
        return Some(TagPin::Exact);
    }
    let parts: Option<Vec<u64>> = stripped.split('.').map(|s| s.parse::<u64>().ok()).collect();
    let parts = parts?;
    if parts.is_empty() || parts.len() >= 3 {
        return None;
    }
    Some(TagPin::Prefix(parts))
}

/// Pick the SemVer-max release tag from `tags` whose components match `pin`'s
/// prefix.  Pre-release tags are excluded.  The original tag string is
/// returned verbatim (preserving any `v` prefix) so that callers can use it
/// directly as a git ref name.
pub fn pick_latest_for_pin(tags: &[String], pin: &[u64]) -> Option<String> {
    let mut best: Option<(semver::Version, &String)> = None;
    for tag in tags {
        let stripped = strip_v_prefix(tag);
        let Ok(ver) = semver::Version::parse(stripped) else {
            continue;
        };
        if !ver.pre.is_empty() {
            continue;
        }
        let comps = [ver.major, ver.minor, ver.patch];
        if pin.len() > comps.len() {
            continue;
        }
        if pin.iter().zip(comps.iter()).any(|(p, c)| p != c) {
            continue;
        }
        match &best {
            None => best = Some((ver, tag)),
            Some((b, _)) if &ver > b => best = Some((ver, tag)),
            _ => {}
        }
    }
    best.map(|(_, t)| t.clone())
}

/// Pick the SemVer-max release tag overall.  Pre-release tags are excluded.
pub fn pick_latest_overall(tags: &[String]) -> Option<String> {
    let mut best: Option<(semver::Version, &String)> = None;
    for tag in tags {
        let stripped = strip_v_prefix(tag);
        let Ok(ver) = semver::Version::parse(stripped) else {
            continue;
        };
        if !ver.pre.is_empty() {
            continue;
        }
        match &best {
            None => best = Some((ver, tag)),
            Some((b, _)) if &ver > b => best = Some((ver, tag)),
            _ => {}
        }
    }
    best.map(|(_, t)| t.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_full_semver_is_exact() {
        assert_eq!(classify_tag_pin("v1.2.3"), Some(TagPin::Exact));
        assert_eq!(classify_tag_pin("1.2.3"), Some(TagPin::Exact));
        assert_eq!(classify_tag_pin("v0.1.0"), Some(TagPin::Exact));
        assert_eq!(classify_tag_pin("1.0.0-rc1"), Some(TagPin::Exact));
    }

    #[test]
    fn classify_partial_is_prefix() {
        assert_eq!(classify_tag_pin("v1.0"), Some(TagPin::Prefix(vec![1, 0])));
        assert_eq!(classify_tag_pin("v1"), Some(TagPin::Prefix(vec![1])));
        assert_eq!(classify_tag_pin("2.5"), Some(TagPin::Prefix(vec![2, 5])));
    }

    #[test]
    fn classify_non_semver_is_none() {
        assert_eq!(classify_tag_pin("latest"), None);
        assert_eq!(classify_tag_pin("release-candidate"), None);
        assert_eq!(classify_tag_pin(""), None);
    }

    #[test]
    fn pick_for_pin_takes_prefix_max() {
        let tags = vec![
            "v1.0.0".to_string(),
            "v1.0.1".to_string(),
            "v1.0.5".to_string(),
            "v1.1.0".to_string(),
            "v2.0.0".to_string(),
        ];
        assert_eq!(
            pick_latest_for_pin(&tags, &[1, 0]),
            Some("v1.0.5".to_string())
        );
        assert_eq!(pick_latest_for_pin(&tags, &[1]), Some("v1.1.0".to_string()));
        assert_eq!(pick_latest_for_pin(&tags, &[3]), None);
    }

    #[test]
    fn pick_for_pin_excludes_prerelease() {
        let tags = vec![
            "v1.0.0".to_string(),
            "v1.0.1-rc1".to_string(),
            "v1.0.1".to_string(),
        ];
        assert_eq!(
            pick_latest_for_pin(&tags, &[1, 0]),
            Some("v1.0.1".to_string())
        );
    }

    #[test]
    fn pick_for_pin_ignores_unrelated_tags() {
        let tags = vec![
            "v1.0.0".to_string(),
            "release-2024".to_string(),
            "v1.0.1".to_string(),
        ];
        assert_eq!(
            pick_latest_for_pin(&tags, &[1, 0]),
            Some("v1.0.1".to_string())
        );
    }

    #[test]
    fn pick_overall_takes_global_max() {
        let tags = vec![
            "v0.9.9".to_string(),
            "v1.0.5".to_string(),
            "v2.0.0".to_string(),
            "v1.9.9".to_string(),
        ];
        assert_eq!(pick_latest_overall(&tags), Some("v2.0.0".to_string()));
    }
}
