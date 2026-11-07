// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
// SPDX-License-Identifier: Apache-2.0

use std::io::{BufRead, Write};

use artidezi_contract::common::ArtifactRef;
use artidezi_worker::client::Client;
use sha2::Digest;

/// Serves `body` once, however many bytes the artifact claims to have.
fn serve(body: Vec<u8>) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        while reader.read_line(&mut line).unwrap() > 2 {
            line.clear();
        }
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(head.as_bytes()).unwrap();
        stream.write_all(&body).unwrap();
    });
    url
}

fn artifact(body: &[u8]) -> ArtifactRef {
    ArtifactRef {
        sha256: hex::encode(sha2::Sha256::digest(body)),
        size: body.len() as u64,
    }
}

#[test]
fn artifacts_of_any_size_download_intact() {
    let dir = tempfile::tempdir().unwrap();
    for size in [10, 64 * 1024, 64 * 1024 + 1, 12 * 1024 * 1024] {
        let body = vec![7; size];
        let dest = dir.path().join("artifact");
        Client::new(&serve(body.clone()), "token")
            .download("att", &artifact(&body), &dest)
            .unwrap_or_else(|error| panic!("{size} bytes: {error}"));
        assert_eq!(std::fs::read(&dest).unwrap(), body);
    }
}

#[test]
fn a_body_larger_than_the_artifact_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let expected = artifact(&vec![7; 100 * 1024]);
    let error = Client::new(&serve(vec![7; 300 * 1024]), "token")
        .download("att", &expected, &dir.path().join("artifact"))
        .unwrap_err();
    assert!(error.to_string().contains("download"));
}
