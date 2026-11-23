//! PEP 440 version ordering (`packaging.version.Version`), enough for `>`/`<=`
//! comparisons between release, pre-, post- and dev-releases.

use regex::Regex;
use std::cmp::Ordering;
use std::sync::LazyLock;

static VERSION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?ix)^\s*v?
        (?:(\d+)!)?
        (\d+(?:\.\d+)*)
        (?:[-_.]?(a|b|c|rc|alpha|beta|pre|preview)[-_.]?(\d+)?)?
        (?:(?:-(\d+))|(?:[-_.]?(post|rev|r)[-_.]?(\d+)?))?
        (?:[-_.]?dev[-_.]?(\d+)?)?
        (?:\+[a-z0-9]+(?:[-_.][a-z0-9]+)*)?
        \s*$",
    )
    .expect("re")
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    epoch: u64,
    release: Vec<u64>,
    /// (phase, n): a=0, b=1, rc=2.
    pre: Option<(u8, u64)>,
    post: Option<u64>,
    dev: Option<u64>,
}

impl Version {
    pub fn parse(s: &str) -> Option<Version> {
        let c = VERSION.captures(s)?;
        let num = |i: usize| c.get(i).map(|m| m.as_str().parse::<u64>().unwrap_or(0));
        let pre = c.get(3).map(|m| {
            let phase = match m.as_str().to_lowercase().as_str() {
                "a" | "alpha" => 0,
                "b" | "beta" => 1,
                _ => 2,
            };
            (phase, num(4).unwrap_or(0))
        });
        let post = num(5).or_else(|| c.get(6).map(|_| num(7).unwrap_or(0)));
        let dev = c
            .get(0)
            .filter(|m| m.as_str().to_lowercase().contains("dev"))
            .map(|_| num(8).unwrap_or(0));
        Some(Version {
            epoch: num(1).unwrap_or(0),
            release: c[2].split('.').map(|p| p.parse().unwrap_or(0)).collect(),
            pre,
            post,
            dev,
        })
    }

    pub fn is_prerelease(&self) -> bool {
        self.pre.is_some() || self.dev.is_some()
    }

    pub fn release(&self) -> &[u64] {
        &self.release
    }

    /// The release with the pre/post/dev parts dropped.
    pub fn base(&self) -> Version {
        Version {
            epoch: self.epoch,
            release: self.release.clone(),
            pre: None,
            post: None,
            dev: None,
        }
    }

    /// `version in Specifier(op + spec)` with packaging's default prerelease rule
    /// left to the caller.
    pub fn matches(&self, op: &str, spec: &str) -> bool {
        let spec = spec.trim();
        if op == "===" {
            return self.to_string() == spec;
        }
        if (op == "==" || op == "!=")
            && let Some(prefix) = spec.strip_suffix(".*")
        {
            let Some(p) = Version::parse(prefix) else {
                return false;
            };
            let n = p.release.len();
            let mut mine = self.release.clone();
            mine.resize(n.max(mine.len()), 0);
            let hit = self.epoch == p.epoch && mine[..n] == p.release[..];
            return if op == "==" { hit } else { !hit };
        }
        let Some(v) = Version::parse(spec) else {
            return false;
        };
        match op {
            "==" => *self == v || self.cmp(&v) == Ordering::Equal,
            "!=" => self.cmp(&v) != Ordering::Equal,
            "<=" => *self <= v,
            ">=" => *self >= v,
            "<" => {
                *self < v
                    && !(self.base() == v.base() && self.is_prerelease() && !v.is_prerelease())
            }
            ">" => {
                *self > v && !(self.post.is_some() && self.base() == v.base() && v.post.is_none())
            }
            "~=" => {
                if v.release.len() < 2 {
                    return false;
                }
                let prefix = &v.release[..v.release.len() - 1];
                let mut mine = self.release.clone();
                mine.resize(prefix.len().max(mine.len()), 0);
                *self >= v && mine[..prefix.len()] == *prefix
            }
            _ => false,
        }
    }

    /// `packaging`'s `_cmpkey`, with -inf/+inf encoded as i128 sentinels.
    fn key(&self) -> (u64, Vec<u64>, (i128, i128), i128, i128) {
        let mut release = self.release.clone();
        while release.last() == Some(&0) {
            release.pop();
        }
        let pre = match (self.pre, self.post, self.dev) {
            (None, None, Some(_)) => (i128::MIN, 0),
            (None, _, _) => (i128::MAX, 0),
            (Some((p, n)), _, _) => (p as i128, n as i128),
        };
        let post = self.post.map_or(i128::MIN, |n| n as i128);
        let dev = self.dev.map_or(i128::MAX, |n| n as i128);
        (self.epoch, release, pre, post, dev)
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.epoch != 0 {
            write!(f, "{}!", self.epoch)?;
        }
        let r: Vec<String> = self.release.iter().map(|x| x.to_string()).collect();
        f.write_str(&r.join("."))?;
        if let Some((p, n)) = self.pre {
            write!(f, "{}{n}", ["a", "b", "rc"][p as usize])?;
        }
        if let Some(n) = self.post {
            write!(f, ".post{n}")?;
        }
        if let Some(n) = self.dev {
            write!(f, ".dev{n}")?;
        }
        Ok(())
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.key().cmp(&other.key())
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(test)]
mod tests {
    use super::Version;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[test]
    fn orders_like_packaging() {
        let order = [
            "3.34.9",
            "3.35.0.dev0",
            "3.35.0a1",
            "3.35.0rc1",
            "3.35",
            "3.35.0.post1",
            "3.35.1",
        ];
        for w in order.windows(2) {
            assert!(v(w[0]) < v(w[1]), "{} < {}", w[0], w[1]);
        }
        assert_eq!(v("3.35.0").cmp(&v("3.35")), std::cmp::Ordering::Equal);
        assert!(Version::parse("not a version").is_none());
    }
}
