use std::cmp::Ordering;

/// Jenkins uses non-standard version strings like:
///   1.234.v5678abc
///   681.vf91669a_32e45
///   2.19-rc289.d09828a
///   1.0.9-28.vf251ce40855d
///
/// Ordering is a port of `hudson.util.VersionNumber` (Maven's
/// `ComparableVersion` as adapted by Jenkins), which is what Jenkins core uses
/// to compare plugin versions:
/// - the version is split into numeric and string items on `.`, `-` and
///   digit/letter boundaries; trailing zeros and empty qualifiers are
///   insignificant (`1.0.9.0` == `1.0.9`);
/// - a `-` between two digits opens a sub-list, which sorts above the bare
///   release (`3.1.18-1` > `3.1.18`, `1.0.9-28.vf251ce40855d` > `1.0.9.0`);
/// - known qualifiers order as `snapshot < alpha < beta < milestone < rc <
///   (release) < sp`, unknown ones sort after them lexically
///   (`2.0-SNAPSHOT` < `2.0-beta-1` < `2.0-rc1` < `2.0`).
#[derive(Debug, Clone)]
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

/// Known qualifiers in ascending order; `""` is the release itself.
const QUALIFIERS: [&str; 7] = ["snapshot", "alpha", "beta", "milestone", "rc", "", "sp"];
/// `comparable_qualifier("")`, i.e. the position of a plain release.
const RELEASE_VERSION_INDEX: &str = "5";

#[derive(Debug)]
enum Item {
    /// Decimal digits with leading zeros stripped, so `""` is zero. Kept as a
    /// string so arbitrarily long numbers compare correctly.
    Int(String),
    /// A qualifier after alias normalisation (`ga`/`final` → `""`, `cr`/`ea` → `rc`).
    Str(String),
    /// A sub-list opened by `-` between two digits.
    List(Vec<Item>),
    Wildcard,
}

impl Item {
    fn int(digits: &str) -> Item {
        Item::Int(digits.trim_start_matches('0').to_string())
    }

    fn string(value: &str, followed_by_digit: bool) -> Item {
        let value = match value {
            // a1 = alpha-1, b1 = beta-1, m1 = milestone-1
            "a" if followed_by_digit => "alpha",
            "b" if followed_by_digit => "beta",
            "m" if followed_by_digit => "milestone",
            v => v,
        };
        let value = match value {
            "ga" | "final" => "",
            "cr" | "ea" => "rc",
            v => v,
        };
        Item::Str(value.to_string())
    }

    fn parse(is_digit: bool, buf: &str) -> Item {
        if is_digit {
            Item::int(buf)
        } else {
            Item::string(buf, false)
        }
    }

    fn is_null(&self) -> bool {
        match self {
            Item::Int(n) => n.is_empty(),
            Item::Str(s) => comparable_qualifier(s) == RELEASE_VERSION_INDEX,
            Item::List(l) => l.is_empty(),
            Item::Wildcard => false,
        }
    }

    /// Compare against the item at the same position in the other version,
    /// where `None` means the other version has no item there.
    fn compare(&self, other: Option<&Item>) -> Ordering {
        use Item::*;
        match (self, other) {
            // 1.0 == 1, 1.1 > 1
            (Int(n), None) => {
                if n.is_empty() {
                    Ordering::Equal
                } else {
                    Ordering::Greater
                }
            }
            (Int(a), Some(Int(b))) => a.len().cmp(&b.len()).then_with(|| a.cmp(b)),
            // 1.1 > 1-sp, 1.1 > 1-1
            (Int(_), Some(Str(_) | List(_))) => Ordering::Greater,
            // VersionNumber returns 0 here but 1 for the mirrored comparison;
            // use the mirror so `Ord` stays antisymmetric.
            (Int(_), Some(Wildcard)) => Ordering::Less,

            // 1-rc < 1, 1-ga == 1, 1-sp > 1
            (Str(s), None) => comparable_qualifier(s).as_str().cmp(RELEASE_VERSION_INDEX),
            (Str(a), Some(Str(b))) => comparable_qualifier(a).cmp(&comparable_qualifier(b)),
            (Str(_), Some(Int(_) | List(_) | Wildcard)) => Ordering::Less,

            // 1-0 == 1, otherwise decided by the first item
            (List(l), None) => l
                .first()
                .map_or(Ordering::Equal, |first| first.compare(None)),
            (List(a), Some(List(b))) => compare_lists(a, b),
            // 1-1 < 1.0.x, 1-1 > 1-sp
            (List(_), Some(Int(_) | Wildcard)) => Ordering::Less,
            (List(_), Some(Str(_))) => Ordering::Greater,

            // 1.* > 1.99 > 1
            (Wildcard, Some(Wildcard)) => Ordering::Equal,
            (Wildcard, _) => Ordering::Greater,
        }
    }
}

