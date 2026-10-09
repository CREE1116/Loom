//! Interruptible model HTTP. Dropping the future closes idle inference sockets;
//! no detached HTTP worker continues consuming a local model after cancellation.
use super::*;
use std::future::Future;

pub(super) fn run<T>(request: &Request, future: impl Future<Output = Result<T, ProviderError>>) -> Result<T, ProviderError> {
    request.check_cancelled()?;
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()
        .map_err(|error| ProviderError::new(ErrorKind::Transient, error.to_string()))?;
    let result = runtime.block_on(async {
        tokio::select! {
            biased;
            error = cancelled(request) => Err(error),
            result = future => result,
        }
    });
    // DNS resolution may use a blocking OS worker; it must not delay Esc either.
    runtime.shutdown_background();
    request.check_cancelled()?;
    result
}

async fn cancelled(request: &Request) -> ProviderError {
    if request.cancel.is_none() { std::future::pending::<()>().await; }
    loop {
        if let Err(error) = request.check_cancelled() { return error; }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

pub(super) fn error(error: reqwest::Error) -> ProviderError {
    ProviderError::new(ErrorKind::Transient, if error.is_timeout() {
        "model request timed out".into()
    } else {
        error.without_url().to_string()
    })
}

pub(super) async fn send(request: &Request, endpoint: &str, headers: &[(String, String)], body: &Value) -> Result<reqwest::Response, ProviderError> {
    let client = reqwest::Client::builder()
        .timeout(request.timeout.unwrap_or(DEFAULT_TIMEOUT))
        .redirect(reqwest::redirect::Policy::limited(5))
        .build().map_err(error)?;
    let mut outgoing = client.post(endpoint).json(body);
    for (name, value) in headers { outgoing = outgoing.header(name, value); }
    let response = outgoing.send().await.map_err(error)?;
    let status = response.status().as_u16();
    if status >= 400 {
        let retry_after = response.headers().get("retry-after").and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse().ok()).map(Duration::from_secs);
        let body = response.text().await.map_err(error)?;
        let code = serde_json::from_str::<Value>(&body).ok()
            .and_then(|value| value["error"]["code"].as_str().map(str::to_owned));
        return Err(ProviderError { kind: status_kind(status, &body),
            message: format!("HTTP {status}: {}", body.chars().take(500).collect::<String>()), retry_after, code });
    }
    Ok(response)
}

pub(super) fn json(request: &Request, endpoint: &str, headers: &[(String, String)], body: &Value) -> Result<(Value, Duration), ProviderError> {
    run(request, async {
        let started = Instant::now();
        let response = send(request, endpoint, headers, body).await?;
        let value = response.json::<Value>().await.map_err(error)?;
        Ok((value, started.elapsed()))
    })
}
