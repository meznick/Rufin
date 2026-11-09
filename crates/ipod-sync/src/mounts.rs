//! The mount table layer: where mounted filesystems come from.
//!
//! The only production implementation is [`ProcMountInfo`], which reads the
//! Linux `/proc/self/mountinfo` table. The [`MountSource`] trait exists so
//! tests can feed synthetic mount tables without touching the host's mounts.

use std::path::PathBuf;

/// One mounted filesystem as reported by a mount source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountEntry {
    pub mount_point: PathBuf,
    pub fstype: String,
    pub source: String,
}

/// Why reading the mount table failed.
#[derive(Debug, thiserror::Error)]
#[error("cannot read the mount table: {0}")]
pub struct MountError(#[from] pub std::io::Error);

/// Where mount entries come from. The trait exists so tests can feed
/// synthetic mount tables; the only production implementation is
/// [`ProcMountInfo`].
pub trait MountSource: Send + Sync {
    fn mounts(&self) -> Result<Vec<MountEntry>, MountError>;
}

/// The Linux mount table reader: `/proc/self/mountinfo`.
#[derive(Debug, Default)]
pub struct ProcMountInfo;

impl MountSource for ProcMountInfo {
    fn mounts(&self) -> Result<Vec<MountEntry>, MountError> {
        let text = std::fs::read_to_string("/proc/self/mountinfo")?;
        Ok(parse_mountinfo(&text))
    }
}

/// Parse mountinfo text into entries.
///
/// Line grammar (`man 5 proc`), e.g.:
///
/// ```text
/// 36 35 98:0 /mnt1 /mnt2 rw,noatime master:1 - ext3 /dev/root rw,errors=continue
/// ```
///
/// Fields 0..=4 are mount id, parent id, major:minor, root, and **mount
/// point**; then the mount options and optional fields until a bare `-`
/// field; then **fstype**, **source**, and super options. Spaces and control
/// characters inside fields are octal-escaped (`\040` space, `\011` tab,
/// `\012` newline, `\134` backslash).
///
/// Malformed lines are skipped with a debug trace — one bad line never fails
/// the whole scan — so this function has no parse-error path.
pub fn parse_mountinfo(text: &str) -> Vec<MountEntry> {
    let mut entries = Vec::new();
    for (index, line) in text.lines().enumerate() {
        match parse_line(line) {
            Some(entry) => entries.push(entry),
            None => tracing::debug!(line = index + 1, "skipping malformed mountinfo line"),
        }
    }
    entries
}

/// Parse one mountinfo line; `None` when the line is malformed.
fn parse_line(line: &str) -> Option<MountEntry> {
    let fields: Vec<&str> = line.split(' ').collect();
    // 6 mandatory fields (mount point is field 4) + `-` + fstype + source.
    if fields.len() < 8 {
        return None;
    }
    // Find the `-` separator that ends the optional-fields section. The
    // search starts after the five mandatory fields (0..=4), so a mount
    // point literally named `-` is not mistaken for the separator (the
    // kernel never escapes `-`).
    let separator = fields[5..]
        .iter()
        .position(|field| *field == "-")
        .map(|index| index + 5)?;
    // fstype and source directly follow the separator.
    let fstype = fields.get(separator + 1)?;
    let source = fields.get(separator + 2)?;
    Some(MountEntry {
        mount_point: PathBuf::from(unescape(fields[4])?.into_owned()),
        fstype: unescape(fstype)?.into_owned(),
        source: unescape(source)?.into_owned(),
    })
}

/// Undo the mountinfo octal escapes: `\040` space, `\011` tab, `\012`
/// newline, `\134` backslash. Any other backslash sequence is malformed
/// (the caller drops the line), including octal values above `\377` — the
/// arithmetic is overflow-checked so a malformed line is skipped, never a
/// panic, per the parser's tolerance contract.
fn unescape(field: &str) -> Option<std::borrow::Cow<'_, str>> {
    if !field.contains('\\') {
        return Some(std::borrow::Cow::Borrowed(field));
    }
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            // An escape is a backslash followed by exactly three octal digits.
            let digits = bytes.get(i + 1..i + 4)?;
            let value = digits.iter().try_fold(0u8, |acc, digit| {
                let d = digit.checked_sub(b'0')?;
                if d > 7 {
                    return None;
                }
                acc.checked_mul(8)?.checked_add(d)
            })?;
            out.push(value);
            i += 4;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok().map(std::borrow::Cow::Owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_LINE: &str =
        "36 35 98:0 /mnt1 /mnt2 rw,noatime master:1 - ext3 /dev/root rw,errors=continue";

    #[test]
    fn parse_mountinfo_wellformed_line() {
        let entries = parse_mountinfo(SAMPLE_LINE);
        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        assert_eq!(entry.mount_point, PathBuf::from("/mnt2"));
        assert_eq!(entry.fstype, "ext3");
        assert_eq!(entry.source, "/dev/root");
    }

    #[test]
    fn parse_mountinfo_unescapes_octal_sequences() {
        let line = "1 1 8:1 / /mnt\\040point rw - vfat /dev/sdb1 rw";
        let entries = parse_mountinfo(line);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].mount_point, PathBuf::from("/mnt point"));
    }

    #[test]
    fn parse_mountinfo_unescapes_all_documented_sequences() {
        // \011 tab, \012 newline, \134 backslash.
        let line = "1 1 8:1 / /m\\011t\\012\\134p rw - vfat /dev/sdb1 rw";
        let entries = parse_mountinfo(line);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].mount_point, PathBuf::from("/m\tt\n\\p"));
    }

    #[test]
    fn parse_mountinfo_optional_fields_and_separator() {
        // A line without optional fields still has the bare `-` field.
        let line = "22 21 8:1 / /simple rw - vfat /dev/sdc1 rw";
        let entries = parse_mountinfo(line);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].mount_point, PathBuf::from("/simple"));
        assert_eq!(entries[0].fstype, "vfat");
    }

    #[test]
    fn parse_mountinfo_skips_malformed_lines() {
        let text = format!(
            "{SAMPLE_LINE}\n\
             too short\n\
             missing separator line with many fields\n\
             1 1 8:1 / /bad\\x99escape rw - vfat /dev/sdc1 rw\n"
        );
        let entries = parse_mountinfo(&text);
        assert_eq!(entries.len(), 1, "only the well-formed line parses");
        assert_eq!(entries[0].mount_point, PathBuf::from("/mnt2"));
    }

    #[test]
    fn parse_mountinfo_rejects_bad_escape_in_mount_point() {
        // `\x99` is not an octal escape: the line is malformed and skipped.
        let line = "1 1 8:1 / /bad\\x99escape rw - vfat /dev/sdc1 rw";
        assert!(parse_mountinfo(line).is_empty());
    }

    #[test]
    fn parse_mountinfo_skips_overflowing_octal_escape() {
        // `\400` is valid octal digits but the value overflows a byte: the
        // line is malformed and skipped — never a panic (overflow-checked
        // arithmetic), per the tolerance contract.
        let line = "1 1 8:1 / /bad\\400escape rw - vfat /dev/sdc1 rw";
        assert!(parse_mountinfo(line).is_empty());
    }

    #[test]
    fn parse_mountinfo_dash_mount_point_is_not_the_separator() {
        // A mount point literally named `-` must parse as the mount point;
        // the real `-` separator comes later. The kernel never escapes `-`.
        let line = "1 1 8:1 / - rw - vfat /dev/sdc1 rw";
        let entries = parse_mountinfo(line);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].mount_point, PathBuf::from("-"));
        assert_eq!(entries[0].fstype, "vfat");
        assert_eq!(entries[0].source, "/dev/sdc1");
    }
}
