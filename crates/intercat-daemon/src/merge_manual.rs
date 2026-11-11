//! Manual conflict choices refer to one frozen Run. The editor recognizes only
//! that Run's marked lines, never similar text from the original source file.
use crate::Result;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Snapshot {
    pub operation: Uuid,
    pub reference: String,
    pub sha: String,
    pub digest: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Begin {
    pub op_id: Uuid,
    pub id: Uuid,
    pub revision: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Read {
    pub id: Uuid,
    pub revision: u64,
    pub path: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Open {
    pub op_id: Uuid,
    pub id: Uuid,
    pub revision: u64,
    pub path: String,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Side {
    Target,
    Incoming,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum Edit {
    Text {
        path: String,
        version: String,
        text: String,
    },
    Side {
        path: String,
        version: String,
        side: Side,
    },
    Current {
        path: String,
        version: String,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Apply {
    pub op_id: Uuid,
    pub id: Uuid,
    pub revision: u64,
    pub files: Vec<Edit>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Hunk {
    pub start_line: u32,
    pub end_line: u32,
    pub ours: String,
    pub theirs: String,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mark {
    Target,
    Base,
    Incoming,
}
fn tag(line: &[u8], marker: Uuid) -> Option<Mark> {
    let first = *line.first()?;
    let mark = match first {
        b'<' => Mark::Target,
        b'|' => Mark::Base,
        b'>' => Mark::Incoming,
        _ => return None,
    };
    let count = line.iter().take_while(|byte| **byte == first).count();
    if !(7..=255).contains(&count) || line.get(count) != Some(&b' ') {
        return None;
    }
    let label = format!(
        "Intercat {} {marker}",
        match mark {
            Mark::Target => "target",
            Mark::Base => "base",
            Mark::Incoming => "change",
        }
    );
    let rest = &line[count + 1..];
    if rest.starts_with(label.as_bytes())
        && rest
            .get(label.len())
            .is_none_or(|byte| matches!(byte, b'\r' | b'\n' | b':' | b' ' | b'\t'))
    {
        Some(mark)
    } else {
        None
    }
}
pub fn has_markers(bytes: &[u8], marker: Uuid) -> bool {
    bytes
        .split_inclusive(|byte| *byte == b'\n')
        .any(|line| tag(line, marker).is_some())
}
pub fn contains_markers(mut reader: impl std::io::Read, marker: Uuid) -> Result<bool> {
    // Generated markers have at most 255 repeated bytes. Retain only each
    // line's prefix, so a large binary/LFS line cannot allocate unbounded RAM.
    let mut buffer = [0u8; 8192];
    let mut prefix = Vec::with_capacity(512);
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            return Ok(tag(&prefix, marker).is_some());
        }
        for byte in &buffer[..count] {
            if prefix.len() < 512 {
                prefix.push(*byte);
            }
            if *byte == b'\n' {
                if tag(&prefix, marker).is_some() {
                    return Ok(true);
                }
                prefix.clear();
            }
        }
    }
}
fn separator(line: &str, size: usize) -> bool {
    let text = line.trim_end_matches(['\r', '\n']);
    text.len() == size && text.bytes().all(|byte| byte == b'=')
}
pub fn hunks(text: &str, marker: Uuid) -> Result<Vec<Hunk>> {
    let lines = text.split_inclusive('\n').collect::<Vec<_>>();
    let mut result = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        match tag(lines[index].as_bytes(), marker) {
            None => {
                index += 1;
                continue;
            }
            Some(Mark::Target) => {}
            _ => return Err("The conflict markers changed. Review the file again.".into()),
        }
        let start = index;
        let size = lines[index]
            .bytes()
            .take_while(|byte| *byte == b'<')
            .count();
        index += 1;
        let ours_start = index;
        while index < lines.len()
            && tag(lines[index].as_bytes(), marker).is_none()
            && !separator(lines[index], size)
        {
            index += 1;
        }
        let ours_end = index;
        if index < lines.len() && tag(lines[index].as_bytes(), marker) == Some(Mark::Base) {
            index += 1;
            while index < lines.len()
                && tag(lines[index].as_bytes(), marker).is_none()
                && !separator(lines[index], size)
            {
                index += 1;
            }
        }
        if index == lines.len() || !separator(lines[index], size) {
            return Err("The conflict separator is missing. Review the file again.".into());
        }
        index += 1;
        let theirs_start = index;
        while index < lines.len() && tag(lines[index].as_bytes(), marker).is_none() {
            if separator(lines[index], size) {
                return Err("This hunk also contains a separator as source text. Edit the result directly or choose a whole file version.".into());
            }
            index += 1;
        }
        if index == lines.len() || tag(lines[index].as_bytes(), marker) != Some(Mark::Incoming) {
            return Err("The conflict end marker is missing. Review the file again.".into());
        }
        result.push(Hunk {
            start_line: u32::try_from(start + 1)?,
            end_line: u32::try_from(index + 1)?,
            ours: lines[ours_start..ours_end].concat(),
            theirs: lines[theirs_start..index].concat(),
        });
        index += 1;
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn recognizes_only_its_own_full_line_marks_and_keeps_japanese_crlf_and_diff3() -> Result<()> {
        let marker = Uuid::now_v7();
        let other = Uuid::now_v7();
        let text = format!(
            "const example = '<<<<<<< Intercat target {marker}';\r\n<<<<<<< Intercat target {marker}\r\n日本語のA\r\n||||||| Intercat base {marker}\r\n前の文\r\n=======\r\n日本語のB\r\n>>>>>>> Intercat change {marker}\r\n<<<<<<< Intercat target {other}\r\nold marks\r\n>>>>>>> Intercat change {other}\r\n<<<<<<< Intercat target {marker}\r\n最後のA\r\n=======\r\n最後のB\r\n>>>>>>> Intercat change {marker}"
        );
        let parsed = hunks(&text, marker)?;
        assert_eq!(parsed.len(), 2);
        assert_eq!(
            parsed[0],
            Hunk {
                start_line: 2,
                end_line: 8,
                ours: "日本語のA\r\n".into(),
                theirs: "日本語のB\r\n".into()
            }
        );
        assert_eq!(parsed[1].start_line, 12);
        assert_eq!(parsed[1].end_line, 16);
        assert_eq!(parsed[1].ours, "最後のA\r\n");
        assert!(has_markers(text.as_bytes(), marker));
        assert!(!has_markers(
            format!("const text='<<<<<<< Intercat target {marker}';\n").as_bytes(),
            marker
        ));
        assert!(
            hunks(
                &format!("<<<<<<< Intercat target {other}\nold\n>>>>>>> Intercat change {other}\n"),
                marker
            )?
            .is_empty()
        );
        Ok(())
    }
    #[test]
    fn incomplete_or_crossed_own_hunks_are_never_reported_as_resolved() {
        let marker = Uuid::now_v7();
        for text in [
            format!("<<<<<<< Intercat target {marker}\nA\n=======\nB\n"),
            format!("<<<<<<< Intercat target {marker}\nA\n>>>>>>> Intercat change {marker}\n"),
            format!(">>>>>>> Intercat change {marker}\n"),
        ] {
            assert!(has_markers(text.as_bytes(), marker));
            assert!(hunks(&text, marker).is_err());
        }
    }

    #[test]
    fn literal_separators_never_silently_choose_the_wrong_side() {
        let marker = Uuid::now_v7();
        let text = format!(
            "<<<<<<< Intercat target {marker}\nA\n=======\na literal source separator\n=======\nB\n>>>>>>> Intercat change {marker}\n"
        );
        assert!(hunks(&text, marker).is_err());
        assert!(has_markers(text.as_bytes(), marker));
    }
}
