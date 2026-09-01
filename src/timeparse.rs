//! Parsing of OCR'd LiveSplit timer strings into milliseconds, and formatting back.
//!
//! All times in this crate are `i64` milliseconds.

/// Parse a timer string as produced by OCR into milliseconds.
///
/// Accepted shapes (OCR runs with a `0123456789:.` whitelist):
/// `H:MM:SS(.f)`, `MM:SS(.f)`, `M:SS(.f)`, `SS.f` — the fraction may be 1–3
/// digits. A bare number with neither a colon nor a fraction is rejected: it
/// is more likely a mangled read of a longer time than a real value.
pub fn parse_time(raw: &str) -> Option<i64> {
    let s: String = raw.chars().filter(|c| !c.is_whitespace()).collect();
    if s.is_empty() || !s.chars().all(|c| c.is_ascii_digit() || c == ':' || c == '.') {
        return None;
    }
    let (main, frac) = match s.split_once('.') {
        Some((m, f)) => (m, Some(f)),
        None => (s.as_str(), None),
    };
    let frac_ms = match frac {
        None => 0,
        Some(f) => {
            if f.is_empty() || f.len() > 3 || !f.chars().all(|c| c.is_ascii_digit()) {
                return None;
            }
            let v: i64 = f.parse().ok()?;
            v * 10i64.pow(3 - f.len() as u32)
        }
    };
    let parts: Vec<&str> = main.split(':').collect();
    if parts
        .iter()
        .any(|p| p.is_empty() || !p.chars().all(|c| c.is_ascii_digit()))
    {
        return None;
    }
    let (h, m, sec): (i64, i64, i64) = match parts.as_slice() {
        [ss] => {
            // Bare seconds only make sense with a fraction ("42.3").
            if frac.is_none() || ss.len() > 2 {
                return None;
            }
            (0, 0, ss.parse().ok()?)
        }
        [mm, ss] => {
            // Seconds must be exactly two digits: "1:5" is a dropped-digit
            // misread of "1:05" or "1:5x", not a real display.
            if ss.len() != 2 || mm.len() > 3 {
                return None;
            }
            (0, mm.parse().ok()?, ss.parse().ok()?)
        }
        [hh, mm, ss] => {
            if mm.len() != 2 || ss.len() != 2 || hh.len() > 2 {
                return None;
            }
            (hh.parse().ok()?, mm.parse().ok()?, ss.parse().ok()?)
        }
        _ => return None,
    };
    if sec >= 60 {
        return None;
    }
    // With an hours field, minutes must be a real 0-59; without one LiveSplit
    // would normally have rolled to H:MM:SS, but be lenient up to 599 minutes.
    if (parts.len() == 3 && m >= 60) || m >= 600 {
        return None;
    }
    Some(((h * 60 + m) * 60 + sec) * 1000 + frac_ms)
}

/// Parse LiveSplit's attempt counter: a bare integer, tolerating stray
/// whitelist punctuation OCR sometimes appends.
pub fn parse_counter(raw: &str) -> Option<i64> {
    let s: String = raw.chars().filter(|c| !c.is_whitespace()).collect();
    let s = s.trim_matches(|c| c == '.' || c == ':');
    if s.is_empty() || s.len() > 7 || !s.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// Format milliseconds the way LiveSplit displays them: `H:MM:SS.t` past an
/// hour, `M:SS.t` below it.
pub fn format_ms(ms: i64) -> String {
    let neg = ms < 0;
    let ms = ms.abs();
    let tenths = (ms % 1000) / 100;
    let s = ms / 1000;
    let (h, m, sec) = (s / 3600, (s % 3600) / 60, s % 60);
    let body = if h > 0 {
        format!("{h}:{m:02}:{sec:02}.{tenths}")
    } else {
        format!("{m}:{sec:02}.{tenths}")
    };
    if neg {
        format!("-{body}")
    } else {
        body
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_hms_with_fraction() {
        assert_eq!(parse_time("1:23:45.6"), Some(((3600 + 23 * 60 + 45) * 1000 + 600) as i64));
        assert_eq!(parse_time("1:23:45.67"), Some((3600 + 23 * 60 + 45) * 1000 + 670));
        assert_eq!(parse_time("1:23:45.678"), Some((3600 + 23 * 60 + 45) * 1000 + 678));
    }

    #[test]
    fn parses_minutes_seconds() {
        assert_eq!(parse_time("12:34.5"), Some((12 * 60 + 34) * 1000 + 500));
        assert_eq!(parse_time("1:07"), Some(67_000));
        assert_eq!(parse_time("0:59.99"), Some(59_990));
        // LiveSplit sometimes stays in MM:SS past an hour depending on settings.
        assert_eq!(parse_time("123:45"), Some((123 * 60 + 45) * 1000));
    }

    #[test]
    fn parses_bare_seconds_only_with_fraction() {
        assert_eq!(parse_time("42.3"), Some(42_300));
        assert_eq!(parse_time("42"), None);
        assert_eq!(parse_time("7.25"), Some(7_250));
    }

    #[test]
    fn tolerates_ocr_whitespace() {
        assert_eq!(parse_time(" 1:23:45.6 \n"), Some((3600 + 23 * 60 + 45) * 1000 + 600));
        assert_eq!(parse_time("1 : 07"), Some(67_000));
    }

    #[test]
    fn rejects_garbage() {
        for s in [
            "", ":", "::", "1:", ":30", "1:60", "1:5", "12:345", "1.2.3", "1:23:45:12",
            "abc", "1:2a", "12:34.", "12:34.5678", "1234:00", "99", "0:0",
        ] {
            assert_eq!(parse_time(s), None, "should reject {s:?}");
        }
    }

    #[test]
    fn rejects_out_of_range_fields() {
        assert_eq!(parse_time("1:60:00"), None);
        assert_eq!(parse_time("0:00:61"), None);
        assert_eq!(parse_time("600:00"), None);
    }

    #[test]
    fn parses_attempt_counter() {
        assert_eq!(parse_counter("96008"), Some(96008));
        assert_eq!(parse_counter(" 96034.\n"), Some(96034));
        assert_eq!(parse_counter("96:034"), None);
        assert_eq!(parse_counter(""), None);
        assert_eq!(parse_counter("12345678"), None);
    }

    #[test]
    fn formats_round_trip() {
        assert_eq!(format_ms(0), "0:00.0");
        assert_eq!(format_ms(59_990), "0:59.9");
        assert_eq!(format_ms(67_000), "1:07.0");
        assert_eq!(format_ms(3_600_000), "1:00:00.0");
        assert_eq!(format_ms((3600 + 23 * 60 + 45) * 1000 + 678), "1:23:45.6");
    }
}
