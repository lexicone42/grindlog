//! Parsing of OCR'd LiveSplit timer strings into milliseconds, and formatting back.
//!
//! All times in this crate are `i64` milliseconds.

/// Parse the main timer's text, repairing the one failure the LiveSplit timer
/// format produces systematically: the hundredths are drawn in a smaller
/// font, and at stream resolution their decimal point is a couple of pixels
/// that thresholding erases. "4.76" then reads as "476", "45.71" as "4571",
/// "3:06.12" as "3:06 12" and "34:37.95" as "34:3795". Every run starts in that sub-ten-second
/// range, so without this repair the first seconds of every attempt are
/// illegible and a quick reset is never seen at all. Two bare digits are NOT
/// repaired: they are the hundredths alone, which is also what a mid-run
/// "1:45.xx" degrades to when its big digits are lost, and reading them as
/// 0.45 twice in a row would reset a running run.
///
/// Only the timer uses this; split rows and reference times stay strict. The
/// tracker's own consistency checks (a reading must advance with the wall
/// clock) are what keep a repaired misread from becoming a run.
pub fn parse_timer_text(raw: &str) -> Option<i64> {
    if let Some(v) = parse_time(raw) {
        return Some(v);
    }
    let t = raw.trim();
    // "3:06 12" / "1 86": a gap where the point was.
    if let Some((head, tail)) = t.rsplit_once(' ') {
        if tail.len() == 2
            && tail.chars().all(|c| c.is_ascii_digit())
            && !head.contains('.')
            && head.chars().all(|c| c.is_ascii_digit() || c == ':')
            && head.chars().any(|c| c.is_ascii_digit())
        {
            return parse_time(&format!("{head}.{tail}"));
        }
    }
    // "34:3795" / "2:52:2609": the same erased point, with nothing left in
    // its place. The CLI OCR engine leaves the gap repaired above; the
    // in-process engine joins the two runs of digits. After a colon LiveSplit
    // always pads the seconds to two digits and always draws two hundredths,
    // so exactly four digits behind the last colon is that field pair and
    // nothing else. `parse_time` still has to accept the result, which is
    // what rejects "34:9995".
    if let Some((head, tail)) = t.rsplit_once(':') {
        if tail.len() == 4
            && tail.chars().all(|c| c.is_ascii_digit())
            && !head.is_empty()
            && head.chars().all(|c| c.is_ascii_digit() || c == ':')
        {
            return parse_time(&format!("{head}:{}.{}", &tail[..2], &tail[2..]));
        }
    }
    // Bare digits with no separator at all: seconds and hundredths. Three or
    // four of them only — two bare digits are the small hundredths alone,
    // which also appear when the big digits of a mid-run "1:45.xx" are lost,
    // and reading those as 0.45 twice in a row would reset a running run.
    if t.len() >= 3 && t.len() <= 4 && t.chars().all(|c| c.is_ascii_digit()) {
        return parse_time(&format!("{}.{}", &t[..t.len() - 2], &t[t.len() - 2..]));
    }
    None
}

