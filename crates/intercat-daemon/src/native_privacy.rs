//! Foundation detects phone numbers locally; its offsets are UTF-16 code units.
use crate::{
    Result,
    privacy::{Kind, Span},
};
use objc2::rc::autoreleasepool;
use objc2_foundation::{NSDataDetector, NSMatchingOptions, NSRange, NSString, NSTextCheckingType};
use regex::Regex;
use std::{ops::Range, sync::OnceLock};

fn utf8_ranges(text: &str, mut ranges: Vec<NSRange>) -> Result<Vec<std::ops::Range<usize>>> {
    ranges.sort_by_key(|range| (range.location, range.length));
    ranges.dedup();
    let mut characters = text.chars();
    let (mut units, mut bytes) = (0usize, 0usize);
    let mut result = Vec::with_capacity(ranges.len());
    for range in ranges {
        let end = range
            .location
            .checked_add(range.length)
            .ok_or("Invalid native privacy range.")?;
        if range.length == 0 || range.location < units {
            return Err("Invalid native privacy range.".into());
        }
        let mut positions = [0usize; 2];
        for (index, target) in [range.location, end].into_iter().enumerate() {
            while units < target {
                let character = characters.next().ok_or("Invalid native privacy range.")?;
                units += character.len_utf16();
                bytes += character.len_utf8();
            }
            if units != target {
                return Err("Native privacy range splits a character.".into());
            }
            positions[index] = bytes;
        }
        result.push(positions[0]..positions[1]);
    }
    Ok(result)
}

// Native phone detection can swallow an adjacent ISO date. Keep calendar
// dates out of that pass; the full-text secret patterns still inspect them.
fn phone_regions(text: &str) -> Vec<Range<usize>> {
    static ISO_DATE: OnceLock<Regex> = OnceLock::new();
    let dates = ISO_DATE.get_or_init(|| {
        Regex::new(r"[0-9]{4}-(?:0[1-9]|1[0-2])-(?:0[1-9]|[12][0-9]|3[01])").unwrap()
    });
    let mut result = Vec::new();
    let mut start = 0;
    for date in dates.find_iter(text) {
        if text
            .as_bytes()
            .get(date.start().wrapping_sub(1))
            .is_some_and(u8::is_ascii_digit)
            || text
                .as_bytes()
                .get(date.end())
                .is_some_and(u8::is_ascii_digit)
        {
            continue;
        }
        let value = date.as_str();
        let year: u32 = value[..4].parse().unwrap();
        let month: u32 = value[5..7].parse().unwrap();
        let day: u32 = value[8..10].parse().unwrap();
        let days = match month {
            4 | 6 | 9 | 11 => 30,
            2 if year.is_multiple_of(4)
                && (!year.is_multiple_of(100) || year.is_multiple_of(400)) =>
            {
                29
            }
            2 => 28,
            _ => 31,
        };
        if year == 0 || day > days {
            continue;
        }
        if start < date.start() {
            result.push(start..date.start());
        }
        start = date.end();
    }
    if start < text.len() {
        result.push(start..text.len());
    }
    result
}

pub(crate) fn phones(texts: &[String]) -> Result<Vec<Vec<Span>>> {
    detect(texts, NSTextCheckingType::PhoneNumber, Kind::Phone, 256)
}

pub(crate) fn addresses(texts: &[String]) -> Result<Vec<Vec<Span>>> {
    detect(texts, NSTextCheckingType::Address, Kind::Address, 1024)
}

