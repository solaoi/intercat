//! Lossless source ranges for requests bounded by the local generation budget.
use std::ops::Range;

pub type Pass = Vec<(usize, Range<usize>)>;
pub const EVIDENCE_BYTES: usize = 4096;

pub fn split(pass: &Pass, texts: &[&str]) -> Option<(Pass, Pass)> {
    if pass.len() > 1 {
        let middle = pass.len() / 2;
        return Some((pass[..middle].to_vec(), pass[middle..].to_vec()));
    }
    let (source, range) = pass.first()?;
    let text = texts.get(*source)?.get(range.clone())?;
    let target = text.len() / 2;
    let middle = text
        .char_indices()
        .map(|(index, _)| index)
        .filter(|index| *index > 0)
        .min_by_key(|index| index.abs_diff(target))?;
    let middle = text
        .char_indices()
        .filter(|(index, character)| {
            *index >= text.len() / 3
                && *index <= text.len() * 2 / 3
                && matches!(character, '\n' | '.' | '。' | '!' | '?' | '！' | '？')
        })
        .map(|(index, character)| index + character.len_utf8())
        .min_by_key(|index| index.abs_diff(middle))
        .unwrap_or(middle);
    if middle == text.len() {
        return None;
    }
    let overlap = EVIDENCE_BYTES.min(text.len() / 2);
    let left_target = (middle + overlap / 2).min(text.len());
    let right_target = middle.saturating_sub(overlap - overlap / 2);
    let left = text
        .char_indices()
        .map(|(index, _)| index)
        .find(|index| *index >= left_target)
        .filter(|index| *index < text.len())
        .unwrap_or(middle);
    let right = text
        .char_indices()
        .map(|(index, _)| index)
        .take_while(|index| *index <= right_target)
        .last()
        .filter(|index| *index > 0)
        .unwrap_or(middle);
    Some((
        vec![(*source, range.start..range.start + left)],
        vec![(*source, range.start + right..range.end)],
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splitting_preserves_every_byte_and_utf8_and_makes_progress() {
        for text in ["日本語の仮定です。".repeat(500), "x".repeat(40_000)] {
            let pass = vec![(0, 0..text.len())];
            let (left, right) = split(&pass, &[&text]).unwrap();
            let first = &left[0].1;
            let second = &right[0].1;
            assert_eq!(first.start, 0);
            assert_eq!(second.end, text.len());
            assert!(first.end >= second.start);
            assert!(first.len() < text.len() && second.len() < text.len());
            assert!(text.get(first.clone()).is_some() && text.get(second.clone()).is_some());
            assert!(first.end - second.start >= EVIDENCE_BYTES);
        }
        assert!(split(&vec![(0, 0..3)], &["日"]).is_none());
    }
}