/// Parse a timer string as produced by OCR into milliseconds.
///
/// Accepted shapes (OCR runs with a `0123456789:.` whitelist):
/// `H:MM:SS(.f)`, `MM:SS(.f)`, `M:SS(.f)`, `SS.f` — the fraction may be 1–3
/// digits. A bare number with neither a colon nor a fraction is rejected: it
/// is more likely a mangled read of a longer time than a real value.
pub fn parse_time(raw: &str) -> Option<i64> {
    let s: String = raw.chars().filter(|c| !c.is_whitespace()).collect();
    if s.is_empty()
        || !s
            .chars()
            .all(|c| c.is_ascii_digit() || c == ':' || c == '.')
    {
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

/// Whether an OCR'd word is a time: it has a separator and parses (a
/// trailing '.' is tolerated). Bare digits are not, so an attempt counter
/// or a row label never passes.
pub fn time_shaped(text: &str) -> bool {
    let t = text.trim().trim_end_matches('.');
    t.contains([':', '.']) && parse_time(t).is_some()
}

/// Whether a time-shaped word carries a fraction ("0:47.3", "5.00"): a '.'
/// followed by a digit. "0:4" and "11:3" do not; nor does a trailing '.'.
pub fn has_fraction(text: &str) -> bool {
    text.trim()
        .split_once('.')
        .is_some_and(|(_, f)| f.chars().next().is_some_and(|c| c.is_ascii_digit()))
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

/// Format milliseconds the way a marathon board prints them: whole seconds,
/// `H:MM:SS` past an hour and `M:SS` below it. A board's split columns are
/// rounded to the second, so this is the form its own times are reported in.
pub fn format_ms_seconds(ms: i64) -> String {
    let s = ms / 1000;
    match s / 3600 {
        0 => format!("{}:{:02}", s / 60, s % 60),
        h => format!("{h}:{:02}:{:02}", (s / 60) % 60, s % 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_board_prints_whole_seconds() {
        assert_eq!(format_ms_seconds(0), "0:00");
        assert_eq!(format_ms_seconds(59_900), "0:59");
        assert_eq!(format_ms_seconds(11 * 60_000 + 53_000), "11:53");
        assert_eq!(
            format_ms_seconds(4 * 3_600_000 + 34 * 60_000 + 10_000),
            "4:34:10"
        );
    }

    #[test]
    fn parses_full_hms_with_fraction() {
        assert_eq!(
            parse_time("1:23:45.6"),
            Some(((3600 + 23 * 60 + 45) * 1000 + 600) as i64)
        );
        assert_eq!(
            parse_time("1:23:45.67"),
            Some((3600 + 23 * 60 + 45) * 1000 + 670)
        );
        assert_eq!(
            parse_time("1:23:45.678"),
            Some((3600 + 23 * 60 + 45) * 1000 + 678)
        );
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
        assert_eq!(
            parse_time(" 1:23:45.6 \n"),
            Some((3600 + 23 * 60 + 45) * 1000 + 600)
        );
        assert_eq!(parse_time("1 : 07"), Some(67_000));
    }

    #[test]
    fn rejects_garbage() {
        for s in [
            "",
            ":",
            "::",
            "1:",
            ":30",
            "1:60",
            "1:5",
            "12:345",
            "1.2.3",
            "1:23:45:12",
            "abc",
            "1:2a",
            "12:34.",
            "12:34.5678",
            "1234:00",
            "99",
            "0:0",
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
    fn time_shaped_words_have_a_separator_and_parse() {
        assert!(time_shaped("0:47.3") && time_shaped("11:35.1") && time_shaped("2.0"));
        assert!(time_shaped(" 1:03:20 ") && time_shaped("12:34."));
        assert!(!time_shaped("96326") && !time_shaped("0:4") && !time_shaped("241.9"));
        assert!(!time_shaped("8:38.6:") && !time_shaped("") && !time_shaped("Act"));
        assert!(has_fraction("0:47.3") && has_fraction("5.00"));
        assert!(!has_fraction("0:4") && !has_fraction("11:3") && !has_fraction("0:47."));
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

#[cfg(test)]
mod timer_text_tests {
    use super::*;

    #[test]
    fn repairs_the_missing_decimal_point_of_the_small_hundredths_font() {
        // Strict values pass straight through.
        assert_eq!(parse_timer_text("11:35.47"), Some(695_470));
        assert_eq!(parse_timer_text("7.66"), Some(7_660));
        // The point of the small fraction font lost to thresholding.
        assert_eq!(parse_timer_text("476"), Some(4_760));
        assert_eq!(parse_timer_text("4571"), Some(45_710));
        // Two bare digits are the hundredths alone — also what a mid-run
        // "1:45.xx" degrades to when its big digits are lost — so they are
        // NOT a near-zero reading.
        assert_eq!(parse_timer_text("45"), None);
        // A gap where the point was.
        assert_eq!(parse_timer_text("3:06 12"), Some(186_120));
        assert_eq!(parse_timer_text("1 86"), Some(1_860));
        // Not repairable: a lone digit, five digits (the attempt counter
        // shape), letters, a gap after a point (the strict parser already
        // ignores whitespace there and reads "1.2 34" as 1.234).
        assert_eq!(parse_timer_text("4"), None);
        assert_eq!(parse_timer_text("95958"), None);
        assert_eq!(parse_timer_text("4a6"), None);
        assert_eq!(parse_timer_text("1.2 34"), Some(1_234));
        assert_eq!(parse_timer_text(""), None);
    }

    /// The same erased point with no gap left behind, which is what the
    /// in-process OCR engine returns where the CLI leaves a space. Measured
    /// on a marathon board, whose total reads "34:3795" on nine frames in
    /// ten: without this the timer parsed on 8% of that broadcast.
    #[test]
    fn repairs_the_erased_point_with_no_gap_after_a_colon() {
        assert_eq!(parse_timer_text("34:3795"), Some(2_077_950));
        assert_eq!(parse_timer_text("2:52:2609"), Some(10_346_090));
        assert_eq!(parse_timer_text("1:0547"), Some(65_470));
        // Only four digits behind the last colon: after a colon LiveSplit
        // pads the seconds to two and always draws two hundredths, so three
        // or five digits there is a misread, not a lost point.
        assert_eq!(parse_timer_text("34:379"), None);
        assert_eq!(parse_timer_text("34:37955"), None);
        // The repair still has to produce a real time.
        assert_eq!(parse_timer_text("34:9995"), None);
        assert_eq!(parse_timer_text("34:6012"), None);
        // A value that already carries its point is never re-cut.
        assert_eq!(parse_timer_text("1:23:45.67"), Some(5_025_670));
    }
}
