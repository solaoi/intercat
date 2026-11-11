//! PR prose is generated locally, screened and shown before confirmation.
use crate::{
    Result,
    editor::{Manager, Priority},
    github_repository::Draft,
    journal::Journal,
    pull_request::{Job, Sources},
};
use serde_json::json;
const PREFIX: &str = include_str!("prompts/pull_request.txt");

pub(crate) async fn screen(
    journal: &Journal,
    draft: &Draft,
) -> Result<(Draft, crate::screening::EditorRequest)> {
    draft.validate()?;
    let lease = journal
        .screen_editor(
            json!({"tail":serde_json::to_string(draft)?}),
            Priority::Background,
        )
        .await?;
    let safe: Draft = serde_json::from_str(
        lease.params()["tail"]
            .as_str()
            .ok_or("Missing screened PR text.")?,
    )?;
    safe.validate()?;
    Ok((safe, lease))
}
pub(crate) async fn generate(
    journal: &Journal,
    editor: &Manager,
    sources: &Sources,
) -> Result<Draft> {
    let reading = crate::input_readings::Ticket {
        input: uuid::Uuid::now_v7().to_string(),
        revision: 1,
        index: 1,
    };
    let _cancellation = crate::editor::ReadingCancellation {
        editor: editor.clone(),
        request: reading.clone(),
    };
    let request = journal
        .screen_editor(
            json!({"prefix":PREFIX,"tail":serde_json::to_string(sources)?,"maxTokens":512}),
            Priority::Background,
        )
        .await?;
    if editor
        .count_generation_tokens(&request, Some(&reading))
        .await?
        > 8000
    {
        return Err(
            "The saved PR sources exceed the local Editor's context. They have not been truncated."
                .into(),
        );
    }
    let response = editor.generate(request, Some(&reading)).await?;
    if response["preempted"] != false {
        return Err("PR drafting was interrupted. Retry retains its saved sources.".into());
    }
    let draft: Draft = serde_json::from_str(
        response["text"]
            .as_str()
            .ok_or("The local Editor did not return PR text.")?,
    )?;
    Ok(screen(journal, &draft).await?.0)
}
/// Related links are a disclosed, deterministic addition to the approved body.
pub(crate) fn linked(job: &Job, index: usize) -> Result<Draft> {
    let member = job.members.get(index).ok_or("Missing PR folder.")?;
    let mut draft = member.draft.clone().ok_or("Missing reviewed PR text.")?;
    if job.members.len() > 1 {
        let links = job
            .members
            .iter()
            .enumerate()
            .filter(|(position, _)| *position != index)
            .map(|(_, other)| {
                let result = other
                    .result
                    .as_ref()
                    .ok_or("Wait for all related PRs before linking.")?;
                Ok(format!(
                    "{}#{}",
                    other.target.repository.full_name(),
                    result.number
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        draft.body.push_str(&format!(
            "\n\n---\nPart {} of {} · see {}",
            index + 1,
            job.members.len(),
            links.join(", ")
        ));
    }
    draft.validate()?;
    Ok(draft)
}
