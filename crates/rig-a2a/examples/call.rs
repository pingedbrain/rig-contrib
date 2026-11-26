//! Send one message to any A2A agent (v1.0 or v0.3) and print the answer as
//! it streams in.
//!
//! ```sh
//! cargo run --example call -- http://127.0.0.1:3000 "Hi" [--token T] [--expect TEXT]
//! ```
//!
//! With `--expect`, exits with an error unless the answer contains `TEXT`,
//! which makes it a one-line interoperability check.

use std::io::Write;

use anyhow::{Context, bail};
use futures::StreamExt;
use rig_a2a::{RemoteAgent, ReplyEvent};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let url = args
        .next()
        .context("usage: call <url> <text> [--token T] [--expect TEXT]")?;
    let text = args.next().context("missing the message text")?;
    let (mut token, mut expect) = (None, None);
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--token" => token = args.next(),
            "--expect" => expect = args.next(),
            other => bail!("unknown flag {other}"),
        }
    }

    let mut builder = RemoteAgent::builder(url);
    if let Some(token) = token {
        builder = builder.bearer_token(token);
    }
    let remote = builder.connect().await?;
    eprintln!("[{}] streams: {}", remote.card().name, remote.streams());

    let mut events = remote.stream(&text, None).await?;
    let mut answer = None;
    while let Some(event) = events.next().await {
        match event? {
            ReplyEvent::Status(state) => eprintln!("[{state:?}]"),
            ReplyEvent::Text(more) => print!("{more}"),
            ReplyEvent::Rewrite(all) => print!("\n[rewrite]\n{all}"),
            ReplyEvent::Done(reply) => answer = Some(reply),
        }
        std::io::stdout().flush()?;
    }
    println!();
    let reply = answer.context("no answer")?;
    for data in &reply.data {
        println!("[data] {data}");
    }
    for file in &reply.files {
        println!("[file] {:?} {:?}", file.name, file.media_type);
    }
    if let Some(expected) = expect
        && !reply.text.contains(&expected)
    {
        bail!(
            "expected the answer to contain {expected:?}, got {:?}",
            reply.text
        );
    }
    Ok(())
}