fn detect(
    texts: &[String],
    checking: NSTextCheckingType,
    kind: Kind,
    context: usize,
) -> Result<Vec<Vec<Span>>> {
    if kind == Kind::Phone && !texts.iter().any(|text| text.chars().any(char::is_numeric)) {
        return Ok(texts.iter().map(|_| Vec::new()).collect());
    }
    autoreleasepool(|_| {
        let detector = NSDataDetector::dataDetectorWithTypes_error(checking.bits())
            .map_err(|_| "The native privacy detector could not be created.")?;
        texts
            .iter()
            .map(|text| {
                // DataDetectors can spend minutes on a single long unbroken CJK run.
                // Each start position belongs to one core, with context on both sides.
                let core = 4096 - 2 * context;
                let mut spans = Vec::new();
                let regions = if kind == Kind::Phone {
                    phone_regions(text)
                } else {
                    std::iter::once(0..text.len()).collect()
                };
                for region in regions {
                    let region_offset = region.start;
                    let text = &text[region];
                    let mut core_start = 0;
                    while core_start < text.len() {
                        let core_end =
                            text.floor_char_boundary((core_start + core).min(text.len()));
                        let start = text.floor_char_boundary(core_start.saturating_sub(context));
                        let end = text.ceil_char_boundary((core_end + context).min(text.len()));
                        let chunk = &text[start..end];
                        if kind == Kind::Address || chunk.chars().any(char::is_numeric) {
                            let string = NSString::from_str(chunk);
                            let matches = detector.matchesInString_options_range(
                                &string,
                                NSMatchingOptions::empty(),
                                NSRange::new(0, string.length()),
                            );
                            if matches.len() > 2048 {
                                return Err("Too many native privacy findings.".into());
                            }
                            let ranges = (0..matches.len())
                                .map(|index| matches.objectAtIndex(index).range())
                                .collect();
                            for range in utf8_ranges(chunk, ranges)? {
                                if range.len() > context {
                                    return Err(
                                        "A native privacy match exceeds the supported context."
                                            .into(),
                                    );
                                }
                                let offset = start + range.start;
                                if (core_start..core_end).contains(&offset) {
                                    spans.push(Span {
                                        start: region_offset + offset,
                                        end: region_offset + start + range.end,
                                        kind,
                                    });
                                    if spans.len() > 2048 {
                                        return Err("Too many native privacy findings.".into());
                                    }
                                }
                            }
                        }
                        core_start = core_end;
                    }
                }
                Ok(spans)
            })
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_offsets_preserve_emoji_and_reject_split_surrogates() -> Result<()> {
        let text = "🙂電話090(1234)5678 終";
        let start = "🙂電話".encode_utf16().count();
        let ranges = utf8_ranges(
            text,
            vec![NSRange::new(start, "090(1234)5678".encode_utf16().count())],
        )?;
        assert_eq!(&text[ranges[0].clone()], "090(1234)5678");
        assert!(utf8_ranges(text, vec![NSRange::new(1, 2)]).is_err());
        assert!(utf8_ranges(text, vec![NSRange::new(100, 2)]).is_err());
        Ok(())
    }

    #[test]
    fn long_japanese_runs_and_window_boundaries_preserve_whole_phone_numbers() -> Result<()> {
        let phone = "090-1234-5678";
        assert!(phones(&["字".repeat(400_000)])?[0].is_empty());
        let mut texts: Vec<String> = (3560..3600)
            .map(|offset| format!("{} {phone} 完了", "x".repeat(offset)))
            .collect();
        texts.push(format!("{} 電話 {phone} 完了", "字".repeat(400_000)));
        let start = std::time::Instant::now();
        let found = phones(&texts)?;
        println!(
            "native long-text and boundary milliseconds: {}",
            start.elapsed().as_millis()
        );
        for (text, spans) in texts.iter().zip(found) {
            assert_eq!(
                spans
                    .iter()
                    .map(|span| &text[span.start..span.end])
                    .collect::<Vec<_>>(),
                vec![phone]
            );
        }
        Ok(())
    }

    #[test]
    fn foundation_detects_japanese_phone_numbers_in_multilingual_text() -> Result<()> {
        let texts = vec![
            "🙂 ご連絡は 090-1234-5678 へ。".to_owned(),
            "Call +81 90 1234 5678 tomorrow.".to_owned(),
            "Call (212) 555-0199 / 2026-11-07.".to_owned(),
        ];
        let started = std::time::Instant::now();
        let found = phones(&texts)?;
        println!(
            "native cold milliseconds: {}",
            started.elapsed().as_millis()
        );
        let warm = std::time::Instant::now();
        let _ = phones(&texts)?;
        println!("native warm milliseconds: {}", warm.elapsed().as_millis());
        assert_eq!(
            found[0]
                .iter()
                .map(|span| &texts[0][span.start..span.end])
                .collect::<Vec<_>>(),
            vec!["090-1234-5678"]
        );
        assert_eq!(
            found[1]
                .iter()
                .map(|span| &texts[1][span.start..span.end])
                .collect::<Vec<_>>(),
            vec!["+81 90 1234 5678"]
        );
        assert_eq!(
            found[2]
                .iter()
                .map(|span| &texts[2][span.start..span.end])
                .collect::<Vec<_>>(),
            vec!["(212) 555-0199"]
        );
        println!(
            "native locale: {}",
            objc2_foundation::NSLocale::currentLocale().localeIdentifier()
        );
        Ok(())
    }
}
