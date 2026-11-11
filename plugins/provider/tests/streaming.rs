//! What happens when a streaming response ends badly.
//!
//! A provider's stream can be complete and its connection still end wrong: an
//! HTTP/2 stream reset, a proxy dropping the socket, a chunked body with no
//! terminating chunk. The events all arrived and the turn is fully assembled —
//! and if the reader is still waiting for the connection to close, that trailing
//! failure throws the whole answer away.
//!
//! Reproduced here rather than reasoned about, because the only way to know a
//! reader stops at the right moment is to end the connection at the wrong one.

use std::io::{Read, Write};
use std::net::TcpListener;

use elmee_core::message::Message;
use elmee_plugin_provider::registry::{Api, ProviderEntry};
use elmee_plugin_provider::{MultiProvider, Provider};

/// Serve one streaming response, then cut the connection without ending the
/// chunked body — which is what reqwest reports as "error decoding response
/// body", the error in the report this test exists for.
fn stub_that_ends_badly(events: &'static [&'static str], truncate: bool) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a port");
    let address = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        let Ok((mut socket, _)) = listener.accept() else { return };
        let mut request = [0_u8; 8192];
        let _ = socket.read(&mut request);
        let _ = socket.write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n",
        );
        for event in events {
            let _ = write!(socket, "{:x}\r\n{event}\r\n", event.len());
        }
        if !truncate {
            let _ = socket.write_all(b"0\r\n\r\n");
        }
        let _ = socket.flush();
        // Dropping the socket here is the whole point: no terminating chunk,
        // so the body is incomplete however complete the stream was.
    });
    address
}

fn provider(address: &str, api: Api) -> MultiProvider {
    MultiProvider::new(
        vec![ProviderEntry {
            name: "stub".into(),
            api,
            base_url: format!("http://{address}/v1"),
            api_key: Some("stub".into()),
            subscription: None,
            declared: vec!["m".into()],
            client_version: None,
            discover: None,
        }],
        0,
        "m".into(),
    )
}

/// The stream said everything it had to say. What the connection did afterwards
/// is not the model's answer, and losing the answer to it is the failure this
/// closes: the terminal event ends the read, so there is no afterwards to fail
/// in.
#[tokio::test]
async fn a_finished_stream_survives_a_connection_that_ends_badly() {
    for (api, events) in [
        (
            Api::OpenAiCompletions,
            &["data: {\"choices\":[{\"delta\":{\"content\":\"the whole answer\"}}]}\n\n", "data: [DONE]\n\n"]
                [..],
        ),
        (
            Api::OpenAiResponses,
            &[
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"the whole answer\"}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":7,\"output_tokens\":3}}}\n\n",
            ][..],
        ),
    ] {
        let address = stub_that_ends_badly(events, true);
        let turn = provider(&address, api)
            .complete(&[Message::user("hello")], &[], &mut |_| {})
            .await
            .unwrap_or_else(|error| panic!("{api:?}: a complete answer was thrown away: {error}"));
        assert_eq!(
            turn.content.as_deref(),
            Some("the whole answer"),
            "{api:?}: and it must be the whole answer"
        );
    }
}

/// The other half of the same rule: a stream cut *before* it finished is a
/// failure, and must stay one. Half a turn is worse than none — a tool call
/// truncated mid-argument is a call nobody meant to make.
#[tokio::test]
async fn a_stream_cut_before_it_finished_is_still_a_failure() {
    let address =
        stub_that_ends_badly(&["data: {\"choices\":[{\"delta\":{\"content\":\"half an ans\"}}]}\n\n"], true);
    let result = provider(&address, Api::OpenAiCompletions)
        .complete(&[Message::user("hello")], &[], &mut |_| {})
        .await;
    assert!(result.is_err(), "an unfinished stream must not pass for an answer");
}

/// A valid HTTP ending is not a protocol terminal event. Proxies can close a
/// perfectly framed body after only part of an answer, and that must fail just
/// like a broken socket does.
#[tokio::test]
async fn clean_eof_before_done_is_not_a_complete_turn() {
    let address =
        stub_that_ends_badly(&["data: {\"choices\":[{\"delta\":{\"content\":\"half\"}}]}\n\n"], false);
    let result = provider(&address, Api::OpenAiCompletions)
        .complete(&[Message::user("hello")], &[], &mut |_| {})
        .await;
    assert!(result.is_err(), "clean HTTP EOF without [DONE] must remain incomplete");
}

/// An in-stream error event fails the turn rather than passing for an empty
/// success. A quota refusal is retried first — exhaustion is transient — so the
/// stub answers every attempt; when each one says the same thing, what the
/// provider said is what reaches the caller.
#[tokio::test]
async fn an_error_event_is_not_an_empty_success() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a port");
    let address = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        for mut socket in listener.incoming().flatten() {
            let mut request = [0_u8; 8192];
            let _ = socket.read(&mut request);
            let body = "data: {\"error\":{\"message\":\"quota exhausted\"}}\n\ndata: [DONE]\n\n";
            let _ = write!(
                socket,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
        }
    });
    let error = provider(&address, Api::OpenAiCompletions)
        .complete(&[Message::user("hello")], &[], &mut |_| {})
        .await
        .expect_err("an in-stream provider error must fail the turn");
    assert!(error.to_string().contains("quota exhausted"), "{error}");
}

/// Nothing was emitted, so there is nothing to duplicate: a transport that
/// broke before the model said a word is worth trying again, and used not to
/// be — only a connection that never opened or a read that timed out were.
#[tokio::test]
async fn a_transport_that_broke_before_a_word_is_tried_again() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a port");
    let address = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        for (attempt, stream) in listener.incoming().flatten().enumerate() {
            let mut socket = stream;
            let mut request = [0_u8; 8192];
            let _ = socket.read(&mut request);
            if attempt == 0 {
                // Headers, then the socket goes: a body that never arrives.
                let _ = socket.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n",
                );
                continue;
            }
            let body =
                "data: {\"choices\":[{\"delta\":{\"content\":\"second time lucky\"}}]}\n\ndata: [DONE]\n\n";
            let _ = write!(
                socket,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
        }
    });

    let turn = provider(&address, Api::OpenAiCompletions)
        .complete(&[Message::user("hello")], &[], &mut |_| {})
        .await
        .expect("a broken first attempt should not end the turn");
    assert_eq!(turn.content.as_deref(), Some("second time lucky"));
}
