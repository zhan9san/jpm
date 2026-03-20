/// Jenkins uses non-standard version strings like:
///   1.234.v5678abc
///   681.vf91669a_32e45
///   2.19-rc289.d09828a
///   525.v2458b_d8a_1a_71
///
/// Comparison strategy:
/// 1. Split on `-` first to separate the "release" part from any "pre-release" suffix.
/// 2. Compare the release parts segment-by-segment (splitting on `.`).
/// 3. If the release parts are equal, a version WITHOUT a pre-release suffix is
///    greater (i.e. `2.19` > `2.19-rc289`) — matching semver pre-release semantics.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct JenkinsVersion(pub String);

impl JenkinsVersion {
    pub fn new(s: impl Into<String>) -> Self {
        JenkinsVersion(s.into())
    }
}

impl std::fmt::Display for JenkinsVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Split a version string into its release part and optional pre-release suffix.
/// `2.19-rc289.d09828a` → `("2.19", Some("rc289.d09828a"))`
/// `2.452.4`             → `("2.452.4", None)`
fn split_prerelease(v: &str) -> (&str, Option<&str>) {
    match v.split_once('-') {
        Some((main, pre)) => (main, Some(pre)),
        None => (v, None),
    }
}

/// Compare two dot-separated version strings segment by segment.
fn compare_dot_segments(a: &str, b: &str) -> std::cmp::Ordering {
    let segs_a = dot_segments(a);
    let segs_b = dot_segments(b);
    let len = segs_a.len().max(segs_b.len());
    for i in 0..len {
        let ord = match (segs_a.get(i), segs_b.get(i)) {
            (Some(x), Some(y)) => x.cmp(y),
            (Some(_), None) => std::cmp::Ordering::Greater,
            (None, Some(_)) => std::cmp::Ordering::Less,
            (None, None) => std::cmp::Ordering::Equal,
        };
        if ord != std::cmp::Ordering::Equal {
            return ord;
        }
    }
    std::cmp::Ordering::Equal
}

/// Parse a dot-separated string into a list of comparable segments.
/// Each `.`-part is further split on numeric/alpha boundaries so that
/// `vf91669a32e45` → `[Str("v"), Num(91669), Str("a"), Num(32), Str("e"), Num(45)]`
/// and `525` → `[Num(525)]`.
fn dot_segments(v: &str) -> Vec<Segment> {
    v.split('.')
        .flat_map(split_numeric_alpha)
        .collect()
}

fn split_numeric_alpha(s: &str) -> Vec<Segment> {
    if s.is_empty() {
        return vec![];
    }
    let mut result = Vec::new();
    let mut buf = String::new();
    let mut is_numeric = s.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false);

    for c in s.chars() {
        let c_numeric = c.is_ascii_digit();
        if c_numeric != is_numeric {
            if !buf.is_empty() {
                result.push(if is_numeric {
                    Segment::Num(buf.parse().unwrap_or(0))
                } else {
                    Segment::Str(buf.clone())
                });
                buf.clear();
            }
            is_numeric = c_numeric;
        }
        buf.push(c);
    }
    if !buf.is_empty() {
        result.push(if is_numeric {
            Segment::Num(buf.parse().unwrap_or(0))
        } else {
            Segment::Str(buf)
        });
    }
    result
}

#[derive(Debug, Eq, PartialEq)]
enum Segment {
    Num(u64),
    Str(String),
}

impl Ord for Segment {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        match (self, other) {
            (Segment::Num(a), Segment::Num(b)) => a.cmp(b),
            (Segment::Str(a), Segment::Str(b)) => a.cmp(b),
            // numeric > string so that "289" > "rc289" (release > pre-release label)
            (Segment::Num(_), Segment::Str(_)) => std::cmp::Ordering::Greater,
            (Segment::Str(_), Segment::Num(_)) => std::cmp::Ordering::Less,
        }
    }
}

impl PartialOrd for Segment {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for JenkinsVersion {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        let (main_a, pre_a) = split_prerelease(&self.0);
        let (main_b, pre_b) = split_prerelease(&other.0);

        let main_ord = compare_dot_segments(main_a, main_b);
        if main_ord != std::cmp::Ordering::Equal {
            return main_ord;
        }

        // Same main version: release > pre-release, matching semver semantics.
        match (pre_a, pre_b) {
            (None, None) => std::cmp::Ordering::Equal,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (Some(_), None) => std::cmp::Ordering::Less,
            (Some(a), Some(b)) => compare_dot_segments(a, b),
        }
    }
}

impl PartialOrd for JenkinsVersion {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_ordering() {
        assert!(JenkinsVersion::new("2.0") > JenkinsVersion::new("1.9"));
        assert!(JenkinsVersion::new("1.10") > JenkinsVersion::new("1.9"));
    }

    #[test]
    fn jenkins_style_versions() {
        assert!(
            JenkinsVersion::new("681.vf91669a32e45") > JenkinsVersion::new("525.v2458bd8a1a71")
        );
    }

    #[test]
    fn rc_is_less_than_release() {
        assert!(JenkinsVersion::new("2.19") > JenkinsVersion::new("2.19-rc289.d09828a"));
    }

    #[test]
    fn equal_versions() {
        assert_eq!(JenkinsVersion::new("1.2.3"), JenkinsVersion::new("1.2.3"));
    }

    #[test]
    fn longer_main_version() {
        assert!(JenkinsVersion::new("2.452.4") > JenkinsVersion::new("2.452.3"));
        assert!(JenkinsVersion::new("2.452.4") > JenkinsVersion::new("2.452"));
    }
}