/// Map a qualifier to a string whose lexical order is the qualifier order:
/// known qualifiers become their index, unknown ones sort after all of them.
fn comparable_qualifier(qualifier: &str) -> String {
    match QUALIFIERS.iter().position(|q| *q == qualifier) {
        Some(i) => i.to_string(),
        None => format!("{}-{}", QUALIFIERS.len(), qualifier),
    }
}

fn compare_lists(a: &[Item], b: &[Item]) -> Ordering {
    for i in 0..a.len().max(b.len()) {
        let ord = match (a.get(i), b.get(i)) {
            (Some(l), r) => l.compare(r),
            // If this list is shorter, invert the comparison.
            (None, Some(r)) => r.compare(None).reverse(),
            (None, None) => Ordering::Equal,
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}

/// Remove trailing null items: `0`, `""`, empty sub-lists.
fn normalize(list: &mut Vec<Item>) {
    while list.last().is_some_and(Item::is_null) {
        list.pop();
    }
}

/// Find a trailing `-SNAPSHOT` or Maven timestamp (`-yyyyMMdd.HHmmss-N`),
/// optionally followed by ` (private build info)`, like VersionNumber's
/// `^.*((?:-\d{8}\.\d{6}-\d+)|-SNAPSHOT)( \(.*\))?$`.
/// Returns the byte offset where the marker starts and the marker itself.
fn find_snapshot(v: &str) -> Option<(usize, &str)> {
    let bytes = v.as_bytes();
    // `^.*` is greedy, so the last matching position wins.
    (0..bytes.len())
        .rev()
        .filter(|&p| bytes[p] == b'-')
        .find_map(|p| {
            let rest = &v[p..];
            let marker_len = if rest.starts_with("-SNAPSHOT") {
                "-SNAPSHOT".len()
            } else {
                timestamp_len(rest.as_bytes())?
            };
            let tail = &rest[marker_len..];
            let private_info = tail.len() >= 3
                && tail.starts_with(" (")
                && tail.ends_with(')')
                && !tail.contains('\n');
            (tail.is_empty() || private_info).then_some((p, &rest[..marker_len]))
        })
}

/// Length of a `-\d{8}\.\d{6}-\d+` prefix, if `s` starts with one.
fn timestamp_len(s: &[u8]) -> Option<usize> {
    let digits =
        |r: std::ops::Range<usize>| s.get(r).is_some_and(|d| d.iter().all(u8::is_ascii_digit));
    let shape = s.first() == Some(&b'-')
        && digits(1..9)
        && s.get(9) == Some(&b'.')
        && digits(10..16)
        && s.get(16) == Some(&b'-');
    if !shape {
        return None;
    }
    let build = s[17..].iter().take_while(|c| c.is_ascii_digit()).count();
    (build > 0).then_some(17 + build)
}

struct Parsed {
    items: Vec<Item>,
    snapshot: Option<String>,
}

fn parse_version(version: &str) -> Parsed {
    let (version, snapshot) = match find_snapshot(version) {
        Some((start, marker)) => (
            format!("{}-SNAPSHOT", &version[..start]),
            Some(marker.to_string()),
        ),
        None => (version.to_string(), None),
    };
    let version = version.to_lowercase();

    // The innermost list is on top; items always go to the innermost list.
    let mut stack: Vec<Vec<Item>> = vec![Vec::new()];
    let mut is_digit = false;
    let mut start = 0;
    let mut chars = version.char_indices().peekable();

    while let Some((i, c)) = chars.next() {
        let list = stack.last_mut().expect("stack is never empty");
        if c == '.' || c == '-' {
            list.push(if i == start {
                Item::Int(String::new())
            } else {
                Item::parse(is_digit, &version[start..i])
            });
            start = i + 1;
            if c == '-' && is_digit {
                // 1.0-* = 1-*
                normalize(list);
                // A new sub-list only when a digit follows a digit, to tell 1-1 from 1.1.
                if chars.peek().is_some_and(|&(_, next)| next.is_ascii_digit()) {
                    stack.push(Vec::new());
                }
            }
        } else if c == '*' {
            list.push(Item::Wildcard);
            start = i + 1;
        } else if c.is_ascii_digit() {
            if !is_digit && i > start {
                list.push(Item::string(&version[start..i], true));
                start = i;
            }
            is_digit = true;
        } else if c.is_whitespace() {
            if i > start {
                list.push(if is_digit {
                    Item::int(&version[start..i])
                } else {
                    Item::string(&version[start..i], true)
                });
                start = i;
            }
            is_digit = false;
        } else {
            if is_digit && i > start {
                list.push(Item::int(&version[start..i]));
                start = i;
            }
            is_digit = false;
        }
    }
    if version.len() > start {
        let list = stack.last_mut().expect("stack is never empty");
        list.push(Item::parse(is_digit, &version[start..]));
    }

    let mut items = stack.pop().expect("stack is never empty");
    normalize(&mut items);
    while let Some(mut parent) = stack.pop() {
        parent.push(Item::List(items));
        normalize(&mut parent);
        items = parent;
    }

    Parsed { items, snapshot }
}

impl Ord for JenkinsVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        let a = parse_version(&self.0);
        let b = parse_version(&other.0);
        compare_lists(&a.items, &b.items).then_with(|| {
            match (a.snapshot.as_deref(), b.snapshot.as_deref()) {
                (None, None) => Ordering::Equal,
                (None, Some(_)) => Ordering::Less,
                (Some(_), None) => Ordering::Greater,
                // A literal -SNAPSHOT cannot be ordered against a timestamp.
                (Some("-SNAPSHOT"), _) | (_, Some("-SNAPSHOT")) => Ordering::Equal,
                // -yyyyMMdd.HHmmss-N: timestamp first, then build number.
                (Some(x), Some(y)) => x[1..16]
                    .cmp(&y[1..16])
                    .then_with(|| Item::int(&x[17..]).compare(Some(&Item::int(&y[17..])))),
            }
        })
    }
}

