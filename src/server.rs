use crate::{app, live};
use axum::extract::State;
use axum::http::{header, Method, StatusCode, Uri};
use axum::response::{
    sse::{Event, KeepAlive, Sse},
    IntoResponse, Response,
};
use axum::{routing::get, Router};
use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{watch, OwnedSemaphorePermit, Semaphore};

#[derive(Clone)]
struct Server {
    settings: Arc<app::Settings>,
    live: Arc<live::Live>,
    renderers: Arc<Semaphore>,
    shutdown: watch::Sender<bool>,
}

pub async fn run(settings: app::Settings) -> Result<(), String> {
    let listener = TcpListener::bind(&settings.listen)
        .await
        .map_err(|error| format!("HTTP listener: {error}"))?;
    let live = live::Live::start(&settings).await?;
    eprintln!(
        "runboard: {} projects on http://{}; live state in {}",
        settings.projects.len(),
        settings.listen,
        live.state_dir.display()
    );
    let settings = Arc::new(settings);
    let consumer_live = live.clone();
    let consumer = tokio::spawn(async move {
        loop {
            if let Err(error) = consumer_live.clone().consume().await {
                consumer_live
                    .consumer_ready
                    .store(false, std::sync::atomic::Ordering::Relaxed);
                eprintln!("Live: consumer reconnecting: {error}");
                consumer_live.status.send_replace("reconnecting");
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    });
    let bridge = tokio::spawn(live.clone().bridge(settings.clone()));
    let (shutdown, mut shutdown_receiver) = watch::channel(false);
    let state = Server {
        settings,
        live: live.clone(),
        renderers: Arc::new(Semaphore::new(4)),
        shutdown: shutdown.clone(),
    };
    let application = Router::new()
        .route("/events", get(events))
        .route("/healthz", get(health))
        .fallback(snapshot)
        .layer(axum::middleware::from_fn(headers))
        .with_state(state);
    let listener = LimitedListener {
        listener,
        connections: Arc::new(Semaphore::new(128)),
    };
    let http = axum::serve(listener, application).with_graceful_shutdown(async move {
        let _ = shutdown_receiver.changed().await;
    });
    let mut http = Box::pin(std::future::IntoFuture::into_future(http));
    let signal = shutdown_signal();
    tokio::pin!(signal);
    let mut sidecar_check = tokio::time::interval(Duration::from_secs(2));
    let result = loop {
        tokio::select! {
            result = &mut http => break result.map_err(|error| error.to_string()),
            _ = &mut signal => break Ok(()),
            _ = sidecar_check.tick() => {
                if !live.sidecar_alive().await { break Err("NATS sidecar exited".into()); }
                if bridge.is_finished() { break Err("Live input worker stopped".into()); }
            }
        }
    };
    shutdown.send_replace(true);
    let _ = tokio::time::timeout(Duration::from_secs(5), &mut http).await;
    bridge.abort();
    consumer.abort();
    let _ = bridge.await;
    let _ = consumer.await;
    drop(http);
    live.shutdown().await;
    result
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! { _ = terminate.recv() => {}, _ = tokio::signal::ctrl_c() => {} }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

async fn health(State(state): State<Server>) -> Response {
    let status = state.live.connection_status();
    (
        if status == "live" {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        },
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::json!({"status":status}).to_string(),
    )
        .into_response()
}

async fn snapshot(State(state): State<Server>, uri: Uri, method: Method) -> Response {
    if method != Method::GET {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    if uri.to_string().len() > 8192 {
        return StatusCode::URI_TOO_LONG.into_response();
    }
    let permit = match state.renderers.try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    let target = uri.to_string();
    let response = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        app::route(&state.settings, &target)
    });
    match tokio::time::timeout(Duration::from_secs(10), response).await {
        Ok(Ok(response)) => {
            let mut result = (
                StatusCode::from_u16(response.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                [(header::CONTENT_TYPE, response.content_type)],
                response.body,
            )
                .into_response();
            if let Some(filename) = response.attachment {
                if let Ok(value) = format!("attachment; filename=\"{filename}\"").parse() {
                    result.headers_mut().insert(header::CONTENT_DISPOSITION, value);
                }
            }
            result
        }
        Ok(Err(_)) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        Err(_) => StatusCode::GATEWAY_TIMEOUT.into_response(),
    }
}

async fn events(State(state): State<Server>) -> Response {
    let permit = match state.live.clients.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    let mut receiver = state.live.events.subscribe();
    let mut shutdown = state.shutdown.subscribe();
    let stream = async_stream::stream! {
        let _permit = permit;
        yield Ok::<_,Infallible>(Event::default().event("resync").data("snapshot"));
        let mut timer = tokio::time::interval(Duration::from_secs(5));
        let deadline = tokio::time::sleep(Duration::from_secs(300));
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                message = receiver.recv() => match message {
                    Ok(event) => yield Ok(Event::default().event("update").id(event.sequence.to_string()).data(event.data.as_ref())),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => yield Ok(Event::default().event("resync").data("snapshot")),
                    Err(_) => break,
                },
                _ = timer.tick() => yield Ok(Event::default().event("status").data(state.live.connection_status())),
                _ = shutdown.changed() => break,
                _ = &mut deadline => break,
            }
        }
    };
    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(10)))
        .into_response()
}

