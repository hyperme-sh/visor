//! Broker for one public GitHub issue. Plugin processes never handle credentials.
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{io::Read, path::PathBuf, time::Duration};
use zeroize::Zeroizing;

const ORIGIN: &str = "https://api.github.com";
const HOST: &str = "https://github.com";
const EGRESS_SOCKET: &str = "VISOR_GITHUB_ISSUE_EGRESS_SOCKET";
const MAX_RESPONSE: u64 = 512 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Selection {
    // Numeric GitHub user ID remains stable across login renames.
    pub account: u64,
    pub repository: String,
}

impl Selection {
    pub(crate) fn valid(&self) -> bool {
        fn name(value: &str, max: usize, punctuation: &[u8]) -> bool {
            !value.is_empty()
                && value.len() <= max
                && !matches!(value, "." | "..")
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || punctuation.contains(&byte))
        }
        self.account != 0
            && self
                .repository
                .split_once('/')
                .is_some_and(|(owner, repo)| name(owner, 39, b"-") && name(repo, 100, b"-_."))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Error {
    Refused,
    Revoked,
    Unavailable,
    Cancelled,
}

pub(crate) fn credential(secret: &[u8]) -> Option<&str> {
    let token = std::str::from_utf8(secret).ok()?;
    let suffix = token.strip_prefix("github_pat_")?;
    ((20..=255).contains(&suffix.len())
        && suffix
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_'))
    .then_some(token)
}

#[derive(Deserialize)]
struct Account {
    id: u64,
}

#[derive(Deserialize)]
struct Repository {
    full_name: String,
    private: bool,
    visibility: String,
}

#[derive(Deserialize)]
struct Issue {
    id: u64,
    number: u64,
    repository_url: String,
    html_url: String,
    title: String,
    body: Option<String>,
    state: String,
}

pub(crate) fn read(
    selected: &Selection,
    requested: &Selection,
    number: u64,
    secret: &[u8],
    live: impl Fn() -> bool,
) -> Result<Value, Error> {
    let socket = std::env::var_os(EGRESS_SOCKET).map(PathBuf::from);
    read_at(ORIGIN, socket, selected, requested, number, secret, live)
}

#[cfg(test)]
pub(crate) fn fixture_read(
    address: std::net::SocketAddr,
    selected: &Selection,
    requested: &Selection,
    number: u64,
    secret: &[u8],
    live: impl Fn() -> bool,
) -> Result<Value, Error> {
    assert!(address.ip().is_loopback());
    read_at(
        &format!("http://{address}"),
        None,
        selected,
        requested,
        number,
        secret,
        live,
    )
}

fn read_at(
    origin: &str,
    socket: Option<PathBuf>,
    selected: &Selection,
    requested: &Selection,
    number: u64,
    secret: &[u8],
    live: impl Fn() -> bool,
) -> Result<Value, Error> {
    let token = credential(secret).ok_or(Error::Revoked)?;
    let client = client(socket)?;
    resolve(
        selected,
        requested,
        number,
        token,
        live,
        |path, authorized| request(&client, origin, path, authorized.then_some(token)),
    )
}

fn client(socket: Option<PathBuf>) -> Result<reqwest::blocking::Client, Error> {
    let builder = reqwest::blocking::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .http1_only();
    let builder = match socket {
        None => builder,
        #[cfg(unix)]
        Some(path) if path.is_absolute() => builder.unix_socket(path),
        Some(_) => return Err(Error::Unavailable),
    };
    builder.build().map_err(|_| Error::Unavailable)
}

fn request(
    client: &reqwest::blocking::Client,
    origin: &str,
    path: &str,
    token: Option<&str>,
) -> Result<Value, Error> {
    let mut request = client
        .get(format!("{origin}{path}"))
        .header("User-Agent", "hypermesh-visor")
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2026-03-10");
    if let Some(token) = token {
        let authorization = Zeroizing::new(format!("Bearer {token}"));
        let mut header =
            reqwest::header::HeaderValue::from_str(&authorization).map_err(|_| Error::Revoked)?;
        header.set_sensitive(true);
        request = request.header(reqwest::header::AUTHORIZATION, header);
    }
    let response = request.send().map_err(|_| Error::Unavailable)?;
    match response.status().as_u16() {
        200 => (),
        401 => return Err(Error::Revoked),
        403 | 404 | 410 => return Err(Error::Refused),
        _ => return Err(Error::Unavailable),
    }
    let mut body = Zeroizing::new(Vec::new());
    response
        .take(MAX_RESPONSE + 1)
        .read_to_end(&mut body)
        .map_err(|_| Error::Unavailable)?;
    if body.len() as u64 > MAX_RESPONSE {
        return Err(Error::Unavailable);
    }
    serde_json::from_slice(&body).map_err(|_| Error::Unavailable)
}

fn resolve(
    selected: &Selection,
    requested: &Selection,
    number: u64,
    token: &str,
    live: impl Fn() -> bool,
    mut get: impl FnMut(&str, bool) -> Result<Value, Error>,
) -> Result<Value, Error> {
    if !selected.valid() || selected != requested || number == 0 {
        return Err(Error::Refused);
    }
    let mut fetch = |path, authorized| {
        if !live() {
            return Err(Error::Cancelled);
        }
        let response = get(path, authorized);
        if !live() {
            return Err(Error::Cancelled);
        }
        response
    };
    let account: Account =
        serde_json::from_value(fetch("/user", true)?).map_err(|_| Error::Unavailable)?;
    if account.id != selected.account {
        return Err(Error::Refused);
    }
    let repo_path = format!("/repos/{}", selected.repository);
    let repository: Repository =
        serde_json::from_value(fetch(&repo_path, true)?).map_err(|_| Error::Unavailable)?;
    if repository.full_name != selected.repository
        || repository.private
        || repository.visibility != "public"
    {
        return Err(Error::Refused);
    }
    let issue_path = format!("{repo_path}/issues/{number}");
    // Fetch issue content without a token so a visibility change after the
    // repository check cannot make private data readable.
    let raw = fetch(&issue_path, false)?;
    if raw.get("pull_request").is_some() {
        return Err(Error::Refused);
    }
    let issue: Issue = serde_json::from_value(raw).map_err(|_| Error::Unavailable)?;
    let url = format!("{HOST}/{}/issues/{number}", selected.repository);
    if issue.id == 0
        || issue.number != number
        || issue.repository_url != format!("{ORIGIN}{repo_path}")
        || issue.html_url != url
        || !matches!(issue.state.as_str(), "open" | "closed")
    {
        return Err(Error::Refused);
    }
    Ok(json!({
        "id": issue.id,
        "number": number,
        "url": url,
        "state": issue.state,
        "title": clean(&issue.title, token, 256),
        "body": issue.body.as_deref().map(|body| clean(body, token, 4096)),
    }))
}

fn clean(value: &str, token: &str, limit: usize) -> String {
    value
        .chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect::<String>()
        .replace(token, "[redacted]")
        .chars()
        .take(limit)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    const TOKEN: &str = "github_pat_fixture00000000000000000";

    fn selection() -> Selection {
        Selection {
            account: 123,
            repository: "Example/Project".into(),
        }
    }

    fn fixture(path: &str, private: bool) -> Value {
        match path {
            "/user" => json!({"id":123, "login":"example"}),
            "/repos/Example/Project" => json!({
                "full_name":"Example/Project", "private":private,
                "visibility":if private {"private"} else {"public"}
            }),
            "/repos/Example/Project/issues/8" => json!({
                "id":456, "number":8, "title":TOKEN, "state":"open",
                "repository_url":"https://api.github.com/repos/Example/Project",
                "html_url":"https://github.com/Example/Project/issues/8",
                "body":"a".repeat(4200), "other":"discard"
            }),
            _ => panic!("unexpected provider path"),
        }
    }

    #[test]
    fn provider_read_is_account_scoped_and_projects_bounded_fields() {
        let selected = selection();
        let mut paths = Vec::new();
        let output = resolve(
            &selected,
            &selected,
            8,
            TOKEN,
            || true,
            |path, authorized| {
                assert_eq!(authorized, !path.contains("/issues/"));
                paths.push(path.to_owned());
                Ok(fixture(path, false))
            },
        )
        .unwrap();
        assert_eq!(
            paths,
            [
                "/user",
                "/repos/Example/Project",
                "/repos/Example/Project/issues/8"
            ]
        );
        assert_eq!(output["id"], 456);
        assert_eq!(output["title"], "[redacted]");
        assert_eq!(output["body"].as_str().unwrap().len(), 4096);
        assert_eq!(output.as_object().unwrap().len(), 6);
    }

    #[test]
    fn scope_mismatch_private_and_provider_unavailability_fail_closed() {
        let selected = selection();
        let mut wrong = selected.clone();
        wrong.repository = "Example/Other".into();
        assert_eq!(
            resolve(&selected, &wrong, 8, TOKEN, || true, |_, _| unreachable!()),
            Err(Error::Refused)
        );
        assert_eq!(
            resolve(
                &selected,
                &selected,
                8,
                TOKEN,
                || true,
                |_, _| Ok(json!({"id": 42}))
            ),
            Err(Error::Refused)
        );
        assert_eq!(
            resolve(
                &selected,
                &selected,
                8,
                TOKEN,
                || true,
                |path, _| {
                    assert!(!path.contains("/issues/"));
                    Ok(fixture(path, true))
                }
            ),
            Err(Error::Refused)
        );
        for error in [Error::Revoked, Error::Unavailable] {
            assert_eq!(
                resolve(&selected, &selected, 8, TOKEN, || true, |_, _| Err(error)),
                Err(error)
            );
        }
    }

    #[test]
    fn local_revocation_stops_the_next_request() {
        let selected = selection();
        let live = Cell::new(true);
        assert_eq!(
            resolve(
                &selected,
                &selected,
                8,
                TOKEN,
                || live.get(),
                |path, _| {
                    assert_eq!(path, "/user");
                    live.set(false);
                    Ok(fixture(path, false))
                }
            ),
            Err(Error::Cancelled)
        );
    }

    #[cfg(unix)]
    #[test]
    fn github_egress_uses_the_configured_socket_and_rejects_redirects() {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixListener;

        let socket = std::env::temp_dir().join(format!("visor-gh-{}.sock", uuid::Uuid::new_v4()));
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let listening = listener.try_clone().unwrap();
        let worker = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + REQUEST_TIMEOUT;
            let mut stream = loop {
                match listening.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(std::time::Instant::now() < deadline, "no socket request");
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("socket accept failed: {error}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream.set_read_timeout(Some(REQUEST_TIMEOUT)).unwrap();
            let mut reader = BufReader::new(&stream);
            let mut headers = String::new();
            loop {
                let mut line = String::new();
                assert!(reader.read_line(&mut line).unwrap() > 0);
                if line == "\r\n" {
                    break;
                }
                headers.push_str(&line);
            }
            assert!(headers.starts_with("GET /user HTTP/1.1\r\n"));
            let headers = headers.to_ascii_lowercase();
            assert!(headers.contains("host: api.example.test\r\n"));
            assert!(headers.contains(&format!("authorization: bearer {TOKEN}\r\n")));
            stream.write_all(b"HTTP/1.1 302 Found\r\nLocation: http://example.test/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
        });
        let client = client(Some(socket.clone())).unwrap();
        // Use HTTP for this socket fixture. Live requests use the fixed TLS origin.
        assert_eq!(
            request(&client, "http://api.example.test", "/user", Some(TOKEN)),
            Err(Error::Unavailable)
        );
        worker.join().unwrap();
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        std::fs::remove_file(&socket).unwrap();
        let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        tcp.set_nonblocking(true).unwrap();
        let direct = format!("http://{}", tcp.local_addr().unwrap());
        assert_eq!(
            request(&client, &direct, "/user", Some(TOKEN)),
            Err(Error::Unavailable)
        );
        assert_eq!(
            tcp.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
}
