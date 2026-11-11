//! Bounded GitHub REST requests. A PR creation never invokes Git or pushes refs.
use crate::{
    Result,
    github_repository::{Draft, Target},
    pull_request::{Account, Pull},
};
use serde::Deserialize;
use serde_json::json;
use std::time::Duration;
const RESPONSE_LIMIT: usize = 2 * 1024 * 1024;
pub struct Client {
    http: reqwest::Client,
    origin: reqwest::Url,
}
/// The token is never serialized, logged or passed in command arguments.
pub struct Token(String);
impl Token {
    pub(crate) fn new(value: String) -> Result<Self> {
        if value.is_empty()
            || value.len() > 4096
            || value.chars().any(char::is_whitespace)
            || value.chars().any(char::is_control)
        {
            return Err("GitHub authentication did not return a usable token.".into());
        }
        Ok(Self(value))
    }
}
#[derive(Deserialize)]
struct Branch {
    object: Sha,
}
#[derive(Deserialize)]
struct Sha {
    sha: String,
}
#[derive(Deserialize)]
struct RemoteRepository {
    full_name: String,
}
#[derive(Deserialize)]
struct Reference {
    #[serde(rename = "ref")]
    reference: String,
    sha: String,
    repo: RemoteRepository,
}
#[derive(Deserialize)]
struct RemotePull {
    number: u64,
    html_url: String,
    title: String,
    body: Option<String>,
    head: Reference,
    base: Reference,
    state: String,
}
impl RemotePull {
    fn checked(self, target: &Target) -> Result<Pull> {
        let url = reqwest::Url::parse(&self.html_url)
            .map_err(|_| "GitHub returned an invalid PR URL.")?;
        let path = format!("/{}/pull/{}", target.repository.full_name(), self.number);
        if self.number == 0
            || url.scheme() != "https"
            || url.host_str() != Some("github.com")
            || url.path() != path
            || !url.username().is_empty()
            || url.password().is_some()
            || url.port().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || self.base.reference != target.base
            || self.head.reference != target.head
            || !self
                .head
                .repo
                .full_name
                .eq_ignore_ascii_case(&target.repository.full_name())
            || !self
                .base
                .repo
                .full_name
                .eq_ignore_ascii_case(&target.repository.full_name())
            || !matches!(self.state.as_str(), "open" | "closed")
        {
            return Err(
                "The PR response does not match the reviewed repository and branches.".into(),
            );
        }
        self.head.sha.parse::<intercat_git::ObjectId>()?;
        Ok(Pull {
            number: self.number,
            url: self.html_url,
            title: self.title,
            body: self.body.unwrap_or_default(),
            head_sha: self.head.sha,
            state: self.state,
        })
    }
}
impl Client {
    pub fn github() -> Result<Self> {
        Self::at(reqwest::Url::parse("https://api.github.com/")?)
    }
    #[cfg(test)]
    pub(crate) fn loopback(origin: reqwest::Url) -> Result<Self> {
        if origin.scheme() != "http" || origin.host_str() != Some("127.0.0.1") {
            return Err("A dummy API must stay on loopback.".into());
        }
        Self::at(origin)
    }
    fn at(origin: reqwest::Url) -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .retry(reqwest::retry::never())
                .referer(false)
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(20))
                .user_agent("Intercat")
                .build()?,
            origin,
        })
    }
    fn url(&self, target: &Target, tail: &[&str]) -> Result<reqwest::Url> {
        target.validate()?;
        let mut url = self.origin.clone();
        url.path_segments_mut()
            .map_err(|_| "Invalid GitHub API origin.")?
            .extend(["repos", &target.repository.owner, &target.repository.name])
            .extend(tail);
        Ok(url)
    }
    fn request(
        &self,
        method: reqwest::Method,
        url: reqwest::Url,
        token: &Token,
    ) -> reqwest::RequestBuilder {
        self.http
            .request(method, url)
            .bearer_auth(&token.0)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2026-03-10")
    }
    async fn response<T: serde::de::DeserializeOwned>(
        mut response: reqwest::Response,
    ) -> Result<T> {
        if !response.status().is_success() {
            return Err(if response.status()==reqwest::StatusCode::UNAUTHORIZED||response.status()==reqwest::StatusCode::FORBIDDEN {"GitHub did not authorize this request. Check the selected account and repository permissions."} else {"GitHub did not complete this request. Its raw response has not been recorded."}.into());
        }
        if response
            .content_length()
            .is_some_and(|length| length > RESPONSE_LIMIT as u64)
        {
            return Err("GitHub returned a response larger than 2 MiB.".into());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| "The GitHub response was interrupted.")?
        {
            if bytes.len().saturating_add(chunk.len()) > RESPONSE_LIMIT {
                return Err("GitHub returned a response larger than 2 MiB.".into());
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes)
            .map_err(|_| "GitHub returned an incomplete or invalid response.".into())
    }
    pub async fn account(&self, token: &Token) -> Result<Account> {
        #[derive(Deserialize)]
        struct User {
            id: u64,
            login: String,
        }
        let response = self
            .request(reqwest::Method::GET, self.origin.join("user")?, token)
            .send()
            .await
            .map_err(|_| "The GitHub account could not be read.")?;
        let user: User = Self::response(response).await?;
        if user.id == 0
            || user.login.is_empty()
            || user.login.len() > 256
            || user.login.chars().any(char::is_control)
        {
            return Err("GitHub returned an invalid account identity.".into());
        }
        Ok(Account {
            id: user.id,
            login: user.login,
        })
    }
    pub async fn pull(&self, target: &Target, number: u64, token: &Token) -> Result<Pull> {
        if number == 0 {
            return Err("Invalid PR number.".into());
        }
        let response = self
            .request(
                reqwest::Method::GET,
                self.url(target, &["pulls", &number.to_string()])?,
                token,
            )
            .send()
            .await
            .map_err(|_| "The PR could not be read on GitHub.")?;
        let pull: RemotePull = Self::response(response).await?;
        pull.checked(target)
    }
    pub async fn edit(
        &self,
        target: &Target,
        number: u64,
        draft: &Draft,
        token: &Token,
    ) -> Result<Pull> {
        if number == 0 {
            return Err("Invalid PR number.".into());
        }
        draft.validate()?;
        self.verify_head(target, token).await?;
        let response = self
            .request(
                reqwest::Method::PATCH,
                self.url(target, &["pulls", &number.to_string()])?,
                token,
            )
            .json(&json!({"title":draft.title,"body":draft.body}))
            .send()
            .await
            .map_err(|_| "The PR update result is unknown. Read it again before updating.")?;
        let pull: RemotePull = Self::response(response).await?;
        pull.checked(target)
    }
    pub async fn verify_head(&self, target: &Target, token: &Token) -> Result<()> {
        let mut tail = vec!["git", "ref", "heads"];
        tail.extend(target.head.split('/'));
        let response = self
            .request(reqwest::Method::GET, self.url(target, &tail)?, token)
            .send()
            .await
            .map_err(|_| "The reviewed branch could not be read on GitHub.")?;
        let branch: Branch = Self::response(response).await?;
        if branch.object.sha != target.sha {
            return Err(
                "The branch on GitHub changed. Review its current commit before creating PR."
                    .into(),
            );
        }
        Ok(())
    }
    pub async fn existing(&self, target: &Target, token: &Token) -> Result<Vec<Pull>> {
        self.matching(target, token, "open").await
    }
    pub async fn history(&self, target: &Target, token: &Token) -> Result<Vec<Pull>> {
        self.matching(target, token, "all").await
    }
    async fn matching(&self, target: &Target, token: &Token, state: &str) -> Result<Vec<Pull>> {
        let mut url = self.url(target, &["pulls"])?;
        url.query_pairs_mut().extend_pairs([
            ("state", state),
            (
                "head",
                &format!("{}:{}", target.repository.owner, target.head),
            ),
            ("base", &target.base),
            ("per_page", "100"),
        ]);
        let response = self
            .request(reqwest::Method::GET, url, token)
            .send()
            .await
            .map_err(|_| "Existing PRs could not be read on GitHub.")?;
        let pulls: Vec<RemotePull> = Self::response(response).await?;
        if pulls.len() >= 100 {
            return Err("Too many matching PRs to confirm a complete result.".into());
        }
        pulls.into_iter().map(|pull| pull.checked(target)).collect()
    }
    /// Call only after the durable explicit confirmation has been recorded.
    pub async fn create(&self, target: &Target, draft: &Draft, token: &Token) -> Result<Pull> {
        target.validate()?;
        draft.validate()?;
        self.verify_head(target, token).await?;
        let response=self.request(reqwest::Method::POST,self.url(target,&["pulls"])?,token).json(&json!({"title":draft.title,"body":draft.body,"head":target.head,"base":target.base,"maintainer_can_modify":true})).send().await.map_err(|_|"The PR creation result is unknown. Check existing PRs before creating again.")?;
        let pull: RemotePull = Self::response(response).await?;
        pull.checked(target)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };
    fn target() -> Target {
        Target {
            repository: crate::github_repository::Repository {
                owner: "own-dummy".into(),
                name: "dummy-repo".into(),
            },
            base: "main".into(),
            head: "intercat/dummy".into(),
            sha: "a".repeat(40),
        }
    }
    fn pull() -> serde_json::Value {
        json!({"number":7,"html_url":"https://github.com/own-dummy/dummy-repo/pull/7","title":"Own dummy title","body":"Own generated description","head":{"ref":"intercat/dummy","sha":"a".repeat(40),"repo":{"full_name":"own-dummy/dummy-repo"}},"base":{"ref":"main","sha":"c".repeat(40),"repo":{"full_name":"own-dummy/dummy-repo"}},"state":"open"})
    }
    pub(crate) async fn server(
        replies: Vec<(String, u16, serde_json::Value)>,
    ) -> Result<(
        reqwest::Url,
        tokio::task::JoinHandle<Result<Vec<serde_json::Value>>>,
    )> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let url = reqwest::Url::parse(&format!("http://{}/", listener.local_addr()?))?;
        let task = tokio::spawn(async move {
            let mut bodies = Vec::new();
            for (expected, status, value) in replies {
                let (mut stream, _) = listener.accept().await?;
                let mut bytes = Vec::new();
                let mut buffer = [0; 4096];
                let boundary = loop {
                    let count = stream.read(&mut buffer).await?;
                    if count == 0 {
                        return Err("Dummy request ended early.".into());
                    }
                    bytes.extend_from_slice(&buffer[..count]);
                    if bytes.len() > 128 * 1024 {
                        return Err("Dummy request too large.".into());
                    }
                    if let Some(position) = bytes.windows(4).position(|bytes| bytes == b"\r\n\r\n")
                    {
                        break position + 4;
                    }
                };
                let header = std::str::from_utf8(&bytes[..boundary])?;
                assert!(header.starts_with(&expected));
                assert!(
                    header
                        .to_ascii_lowercase()
                        .contains("authorization: bearer dummy-token-local-only\r\n")
                );
                let length = header
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|value| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                while bytes.len() < boundary + length {
                    let count = stream.read(&mut buffer).await?;
                    if count == 0 {
                        return Err("Dummy body ended early.".into());
                    }
                    bytes.extend_from_slice(&buffer[..count]);
                }
                bodies.push(if length == 0 {
                    serde_json::Value::Null
                } else {
                    serde_json::from_slice(&bytes[boundary..boundary + length])?
                });
                let body = serde_json::to_vec(&value)?;
                stream.write_all(format!("HTTP/1.1 {status} Dummy\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len()).as_bytes()).await?;
                stream.write_all(&body).await?;
            }
            Ok(bodies)
        });
        Ok((url, task))
    }
    #[tokio::test]
    async fn creation_checks_the_frozen_head_and_sends_only_the_reviewed_json() -> Result<()> {
        let (url, server) = server(vec![
            (
                "GET /repos/own-dummy/dummy-repo/git/ref/heads/intercat/dummy HTTP/1.1".into(),
                200,
                json!({"object":{"sha":"a".repeat(40)}}),
            ),
            (
                "POST /repos/own-dummy/dummy-repo/pulls HTTP/1.1".into(),
                201,
                pull(),
            ),
        ])
        .await?;
        let client = Client::at(url)?;
        let draft = Draft {
            title: "Own dummy title".into(),
            body: "Own generated description".into(),
        };
        let result = client
            .create(
                &target(),
                &draft,
                &Token::new("dummy-token-local-only".into())?,
            )
            .await?;
        assert_eq!(result.number, 7);
        let bodies = server.await??;
        assert!(bodies[0].is_null());
        assert_eq!(
            bodies[1],
            json!({"title":draft.title,"body":draft.body,"head":"intercat/dummy","base":"main","maintainer_can_modify":true})
        );
        Ok(())
    }
    #[tokio::test]
    async fn changed_remote_head_prevents_creation_and_wrong_repository_responses_are_rejected()
    -> Result<()> {
        let (url, server) = server(vec![(
            "GET /repos/own-dummy/dummy-repo/git/ref/heads/intercat/dummy HTTP/1.1".into(),
            200,
            json!({"object":{"sha":"b".repeat(40)}}),
        )])
        .await?;
        assert!(
            Client::at(url)?
                .create(
                    &target(),
                    &Draft {
                        title: "Dummy".into(),
                        body: String::new()
                    },
                    &Token::new("dummy-token-local-only".into())?
                )
                .await
                .is_err()
        );
        assert_eq!(server.await??.len(), 1);
        let mut wrong = pull();
        wrong["head"]["repo"]["full_name"] = json!("another-dummy/wrong-repo");
        assert!(
            serde_json::from_value::<RemotePull>(wrong)?
                .checked(&target())
                .is_err()
        );
        let mut wrong = pull();
        wrong["html_url"] =
            json!("https://github.com.attacker.invalid/own-dummy/dummy-repo/pull/7");
        assert!(
            serde_json::from_value::<RemotePull>(wrong)?
                .checked(&target())
                .is_err()
        );
        Ok(())
    }
    #[tokio::test]
    async fn account_existing_pull_and_reviewed_edit_keep_the_same_repository_and_branches()
    -> Result<()> {
        let mut updated = pull();
        updated["title"] = json!("Own reviewed update");
        updated["body"] = json!("Own reviewed body with related PR links");
        let(url,server)=server(vec![
            ("GET /user HTTP/1.1".into(),200,json!({"id":42,"login":"own-dummy","unused":"not copied"})),
            ("GET /repos/own-dummy/dummy-repo/pulls?state=open&head=own-dummy%3Aintercat%2Fdummy&base=main&per_page=100 HTTP/1.1".into(),200,json!([pull()])),
            ("GET /repos/own-dummy/dummy-repo/pulls/7 HTTP/1.1".into(),200,pull()),
            ("GET /repos/own-dummy/dummy-repo/git/ref/heads/intercat/dummy HTTP/1.1".into(),200,json!({"object":{"sha":"a".repeat(40)}})),
            ("PATCH /repos/own-dummy/dummy-repo/pulls/7 HTTP/1.1".into(),200,updated),
        ]).await?;
        let client = Client::at(url)?;
        let token = Token::new("dummy-token-local-only".into())?;
        let account = client.account(&token).await?;
        assert_eq!(account.id, 42);
        assert_eq!(account.login, "own-dummy");
        let existing = client.existing(&target(), &token).await?;
        assert_eq!(existing.len(), 1);
        assert_eq!(client.pull(&target(), 7, &token).await?, existing[0]);
        let draft = Draft {
            title: "Own reviewed update".into(),
            body: "Own reviewed body with related PR links".into(),
        };
        assert_eq!(
            client.edit(&target(), 7, &draft, &token).await?.body,
            draft.body
        );
        let bodies = server.await??;
        assert_eq!(bodies[4], json!({"title":draft.title,"body":draft.body}));
        Ok(())
    }
}
