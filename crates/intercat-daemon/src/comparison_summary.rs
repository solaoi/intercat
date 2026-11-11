//! Check evidence is descriptive. It never makes a path choice.
use crate::{check_run::State, comparison::Job};
use serde::Serialize;
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Summary {
    label: String,
    reason: Option<String>,
}
fn summary(label: &str, reason: Option<&str>) -> Summary {
    Summary {
        label: label.into(),
        reason: reason.map(str::to_owned),
    }
}
pub(crate) fn summarize(job: &Job) -> Summary {
    let [left, right] = &job.sides;
    let (Some(a), Some(b)) = (&left.outcome, &right.outcome) else {
        return summary("Checks haven’t run on both yet", None);
    };
    let incomparable = |reason| {
        summary(
            "The checks can’t be compared · Compare the diffs",
            Some(reason),
        )
    };
    if !a.cacheable() || !b.cacheable() {
        return incomparable(
            "A Check could not finish against its saved tree. Review its result before retrying.",
        );
    }
    let (Some(a_check), Some(b_check)) = (&a.check, &b.check) else {
        return summary("Checks haven’t run on both yet", None);
    };
    if a_check.command.is_none() && b_check.command.is_none() {
        return summary("No checks configured · Compare the diffs", None);
    }
    if a_check.command != b_check.command {
        return incomparable("The saved paths use different Check commands.");
    }
    match (a.state, b.state) {
        (State::Passed, State::Passed) => {
            summary("The checks don’t tell them apart · Compare the diffs", None)
        }
        (State::Failed, State::Failed) => summary("Both fail the checks · Compare the diffs", None),
        (State::Passed, State::Failed) => summary(
            &format!(
                "Only {} passes the checks · Compare the diffs",
                left.source.label
            ),
            None,
        ),
        (State::Failed, State::Passed) => summary(
            &format!(
                "Only {} passes the checks · Compare the diffs",
                right.source.label
            ),
            None,
        ),
        _ => incomparable("A Check did not produce a comparable exit status."),
    }
}
