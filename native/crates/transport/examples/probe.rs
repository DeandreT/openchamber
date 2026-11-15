//! Exercises the transport against a running server.
//!
//! `cargo run -p openchamber-native-transport --example probe -- http://127.0.0.1:4799 10`
//! probes the server, reads every session and the newest messages of the
//! first one, then prints event counts for the given number of seconds.
//! Set `OPENCHAMBER_NATIVE_PASSWORD` for a password server. Prints counts and
//! identifiers only, never message text or credentials.

use std::collections::BTreeMap;
use std::time::Duration;

use openchamber_native_transport::client::{ClientDescription, ServerClient};
use openchamber_native_transport::credential::Credential;
use openchamber_native_transport::endpoint::Endpoint;
use openchamber_native_transport::event::ServerEvent;
use openchamber_native_transport::stream::{EventStream, StreamItem};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = std::env::args().skip(1);
    let url = arguments
        .next()
        .unwrap_or_else(|| "http://127.0.0.1:4799".to_owned());
    let seconds: u64 = arguments
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or(10);

    let endpoint = Endpoint::parse(&url)?;
    let anonymous = ServerClient::new(endpoint.clone(), Credential::None)?;
    let identity = anonymous.probe().await?;
    println!(
        "server: OpenChamber {} ({}), API {}, {} capabilities",
        identity.openchamber_version,
        identity.runtime,
        identity.api_version,
        identity.capabilities.len()
    );
    let client = match std::env::var("OPENCHAMBER_NATIVE_PASSWORD") {
        Ok(password) => {
            let client = ClientDescription {
                label: "OpenChamber transport probe".to_owned(),
                kind: "desktop".to_owned(),
                ..ClientDescription::default()
            };
            ServerClient::new(endpoint, anonymous.login(&password, &client).await?)?
        }
        Err(_) => anonymous,
    }
    .with_identity(&identity);

    let sessions = client.all_sessions().await?;
    let archived = sessions
        .iter()
        .filter(|session| session.is_archived())
        .count();
    println!("sessions: {} ({} archived)", sessions.len(), archived);
    if let Some(first) = sessions.first() {
        let page = client.latest_messages(&first.id, 50).await?;
        let parts: usize = page
            .messages
            .iter()
            .map(|message| message.parts.len())
            .sum();
        println!(
            "messages of {}: {} with {} parts, older pages: {}",
            first.id,
            page.messages.len(),
            parts,
            page.older.is_some()
        );
    }
    let roots = client.chat_roots().await?;
    println!("chat roots known: {}", roots.is_some());
    let projects = client.settings().await?.projects;
    let paths: Vec<String> = projects
        .iter()
        .map(|project| project.path.clone())
        .collect();
    let worktrees = client.project_worktrees(&paths).await?;
    let listed = worktrees
        .iter()
        .filter(|read| read.worktrees.is_some())
        .count();
    let linked: usize = worktrees
        .iter()
        .filter_map(|read| read.worktrees.as_ref())
        .map(Vec::len)
        .sum();
    println!(
        "projects: {}, worktree lists: {listed}, linked worktrees: {linked}",
        projects.len()
    );

    let mut stream = EventStream::subscribe(client);
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);
    while let Ok(Some(item)) = tokio::time::timeout_at(deadline, stream.next()).await {
        let key = match item {
            StreamItem::Connected { resumed, .. } => format!("connected (resumed {resumed})"),
            StreamItem::Disconnected(error) => format!("disconnected: {error}"),
            StreamItem::Events { events, .. } => {
                for event in events {
                    let name = match event {
                        ServerEvent::Transcript { event, .. } => {
                            format!(
                                "transcript:{}",
                                format!("{event:?}")
                                    .split([' ', '(', '{'])
                                    .next()
                                    .unwrap_or_default()
                            )
                        }
                        ServerEvent::Unsupported { kind } => format!("unsupported:{kind}"),
                        other => format!("{other:?}")
                            .split([' ', '(', '{'])
                            .next()
                            .unwrap_or_default()
                            .to_owned(),
                    };
                    *counts.entry(name).or_default() += 1;
                }
                continue;
            }
        };
        *counts.entry(key).or_default() += 1;
    }
    for (name, count) in counts {
        println!("{count:4} {name}");
    }
    Ok(())
}
