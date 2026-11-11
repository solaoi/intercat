//! Git's bounded wire output is interpreted without recording raw URLs or
//! server output. Credentials may be omitted by Git in its `To` line.
use crate::{
    Result,
    command::Output,
    push_target::{Destination, Target},
};
use intercat_git::{ObjectId, RemoteAddress};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Advertised {
    pub reference: String,
    pub sha: ObjectId,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Receipt {
    pub sha: String,
    pub reference: String,
    pub destination: Destination,
    pub changed: bool,
}
pub(crate) fn advertisement(output: Output) -> Result<Vec<Advertised>> {
    if !output.success() || output.truncated {
        return Err("The Push destination's refs could not be read completely.".into());
    }
    let text = String::from_utf8(output.stdout)
        .map_err(|_| "The Push destination returned invalid refs.")?;
    let mut refs = Vec::new();
    for line in text.lines() {
        let (sha, reference) = line
            .split_once('\t')
            .ok_or("The Push destination returned invalid refs.")?;
        if (reference != "HEAD" && !reference.starts_with("refs/"))
            || reference.chars().any(char::is_control)
        {
            return Err("The Push destination returned invalid refs.".into());
        }
        refs.push(Advertised {
            reference: reference.into(),
            sha: sha
                .parse()
                .map_err(|_| "The Push destination returned invalid refs.")?,
        });
    }
    Ok(refs)
}
pub(crate) fn receipt(output: Output, target: &Target, url: &str) -> Result<Receipt> {
    if !output.success() || output.truncated {
        return Err("Push did not return a complete success receipt. Check the destination before retrying.".into());
    }
    let refspec = target.refspec()?;
    let expected = RemoteAddress::parse(url)?;
    let text = String::from_utf8(output.stdout).map_err(|_| "Push returned an invalid receipt.")?;
    let mut destination = false;
    let mut status = None;
    for line in text.lines() {
        if let Some(reported) = line.strip_prefix("To ") {
            let reported = RemoteAddress::parse(reported)?;
            if destination
                || expected.transport != reported.transport
                || expected.display != reported.display
            {
                return Err("Push reported a different destination.".into());
            }
            destination = true;
        } else if line == "Done" || line.is_empty() {
            continue;
        } else {
            let mut fields = line.split('\t');
            let flag = fields.next().ok_or("Push returned an invalid receipt.")?;
            let to = fields.next().ok_or("Push returned an invalid receipt.")?;
            let summary = fields.next().ok_or("Push returned an invalid receipt.")?;
            if status.is_some()
                || to != refspec
                || fields.next().is_some()
                || summary.is_empty()
                || !["*", " ", "="].contains(&flag)
            {
                return Err("Push reported a different ref or result.".into());
            }
            status = Some(flag != "=");
        }
    }
    if !destination {
        return Err("Push did not identify its destination.".into());
    }
    Ok(Receipt {
        sha: target.sha.clone(),
        reference: target.target_ref.clone(),
        destination: Destination::capture(url)?,
        changed: status.ok_or("Push did not identify its ref.")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn output(text: String) -> Output {
        Output {
            code: Some(0),
            stdout: text.into_bytes(),
            stderr: Vec::new(),
            truncated: false,
            cancelled: false,
            timed_out: false,
        }
    }
    #[test]
    fn receipt_accepts_git_credential_redaction_but_binds_query_and_single_ref() -> Result<()> {
        let url = "https://dummy:DUMMY_LOCAL_PASSWORD@example.invalid/repo.git?tenant=One";
        let target = Target {
            source_ref: "refs/heads/intercat/dummy".into(),
            sha: "ab".repeat(20),
            remote: "origin".into(),
            target_ref: "refs/heads/intercat/dummy".into(),
            destinations: vec![Destination::capture(url)?],
        };
        let valid = |query: &str| {
            format!(
                "To https://example.invalid/repo.git?tenant={query}\n*\t{}\t[new branch]\nDone\n",
                target.refspec().unwrap()
            )
        };
        let saved = receipt(output(valid("One")), &target, url)?;
        assert!(saved.changed);
        let record = serde_json::to_string(&saved)?;
        assert!(!record.contains("DUMMY_LOCAL_PASSWORD"));
        assert!(!record.contains("dummy@"));
        assert!(!record.contains("tenant="));
        assert!(receipt(output(valid("Two")), &target, url).is_err());
        assert!(receipt(output(valid("One").replace("*\t", "+\t")), &target, url).is_err());
        assert!(
            receipt(
                output(
                    valid("One").replace(&target.refspec()?, "refs/heads/other:refs/heads/other")
                ),
                &target,
                url
            )
            .is_err()
        );
        let mut cut = output(valid("One"));
        cut.truncated = true;
        assert!(receipt(cut, &target, url).is_err());
        Ok(())
    }
}
