//! Local calibration probe using the production request and decision decoder.
use intercat_daemon::{
    Result,
    decision::{Candidate, Request, Target},
    editor_calibration::{Applied, Identity},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::{self, Read};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Case {
    text: String,
    target: Target,
    tracks: Vec<Candidate>,
    selected: Option<String>,
    later: Option<String>,
    response: Option<Value>,
    calibration: Option<Applied>,
}

fn main() -> Result<()> {
    let mut input = String::new();
    io::stdin().read_to_string(&mut input)?;
    let cases: Vec<Case> = serde_json::from_str(&input)?;
    let mut results = Vec::new();
    for case in cases {
        let mut request = Request::new(
            &case.text,
            case.tracks,
            case.target,
            case.selected.as_deref(),
        )?;
        if let Some(later) = case.later {
            request.reread(&later)?;
        }
        let identity = Identity::for_prefix(
            request.wire["prefix"].as_str().unwrap(),
            request.wire["cachePrefix"].as_bool().unwrap(),
        );
        if let Some(calibration) = case.calibration {
            request.calibrate(calibration)?;
        }
        let decision = case
            .response
            .map(|response| request.resolve(response))
            .transpose()?;
        results.push(json!({"request":request.wire,"identity":identity,"profile":identity.id(),"decision":decision}));
    }
    println!("{}", serde_json::to_string(&results)?);
    Ok(())
}
