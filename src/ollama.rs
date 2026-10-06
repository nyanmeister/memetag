//! The ollama client shared by OCR and action tags: is the server there, does it hold the model, one completion.
//! Callers retain their model selection, output budget and handling of failed work.
use base64::Engine;

/// The agent for completions: a vision model on a large image can take minutes on a busy card.
pub(crate) fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(300))
        .build()
}

/// Is the server there? (systemd Wants= starts it, but say something useful if not.)
pub(crate) fn reachable(url: &str) -> Result<(), String> {
    ureq::get(&format!("{url}/api/version"))
        .timeout(std::time::Duration::from_secs(5))
        .call()
        .map(drop)
        .map_err(|e| format!("ollama not reachable at {url}: {e} (systemctl --user start ollama)"))
}

/// Whether ollama at `url` has `model` pulled. An exact tag match is required; a config value with no `:tag` matches any
/// tag of that base (so `qwen3-vl` finds `qwen3-vl:latest`), but a specific tag never matches a different one.
pub(crate) fn has_model(url: &str, model: &str) -> Result<bool, String> {
    let tags: serde_json::Value = ureq::get(&format!("{url}/api/tags"))
        .timeout(std::time::Duration::from_secs(5))
        .call()
        .map_err(|e| format!("ollama at {url} did not list its models: {e}"))?
        .into_json()
        .map_err(|e| format!("ollama /api/tags response: {e}"))?;
    Ok(tags["models"].as_array().is_some_and(|ms| {
        ms.iter().any(|m| {
            m["name"].as_str().is_some_and(|n| {
                n == model || (!model.contains(':') && n.split(':').next() == Some(model))
            })
        })
    }))
}

/// One completion; `image` is None for a text-only model. `num_predict` is the caller's output budget.
pub(crate) fn generate(
    agent: &ureq::Agent,
    url: &str,
    model: &str,
    prompt: &str,
    image: Option<&[u8]>,
    num_predict: u32,
) -> Result<String, String> {
    let mut body = serde_json::json!({
        "model": model, "prompt": prompt, "stream": false,
        "options": { "temperature": 0, "num_predict": num_predict, "num_ctx": 8192 },
        "keep_alive": "30m"
    });
    // num_ctx: ollama defaults to 4096, and qwen3-vl's image tokens alone reach ~4150 for near-square images at the
    // processor's cap (measured 2026-09-13: HTTP 400 "request (4117 tokens) exceeds the available context size
    // (4096)"). 8192 costs ~1.2 GB of KV cache on the 16 GB card and leaves room for the prompt and the output budget.
    if let Some(bytes) = image {
        body["images"] =
            serde_json::json!([base64::engine::general_purpose::STANDARD.encode(bytes)]);
    }
    let response = agent
        .post(url)
        .send_json(body)
        .map_err(|e| format!("ollama: {e}"))?;
    let json: serde_json::Value = response
        .into_json()
        .map_err(|e| format!("ollama json: {e}"))?;
    Ok(crate::ocr::clean_model(
        json.get("response").and_then(|v| v.as_str()).unwrap_or(""),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{BufRead, BufReader, Read, Write},
        net::TcpListener,
    };

    #[test]
    fn vision_and_text_requests_preserve_budgets_and_clean_responses() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/api/generate", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            for (budget, vision, status, response) in [
                (
                    700,
                    true,
                    "200 OK",
                    r#"{"response":"<b>A caption</b>\nA caption"}"#,
                ),
                (1200, false, "200 OK", r#"{"response":"none"}"#),
                (
                    700,
                    true,
                    "503 Service Unavailable",
                    r#"{"error":"unavailable"}"#,
                ),
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .unwrap();
                let mut reader = BufReader::new(&mut stream);
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                assert_eq!(line, "POST /api/generate HTTP/1.1\r\n");
                let mut length = None;
                loop {
                    line.clear();
                    assert_ne!(reader.read_line(&mut line).unwrap(), 0);
                    if line == "\r\n" {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':') {
                        if name.eq_ignore_ascii_case("content-length") {
                            length = Some(value.trim().parse::<usize>().unwrap());
                        }
                    }
                }
                let mut bytes = vec![0; length.unwrap()];
                reader.read_exact(&mut bytes).unwrap();
                let request: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(request["model"], "fixture-model");
                assert_eq!(request["prompt"], "Read 猫");
                assert_eq!(request["stream"], false);
                assert_eq!(request["keep_alive"], "30m");
                assert_eq!(
                    request["options"],
                    serde_json::json!({"temperature":0,"num_predict":budget,"num_ctx":8192})
                );
                if vision {
                    assert_eq!(request["images"], serde_json::json!(["AAH/"]));
                } else {
                    assert!(request.get("images").is_none());
                }
                write!(stream, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).unwrap();
            }
        });
        let agent = ureq::AgentBuilder::new()
            .timeout(std::time::Duration::from_secs(5))
            .build();
        assert_eq!(
            generate(
                &agent,
                &url,
                "fixture-model",
                "Read 猫",
                Some(&[0, 1, 255]),
                700
            )
            .unwrap(),
            "A caption"
        );
        assert_eq!(
            generate(&agent, &url, "fixture-model", "Read 猫", None, 1200).unwrap(),
            ""
        );
        assert!(generate(
            &agent,
            &url,
            "fixture-model",
            "Read 猫",
            Some(&[0, 1, 255]),
            700
        )
        .unwrap_err()
        .starts_with("ollama: "));
        server.join().unwrap();
    }
}
