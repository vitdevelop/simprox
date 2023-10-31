use std::io::ErrorKind;
use std::net::SocketAddr;

use http_body_util::{combinators::BoxBody, BodyExt, Empty, Full};
use hyper::client::conn::http1::Builder;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::upgrade::Upgraded;
use hyper::{http, Method, Request, Response};
use hyper::body::Bytes;
use hyper_util::rt::TokioIo;

use tokio::net::{TcpListener, TcpStream};
use log::{error, info, trace};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

// To try this example:
// 1. cargo run
// 2. config http_proxy in command line
//    $ export http_proxy=http://127.0.0.1:<port>
//    $ export https_proxy=http://127.0.0.1:<port>
// 3. send requests
//    $ curl -i https://www.example.com/
#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();
    let addr = SocketAddr::from(([127, 0, 0, 1], 8100));

    let listener = TcpListener::bind(addr).await?;
    info!("Listening on http://{}", addr);

    loop {
        let (stream, _) = listener.accept().await?;
        let io = TokioIo::new(stream);

        tokio::task::spawn(async move {
            if let Err(err) = http1::Builder::new()
                .preserve_header_case(true)
                .title_case_headers(true)
                .serve_connection(io, service_fn(proxy))
                .with_upgrades()
                .await
            {
                error!("Failed to serve connection: {:?}", err);
            }
        });
    }
}

async fn proxy(
    req: Request<hyper::body::Incoming>,
) -> Result<Response<BoxBody<Bytes, hyper::Error>>> {
    trace!("req: {:?}", req);

    if Method::CONNECT == req.method() {
        // Received an HTTP request like:
        // ```
        // CONNECT www.domain.com:443 HTTP/1.1
        // Host: www.domain.com:443
        // Proxy-Connection: Keep-Alive
        // ```
        //
        // When HTTP method is CONNECT we should return an empty body
        // then we can eventually upgrade the connection and talk a new protocol.
        //
        // Note: only after client received an empty body with STATUS_OK can the
        // connection be upgraded, so we can't return a response inside
        // `on_upgrade` future.
        if let Some(addr) = host_addr(req.uri()) {
            tokio::task::spawn(async move {
                match hyper::upgrade::on(req).await {
                    Ok(upgraded) => {
                        if let Err(e) = tunnel(upgraded, addr).await {
                            match e.kind() {
                                ErrorKind::NotConnected => {
                                    trace!("Connection reset by peer")
                                }
                                _ => {
                                    error!("server io error: {}", e);
                                }
                            }
                        };
                    }
                    Err(e) => error!("upgrade error: {}", e),
                }
            });

            Ok(Response::new(empty()))
        } else {
            trace!("CONNECT host is not socket addr: {:?}", req.uri());
            let mut resp = Response::new(full("CONNECT must be to a socket address"));
            *resp.status_mut() = http::StatusCode::BAD_REQUEST;

            Ok(resp)
        }
    } else {
        let port = match req.uri().scheme() {
            Some(scheme) => {
                if scheme.eq("https") { 443 } else if scheme.eq("http") { 80 } else { 80 }
            }
            _ => 80
        };
        let host = req.uri().host().ok_or("uri has no host")?;
        let port = req.uri().port_u16().unwrap_or(port);
        let addr = format!("{}:{}", host, port);

        let stream = TcpStream::connect(addr).await.unwrap();
        let io = TokioIo::new(stream);

        let (mut sender, conn) = Builder::new()
            .preserve_header_case(true)
            .title_case_headers(true)
            .handshake(io)
            .await?;
        tokio::task::spawn(async move {
            if let Err(err) = conn.await {
                error!("Connection failed: {:?}", err);
            }
        });

        let resp = sender.send_request(req).await?;
        Ok(resp.map(|b| b.boxed()))
    }
}

fn host_addr(uri: &http::Uri) -> Option<String> {
    uri.authority().and_then(|auth| Some(auth.to_string()))
}

fn empty() -> BoxBody<Bytes, hyper::Error> {
    Empty::<Bytes>::new()
        .map_err(|never| match never {})
        .boxed()
}

fn full<T: Into<Bytes>>(chunk: T) -> BoxBody<Bytes, hyper::Error> {
    Full::new(chunk.into())
        .map_err(|never| match never {})
        .boxed()
}

// Create a TCP connection to host:port, build a tunnel between the connection and
// the upgraded connection
async fn tunnel(upgraded: Upgraded, addr: String) -> std::io::Result<()> {
    // Connect to remote server
    let mut server = TcpStream::connect(addr).await?;
    let mut upgraded = TokioIo::new(upgraded);

    // Proxying data
    tokio::io::copy_bidirectional(&mut upgraded, &mut server).await?;

    Ok(())
}