impl PartialOrd for JenkinsVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Equality follows the ordering, so `1.0.9.0` == `1.0.9`.
impl PartialEq for JenkinsVersion {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for JenkinsVersion {}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> JenkinsVersion {
        JenkinsVersion::new(s)
    }

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

    #[test]
    fn dash_build_number_is_newer_than_release() {
        // mapdb-api 1.0.9-28.vf251ce40855d (2022) supersedes 1.0.9.0 (2016).
        assert!(v("1.0.9-28.vf251ce40855d") > v("1.0.9.0"));
        assert!(v("1.0.9.0") < v("1.0.9-28.vf251ce40855d"));
        assert!(v("1.0.9-28.vf251ce40855d") > v("1.0.9"));
        assert!(v("3.1.18-1") > v("3.1.18"));
        assert!(v("1.0.9-28.vf251ce40855d") < v("1.0.10"));
    }

    #[test]
    fn trailing_zeros_are_insignificant() {
        assert_eq!(v("1.0.9.0"), v("1.0.9"));
        assert_eq!(v("1.0.9").cmp(&v("1.0.9.0")), Ordering::Equal);
        assert_eq!(v("2.0.0"), v("2"));
        assert_eq!(v("1.0-ga"), v("1.0"));
        assert_eq!(v("1.0.final"), v("1"));
    }

    #[test]
    fn qualifiers_sort_below_release() {
        assert!(v("2.0-SNAPSHOT") < v("2.0"));
        assert!(v("2.0-beta-1") < v("2.0"));
        assert!(v("2.0-alpha-1") < v("2.0"));
        assert!(v("2.0-rc-1") < v("2.0"));
        assert!(v("2.0-rc1") < v("2.0"));
        assert!(v("2.0-cr1") < v("2.0"));
        assert!(v("2.0-milestone-1") < v("2.0"));
        assert!(v("2.0-SNAPSHOT") < v("2.0-alpha-1"));
        assert!(v("2.0-alpha-1") < v("2.0-beta-1"));
        assert!(v("2.0-a1") < v("2.0-b1"));
        assert!(v("2.0-beta-1") < v("2.0-beta-2"));
        assert!(v("2.0-beta-1") < v("2.0-rc-1"));
        assert!(v("2.0-sp1") > v("2.0"));
        assert!(v("2.0-beta-1") > v("1.9"));
    }

    #[test]
    fn core_lts_versions() {
        assert!(v("2.541.3") < v("2.555.1"));
        assert!(v("2.555.1") > v("2.541.3"));
        assert!(v("2.479.10") > v("2.479.9"));
    }

    #[test]
    fn incrementals_versions() {
        assert!(v("1498.vd852f8831d79") < v("1511.v2e3cb_0008ef0"));
        assert!(v("5.6-191.vb_47e2b_41c698") > v("5.6"));
        assert!(v("5.6-191.vb_47e2b_41c698") < v("5.6-192.v0123456789ab"));
        assert!(v("5.6-191.vb_47e2b_41c698") < v("5.7"));
        assert!(v("1311.v4250fde22ce7") > v("1.65"));
    }

    #[test]
    fn snapshot_timestamps() {
        assert!(v("1.0-20100101.123456-1") < v("1.0-20100101.123456-2"));
        assert!(v("1.0-20100101.123456-9") < v("1.0-20100102.000000-1"));
        assert!(v("1.0-20100101.123456-1") < v("1.0"));
        assert_eq!(v("1.0-20100101.123456-1"), v("1.0-SNAPSHOT"));
        assert_eq!(
            v("1.0-SNAPSHOT (private-08/24/2008 12:13-hudson)"),
            v("1.0-SNAPSHOT")
        );
    }

    #[test]
    fn long_numbers_do_not_overflow() {
        assert!(v("1.123456789012345678901234567890") > v("1.99999999999999999999"));
        assert_eq!(v("1.007"), v("1.7"));
    }

    #[test]
    fn ordering_is_antisymmetric_with_wildcards() {
        assert!(v("1.*") > v("1.99"));
        assert!(v("1.99") < v("1.*"));
        assert!(v("1.*") > v("1"));
    }
}
