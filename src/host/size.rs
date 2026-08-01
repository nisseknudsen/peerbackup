//! Sizes in and sizes out.
//!
//! Both directions used to live in `deploy/peerbackup-host` as shell. The
//! parser had no real test: `deploy/test-host-tooling.sh` reimplemented
//! `to_bytes` inside the test file and asserted against the copy, so the
//! shipped function was never run and a bug in it would have gone unnoticed.
//! That was not a sloppy test, it was forced: a bash file that is both a
//! library and an executable cannot be sourced without running its dispatcher.
//!
//! This is the whole reason the host side moved into the binary, so these two
//! functions get the most tests in the module.

/// Parse `500G`, `500GB`, `512M`, `1024K`, `1T` or a bare byte count.
///
/// Rejects everything else loudly. A silently misparsed size means a grant of
/// the wrong magnitude, which is the one thing provisioning must never get
/// wrong: too small and a friend's backup fails at 3am, too large and the host
/// overcommits the disk this design exists to protect.
pub fn parse_size(input: &str) -> Result<u64, String> {
    let s = input.trim().to_ascii_uppercase();
    let bad = || {
        format!(
            "bad size '{input}' (use e.g. 500G, or a plain number of bytes; K, M, G and T are the units)"
        )
    };

    // Split at the first unit letter, mirroring the shell's ${s%%[KMGT]*}.
    let split = s.find(['K', 'M', 'G', 'T']).unwrap_or(s.len());
    let (digits, unit) = s.split_at(split);
    let unit = unit.strip_suffix('B').unwrap_or(unit);

    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad());
    }
    let n: u64 = digits.parse().map_err(|_| bad())?;

    let scale: u64 = match unit {
        "" => 1,
        "K" => 1024,
        "M" => 1024 * 1024,
        "G" => 1024 * 1024 * 1024,
        "T" => 1024 * 1024 * 1024 * 1024,
        _ => return Err(bad()),
    };

    // The shell wrapped silently here. A wrapped size is a grant of the wrong
    // magnitude, which is exactly what this function exists to prevent.
    n.checked_mul(scale)
        .ok_or_else(|| format!("size '{input}' is too large to represent in bytes"))
}

/// Bytes as a human reads them: `500GB`, `1.5KB`, `999B`.
///
/// Matches `numfmt --to=iec --suffix=B`, which is what the shell called, so
/// output does not shift under anyone mid-upgrade. That means IEC units, one
/// decimal below 10 and none at or above it, rounding away from zero, and
/// carrying into the next unit when rounding reaches 1024.
pub fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];

    let mut idx = 0usize;
    let mut div: u64 = 1;
    while idx + 1 < UNITS.len() && bytes / div >= 1024 {
        div *= 1024;
        idx += 1;
    }
    if idx == 0 {
        return format!("{bytes}B");
    }

    if bytes / div >= 10 {
        let v = bytes.div_ceil(div);
        // Rounding up can reach a whole unit: 1023.9MB becomes 1024MB, which a
        // reader should see as 1.0GB.
        if v >= 1024 && idx + 1 < UNITS.len() {
            let promoted = v / 1024;
            return format!("{}.{}{}", promoted, 0, UNITS[idx + 1]);
        }
        format!("{}{}", v, UNITS[idx])
    } else {
        // Safe from overflow: this branch only runs when bytes < 10 * div, and
        // div is at most 1024^4.
        let tenths = (bytes * 10).div_ceil(div);
        format!("{}.{}{}", tenths / 10, tenths % 10, UNITS[idx])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_sizes_people_actually_type() {
        for (input, want) in [
            ("500G", 536_870_912_000),
            ("500GB", 536_870_912_000),
            ("500g", 536_870_912_000),
            ("1T", 1_099_511_627_776),
            ("512M", 536_870_912),
            ("1024K", 1_048_576),
            ("4096", 4096),
            ("0", 0),
            (" 500G ", 536_870_912_000),
        ] {
            assert_eq!(parse_size(input).unwrap(), want, "parsing {input}");
        }
    }

    #[test]
    fn rejects_anything_it_cannot_be_sure_about() {
        // Each of these would be a grant of the wrong size if it slipped
        // through, so every one must be an error rather than a guess.
        for input in [
            "", "G", "abc", "5X", "500GG", "-5G", "5.5G", "500 G", "1e3", "٥٠٠",
        ] {
            assert!(
                parse_size(input).is_err(),
                "{input:?} must be rejected, got {:?}",
                parse_size(input)
            );
        }
    }

    #[test]
    fn a_size_too_large_to_represent_is_an_error_not_a_wrapped_number() {
        // The shell wrapped silently here, turning an absurd request into a
        // small one. Refusing is the only safe answer.
        let err = parse_size("99999999999999T").unwrap_err();
        assert!(err.contains("too large"), "got: {err}");
    }

    #[test]
    fn the_error_says_what_a_good_size_looks_like() {
        let err = parse_size("banana").unwrap_err();
        assert!(err.contains("500G"), "must show an example, got: {err}");
    }

    #[test]
    fn formats_exactly_as_numfmt_to_iec_did() {
        // Captured from `numfmt --to=iec --suffix=B` on the coreutils the shell
        // version called. Output must not shift under anyone upgrading.
        //
        // These are a readable sample. The claim that this matches numfmt was
        // checked differentially over 406 values -- every power of two up to
        // 2^43, each one's neighbours, and a spread of fractional cases -- and
        // agreed on all of them. Rerun that if the rounding is ever touched:
        // pipe the same inputs through `numfmt --to=iec --suffix=B` and diff.
        for (bytes, want) in [
            (0, "0B"),
            (1, "1B"),
            (512, "512B"),
            (999, "999B"),
            (1000, "1000B"),
            (1023, "1023B"),
            (1024, "1.0KB"),
            (1025, "1.1KB"),
            (1100, "1.1KB"),
            (1126, "1.1KB"),
            (1500, "1.5KB"),
            (1536, "1.5KB"),
            (4096, "4.0KB"),
            (10240, "10KB"),
            (10241, "11KB"),
            (11263, "11KB"),
            (11264, "11KB"),
            (536_870_912, "512MB"),
            (536_870_912_000, "500GB"),
            (107_374_182_400, "100GB"),
            (1_073_741_823, "1.0GB"),
            (1_610_612_736, "1.5GB"),
            (2_147_483_648, "2.0GB"),
            (1_099_511_627_776, "1.0TB"),
        ] {
            assert_eq!(human(bytes), want, "formatting {bytes}");
        }
    }

    #[test]
    fn round_trips_through_both_directions() {
        // What someone types, and what they are shown afterwards. Single-digit
        // magnitudes keep a decimal because that is what numfmt did: `1T` reads
        // back as `1.0TB`, not `1TB`.
        for (typed, shown) in [
            ("500G", "500GB"),
            ("100G", "100GB"),
            ("512M", "512MB"),
            ("1T", "1.0TB"),
            ("2G", "2.0GB"),
        ] {
            let bytes = parse_size(typed).unwrap();
            assert_eq!(human(bytes), shown, "round trip of {typed}");
        }
    }
}
