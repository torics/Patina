//! `patina export` and `patina version --check` from the real binary.
mod common;
use common::*;
use std::process::Stdio;

async fn patina_cmd(args: &[&str], env: &[(&str, &str)]) -> (i32, String, String) {
    let out = tokio::process::Command::new(env!("CARGO_BIN_EXE_patina"))
        .args(args)
        .envs(env.iter().copied())
        .stdin(Stdio::null())
        .output()
        .await
        .unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

#[tokio::test]
async fn a_run_is_exported_as_markdown_or_json_by_the_end_of_its_id() {
    let port = fake_model(vec![says("All done here.")]).await;
    let s = setup(port);
    without_sandbox(&s);
    let ran = run_json(&s, &[], &["Say something"]).await;
    assert_eq!(ran.code, 0, "{}", ran.stderr);
    let id = ran.of("run_started")[0]["run_id"].as_str().unwrap().to_string();
    let dirs = ["--config-dir", s.config.to_str().unwrap(), "--data-dir", s.data.to_str().unwrap()];
    let short = &id[id.len() - 8..];

    let (code, md, err) = patina_cmd(&[&["export", short][..], &dirs].concat(), &[]).await;
    assert_eq!(code, 0, "{err}");
    assert!(md.contains("Say something") && md.contains("All done here."), "{md}");

    let file = s.tmp.path().join("run.json");
    let (code, _, err) = patina_cmd(
        &[&["export", &id, "--format", "json", "--output", file.to_str().unwrap()][..], &dirs].concat(),
        &[],
    )
    .await;
    assert_eq!(code, 0, "{err}");
    let json: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(json["run"]["id"], id.as_str());
    assert!(json["events"].as_array().unwrap().len() > 3);

    let (code, _, err) = patina_cmd(&[&["export", "zzzzzzzz"][..], &dirs].concat(), &[]).await;
    assert!(code != 0 && err.contains("no run"), "{err}");
}

#[tokio::test]
async fn version_check_compares_with_the_latest_release() {
    let (code, out, _) = patina_cmd(&["version"], &[]).await;
    assert_eq!(code, 0);
    assert!(out.starts_with(&format!("patina {}\ncommit ", env!("CARGO_PKG_VERSION"))), "{out}");

    let feed = |tag: &'static str| async move {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let body = format!("{{\"tag_name\": \"{tag}\", \"html_url\": \"https://example.invalid/r/{tag}\"}}");
                let reply = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(reply.as_bytes()).await;
            }
        });
        format!("http://127.0.0.1:{port}/releases/latest")
    };
    let newer = feed("v99.0.0").await;
    let (code, out, err) = patina_cmd(&["version", "--check"], &[("PATINA_RELEASE_URL", &newer)]).await;
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("A newer release is available: v99.0.0"), "{out}");
    let same = feed(concat!("v", env!("CARGO_PKG_VERSION"))).await;
    let (_, out, _) = patina_cmd(&["version", "--check"], &[("PATINA_RELEASE_URL", &same)]).await;
    assert!(out.contains("This is the latest release"), "{out}");
    let (code, _, err) = patina_cmd(&["version", "--check"], &[("PATINA_RELEASE_URL", "http://127.0.0.1:9/none")]).await;
    assert!(code == 1 && err.contains("could not check"), "{err}");
}