async fn headers(request: axum::extract::Request, next: axum::middleware::Next) -> Response {
    if request.method() != Method::GET {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let header_bytes = request
        .headers()
        .iter()
        .map(|(name, value)| name.as_str().len() + value.len() + 4)
        .sum::<usize>();
    if header_bytes + request.uri().to_string().len() > 8192 {
        return StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE.into_response();
    }
    let mut response = next.run(request).await;
    for (name,value) in [
        ("cache-control","no-store"),("x-content-type-options","nosniff"),("referrer-policy","no-referrer"),
        ("content-security-policy","default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src 'self'; connect-src 'self'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'"),
    ] { response.headers_mut().insert(axum::http::HeaderName::from_static(name),axum::http::HeaderValue::from_static(value)); }
    response
}

struct LimitedListener {
    listener: TcpListener,
    connections: Arc<Semaphore>,
}
impl axum::serve::Listener for LimitedListener {
    type Io = Connection;
    type Addr = SocketAddr;
    async fn accept(&mut self) -> (Connection, SocketAddr) {
        loop {
            match self.listener.accept().await {
                Ok((socket, address)) => {
                    if let Ok(permit) = self.connections.clone().try_acquire_owned() {
                        return (
                            Connection {
                                socket,
                                _permit: permit,
                                deadline: Box::pin(tokio::time::sleep(Duration::from_secs(15))),
                            },
                            address,
                        );
                    }
                }
                Err(error) => eprintln!("HTTP: accept failed: {error}"),
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }
}
struct Connection {
    socket: TcpStream,
    _permit: OwnedSemaphorePermit,
    deadline: Pin<Box<tokio::time::Sleep>>,
}
impl AsyncRead for Connection {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if self.deadline.as_mut().poll(context).is_ready() {
            return Poll::Ready(Err(std::io::ErrorKind::TimedOut.into()));
        }
        let length = buffer.filled().len();
        let result = Pin::new(&mut self.socket).poll_read(context, buffer);
        if buffer.filled().len() > length {
            self.deadline
                .as_mut()
                .reset(tokio::time::Instant::now() + Duration::from_secs(15));
        }
        result
    }
}
impl AsyncWrite for Connection {
    fn poll_write(mut self: Pin<&mut Self>, context: &mut Context<'_>, buffer: &[u8]) -> Poll<std::io::Result<usize>> {
        if self.deadline.as_mut().poll(context).is_ready() {
            return Poll::Ready(Err(std::io::ErrorKind::TimedOut.into()));
        }
        let result = Pin::new(&mut self.socket).poll_write(context, buffer);
        if matches!(result,Poll::Ready(Ok(written)) if written>0) {
            self.deadline
                .as_mut()
                .reset(tokio::time::Instant::now() + Duration::from_secs(15));
        }
        result
    }
    fn poll_flush(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.socket).poll_flush(context)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.socket).poll_shutdown(context)
    }
}
