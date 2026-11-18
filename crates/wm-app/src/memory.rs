//! The app's memory, for the hourly disk line: the process's resident set
//! and, in a container, its use as `docker stats` counts it against its
//! limit (`deploy.resources.limits.memory`). Linux only; elsewhere, and
//! where a file is missing, that part is left out.
//!
//! The two tell a leak from a full page cache: Docker counts the files the
//! container read or wrote (page cache) as its use, and the kernel takes
//! that back before it would stop the app; the resident set is the app's
//! own (18 November 2026: the app sat at 764.6 MiB of its 768 MiB and had
//! restarted 24 times, and nothing in its log said which it was).

/// One reading, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Reading {
    /// The process's resident set (`VmRSS`).
    pub resident: Option<u64>,
    /// The container's use as `docker stats` counts it: the cgroup's use
    /// less its inactive page cache.
    pub container: Option<u64>,
    /// The container's limit; `None` without one.
    pub limit: Option<u64>,
}

/// cgroup v2's files, then v1's: (use, statistics, the limit, the
/// statistic of inactive page cache).
const CGROUPS: [(&str, &str, &str, &str); 2] = [
    (
        "/sys/fs/cgroup/memory.current",
        "/sys/fs/cgroup/memory.stat",
        "/sys/fs/cgroup/memory.max",
        "inactive_file",
    ),
    (
        "/sys/fs/cgroup/memory/memory.usage_in_bytes",
        "/sys/fs/cgroup/memory/memory.stat",
        "/sys/fs/cgroup/memory/memory.limit_in_bytes",
        "total_inactive_file",
    ),
];

/// The app's memory now.
pub fn read() -> Reading {
    let file = |path: &str| std::fs::read_to_string(path).ok();
    let resident = file("/proc/self/status").and_then(|s| vm_rss(&s));
    for (usage, stat, limit, inactive) in CGROUPS {
        if let Some(used) = file(usage).and_then(|s| number(&s)) {
            let inactive = file(stat).and_then(|s| stat_value(&s, inactive));
            return Reading {
                resident,
                container: Some(docker_usage(used, inactive)),
                limit: file(limit).and_then(|s| number(&s)),
            };
        }
    }
    Reading {
        resident,
        ..Reading::default()
    }
}

/// `VmRSS:   123456 kB` in `/proc/self/status`, in bytes.
pub fn vm_rss(status: &str) -> Option<u64> {
    let kb = status
        .lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))?
        .trim()
        .strip_suffix("kB")?
        .trim()
        .parse::<u64>()
        .ok()?;
    kb.checked_mul(1024)
}

/// A cgroup file's number. No limit reads "max" (v2) or a number near
/// 2⁶³ (v1): `None`.
pub fn number(text: &str) -> Option<u64> {
    text.trim().parse::<u64>().ok().filter(|&n| n < 1 << 62)
}

/// A statistic's value in a cgroup's `memory.stat`.
pub fn stat_value(stat: &str, key: &str) -> Option<u64> {
    stat.lines().find_map(|l| {
        let (k, v) = l.split_once(' ')?;
        (k == key).then(|| v.trim().parse().ok()).flatten()
    })
}

/// The use as `docker stats` shows it: the inactive page cache left out.
pub fn docker_usage(used: u64, inactive_file: Option<u64>) -> u64 {
    match inactive_file {
        Some(i) if i < used => used - i,
        _ => used,
    }
}

/// Bytes in MB (1024², as `docker stats` counts MiB).
fn mb(bytes: u64) -> String {
    format!("{:.0} MB", bytes as f64 / (1024.0 * 1024.0))
}

/// "memory: the app 312 MB resident, the container 540 MB of 768 MB";
/// `None` when nothing could be read.
pub fn summary(r: &Reading) -> Option<String> {
    let app = r.resident.map(|b| format!("the app {} resident", mb(b)));
    let container = r.container.map(|b| match r.limit {
        Some(l) => format!("the container {} of {}", mb(b), mb(l)),
        None => format!("the container {}", mb(b)),
    });
    let parts: Vec<String> = app.into_iter().chain(container).collect();
    (!parts.is_empty()).then(|| format!("memory: {}", parts.join(", ")))
}

/// Bytes in whole MB, for a log field.
pub fn whole_mb(bytes: Option<u64>) -> Option<u64> {
    bytes.map(|b| b / (1024 * 1024))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MB: u64 = 1024 * 1024;

    #[test]
    fn the_resident_set_is_read_from_proc_status() {
        let status = "Name:\tweather-machine\nVmPeak:\t  900000 kB\nVmRSS:\t  319488 kB\nRssAnon:\t  300000 kB\n";
        assert_eq!(vm_rss(status), Some(312 * MB));
        assert_eq!(vm_rss("Name:\tx\n"), None);
        assert_eq!(vm_rss("VmRSS:\tlots kB\n"), None);
    }

    /// v2 says "max" without a limit, v1 a number near 2⁶³.
    #[test]
    fn a_cgroup_without_a_limit_has_none() {
        assert_eq!(number("805306368\n"), Some(768 * MB));
        assert_eq!(number("max\n"), None);
        assert_eq!(number("9223372036854771712\n"), None);
        assert_eq!(number(""), None);
    }

    /// The use `docker stats` shows: the cgroup's, less the inactive page
    /// cache, which the kernel takes back before it would stop the app.
    #[test]
    fn the_containers_use_leaves_out_the_inactive_page_cache() {
        let v2 = "anon 300000000\nfile 500000000\nactive_file 100000000\ninactive_file 400000000\n";
        assert_eq!(stat_value(v2, "inactive_file"), Some(400_000_000));
        assert_eq!(stat_value(v2, "file"), Some(500_000_000), "the whole key");
        let v1 = "cache 1\ninactive_file 7\ntotal_inactive_file 6460809216\n";
        assert_eq!(stat_value(v1, "total_inactive_file"), Some(6_460_809_216));
        assert_eq!(stat_value(v1, "missing"), None);
        assert_eq!(docker_usage(800, Some(300)), 500);
        assert_eq!(docker_usage(800, None), 800);
        assert_eq!(docker_usage(800, Some(900)), 800, "never below zero");
    }

    #[test]
    fn the_summary_names_what_was_read() {
        let full = Reading {
            resident: Some(312 * MB),
            container: Some(540 * MB),
            limit: Some(768 * MB),
        };
        assert_eq!(
            summary(&full).as_deref(),
            Some("memory: the app 312 MB resident, the container 540 MB of 768 MB")
        );
        let unlimited = Reading {
            limit: None,
            ..full
        };
        assert_eq!(
            summary(&unlimited).as_deref(),
            Some("memory: the app 312 MB resident, the container 540 MB")
        );
        let bare = Reading {
            resident: Some(312 * MB),
            ..Reading::default()
        };
        assert_eq!(
            summary(&bare).as_deref(),
            Some("memory: the app 312 MB resident")
        );
        assert_eq!(summary(&Reading::default()), None);
        assert_eq!(whole_mb(Some(312 * MB + 5)), Some(312));
    }

    /// On Linux the process's own resident set can always be read.
    #[cfg(target_os = "linux")]
    #[test]
    fn this_process_has_a_resident_set() {
        assert!(read().resident.is_some_and(|b| b > 0));
    }
}
