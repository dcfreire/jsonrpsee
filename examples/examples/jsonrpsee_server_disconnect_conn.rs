// Copyright 2019-2021 Parity Technologies (UK) Ltd.
//
// Permission is hereby granted, free of charge, to any
// person obtaining a copy of this software and associated
// documentation files (the "Software"), to deal in the
// Software without restriction, including without
// limitation the rights to use, copy, modify, merge,
// publish, distribute, sublicense, and/or sell copies of
// the Software, and to permit persons to whom the Software
// is furnished to do so, subject to the following
// conditions:
//
// The above copyright notice and this permission notice
// shall be included in all copies or substantial portions
// of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF
// ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED
// TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A
// PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT
// SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY
// CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION
// OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR
// IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
// DEALINGS IN THE SOFTWARE.

use std::collections::HashSet;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, AtomicUsize};
use std::sync::{Arc, Mutex};

use futures::FutureExt;
use jsonrpsee::core::{async_trait, client::ClientT};
use jsonrpsee::proc_macros::rpc;
use jsonrpsee::server::middleware::rpc::*;
use jsonrpsee::server::ws::{self, run_websocket};
use jsonrpsee::server::{http, ConnectionGuard, ServiceData, StopHandle};
use jsonrpsee::types::{ErrorObject, ErrorObjectOwned, Request};
use jsonrpsee::ws_client::WsClientBuilder;
use jsonrpsee::{rpc_params, MethodResponse};

use hyper::server::conn::AddrStream;
use tokio::sync::mpsc;
use tracing_subscriber::util::SubscriberInitExt;

struct DummyRateLimit<S> {
	service: S,
	count: Arc<AtomicUsize>,
	state: mpsc::Sender<()>,
}

#[async_trait]
impl<'a, S> RpcServiceT<'a> for DummyRateLimit<S>
where
	S: Send + Sync + RpcServiceT<'a>,
{
	async fn call(&self, req: Request<'a>, ctx: &Context) -> MethodResponse {
		let count = self.count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

		if count > 10 {
			let _ = self.state.try_send(());
			MethodResponse::error(req.id, ErrorObject::borrowed(-32000, "RPC rate limit", None))
		} else {
			self.service.call(req, ctx).await
		}
	}
}

#[rpc(server)]
pub trait Rpc {
	#[method(name = "say_hello")]
	async fn say_hello(&self) -> Result<String, ErrorObjectOwned>;
}

#[async_trait]
impl RpcServer for () {
	async fn say_hello(&self) -> Result<String, ErrorObjectOwned> {
		Ok("lo".to_string())
	}
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
	let filter = tracing_subscriber::EnvFilter::try_from_default_env()?
		.add_directive("jsonrpsee[method_call{name = \"say_hello\"}]=trace".parse()?);
	tracing_subscriber::FmtSubscriber::builder().with_env_filter(filter).finish().try_init()?;

	tokio::spawn(run_server());

	// Make a bunch of requests to be blacklisted by server.
	{
		let client = WsClientBuilder::default().build("ws://127.0.0.1:9944").await.unwrap();
		while client.is_connected() {
			let rp: Result<String, _> = client.request("say_hello", rpc_params!()).await;
			tracing::info!("response: {:?}", rp);
		}
	}

	// After the server has blacklisted the IP address, the connection is denied.
	assert!(WsClientBuilder::default().build("ws://127.0.0.1:9944").await.is_err());

	Ok(())
}

async fn run_server() {
	use hyper::service::{make_service_fn, service_fn};

	// Construct our SocketAddr to listen on...
	let addr = SocketAddr::from(([127, 0, 0, 1], 9944));

	// Maybe we want to be able to stop our server but not added here.
	let (_tx, rx) = tokio::sync::watch::channel(());

	let stop_handle = StopHandle::new(rx);

	let service_cfg = jsonrpsee::server::Server::builder().to_service(().into_rpc());
	let conn_guard = Arc::new(ConnectionGuard::new(service_cfg.settings.max_connections as usize));
	let conn_id = Arc::new(AtomicU32::new(0));

	// Blacklisted peers
	let blacklisted_peers = Arc::new(Mutex::new(HashSet::new()));

	// And a MakeService to handle each connection...
	let make_service = make_service_fn(|conn: &AddrStream| {
		// You may use `conn` or the actual HTTP request to deny a certain peer.

		// Connection state.
		let conn_id = conn_id.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
		let remote_addr = conn.remote_addr();
		let stop_handle = stop_handle.clone();
		let conn_guard = conn_guard.clone();
		let service_cfg = service_cfg.clone();
		let blacklisted_peers = blacklisted_peers.clone();

		async move {
			let stop_handle = stop_handle.clone();
			let conn_guard = conn_guard.clone();
			let service_cfg = service_cfg.clone();
			let stop_handle = stop_handle.clone();
			let blacklisted_peers = blacklisted_peers.clone();

			Ok::<_, Infallible>(service_fn(move |req| {
				// Connection number limit exceeded.
				let Some(conn_permit) = conn_guard.try_acquire() else {
					return async { Ok::<_, Infallible>(http::response::too_many_requests()) }.boxed();
				};

				// The IP addr was blacklisted.
				if blacklisted_peers.lock().unwrap().get(&remote_addr.ip()).is_some() {
					return async { Ok(http::response::denied()) }.boxed();
				}

				if ws::is_upgrade_request(&req) && service_cfg.settings.enable_ws {
					let service_cfg = service_cfg.clone();
					let stop_handle = stop_handle.clone();
					let blacklisted_peers = blacklisted_peers.clone();

					let (tx, mut disconnect) = mpsc::channel(1);
					let rpc_service = RpcServiceBuilder::new().layer_fn(move |service| DummyRateLimit {
						service,
						count: Arc::new(AtomicUsize::new(0)),
						state: tx.clone(),
					});

					let svc = ServiceData {
						cfg: service_cfg.settings,
						conn_id,
						remote_addr,
						stop_handle,
						conn_permit: Arc::new(conn_permit),
						methods: service_cfg.methods.clone(),
					};

					// Establishes the websocket connection
					// and if the `DummyRateLimit` middleware triggers the hard limit
					// then the connection is closed i.e, the `conn_fut` is dropped.
					async move {
						match run_websocket(req, svc, rpc_service).await {
							Ok((rp, conn_fut)) => {
								tokio::spawn(async move {
									tokio::select! {
										_ = conn_fut => (),
										_ = disconnect.recv() => {
											blacklisted_peers.lock().unwrap().insert(remote_addr.ip());
										},
									}
								});
								Ok(rp)
							}
							Err(rp) => Ok(rp),
						}
					}
					.boxed()
				} else {
					// TODO: for simplicity in this example the server doesn't support HTTP requests.
					async { Ok(http::response::denied()) }.boxed()
				}
			}))
		}
	});

	// Then bind and serve...
	let server = hyper::Server::bind(&addr).serve(make_service);

	server.await.unwrap();
}
