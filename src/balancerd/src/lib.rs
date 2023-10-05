// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

// BEGIN LINT CONFIG
// DO NOT EDIT. Automatically generated by bin/gen-lints.
// Have complaints about the noise? See the note in misc/python/materialize/cli/gen-lints.py first.
#![allow(unknown_lints)]
#![allow(clippy::style)]
#![allow(clippy::complexity)]
#![allow(clippy::large_enum_variant)]
#![allow(clippy::mutable_key_type)]
#![allow(clippy::stable_sort_primitive)]
#![allow(clippy::map_entry)]
#![allow(clippy::box_default)]
#![allow(clippy::drain_collect)]
#![warn(clippy::bool_comparison)]
#![warn(clippy::clone_on_ref_ptr)]
#![warn(clippy::no_effect)]
#![warn(clippy::unnecessary_unwrap)]
#![warn(clippy::dbg_macro)]
#![warn(clippy::todo)]
#![warn(clippy::wildcard_dependencies)]
#![warn(clippy::zero_prefixed_literal)]
#![warn(clippy::borrowed_box)]
#![warn(clippy::deref_addrof)]
#![warn(clippy::double_must_use)]
#![warn(clippy::double_parens)]
#![warn(clippy::extra_unused_lifetimes)]
#![warn(clippy::needless_borrow)]
#![warn(clippy::needless_question_mark)]
#![warn(clippy::needless_return)]
#![warn(clippy::redundant_pattern)]
#![warn(clippy::redundant_slicing)]
#![warn(clippy::redundant_static_lifetimes)]
#![warn(clippy::single_component_path_imports)]
#![warn(clippy::unnecessary_cast)]
#![warn(clippy::useless_asref)]
#![warn(clippy::useless_conversion)]
#![warn(clippy::builtin_type_shadow)]
#![warn(clippy::duplicate_underscore_argument)]
#![warn(clippy::double_neg)]
#![warn(clippy::unnecessary_mut_passed)]
#![warn(clippy::wildcard_in_or_patterns)]
#![warn(clippy::crosspointer_transmute)]
#![warn(clippy::excessive_precision)]
#![warn(clippy::overflow_check_conditional)]
#![warn(clippy::as_conversions)]
#![warn(clippy::match_overlapping_arm)]
#![warn(clippy::zero_divided_by_zero)]
#![warn(clippy::must_use_unit)]
#![warn(clippy::suspicious_assignment_formatting)]
#![warn(clippy::suspicious_else_formatting)]
#![warn(clippy::suspicious_unary_op_formatting)]
#![warn(clippy::mut_mutex_lock)]
#![warn(clippy::print_literal)]
#![warn(clippy::same_item_push)]
#![warn(clippy::useless_format)]
#![warn(clippy::write_literal)]
#![warn(clippy::redundant_closure)]
#![warn(clippy::redundant_closure_call)]
#![warn(clippy::unnecessary_lazy_evaluations)]
#![warn(clippy::partialeq_ne_impl)]
#![warn(clippy::redundant_field_names)]
#![warn(clippy::transmutes_expressible_as_ptr_casts)]
#![warn(clippy::unused_async)]
#![warn(clippy::disallowed_methods)]
#![warn(clippy::disallowed_macros)]
#![warn(clippy::disallowed_types)]
#![warn(clippy::from_over_into)]
// END LINT CONFIG

//! The balancerd service is a horizontally scalable, stateless, multi-tenant ingress router for
//! pgwire and HTTPS connections.
//!
//! It listens on pgwire and HTTPS ports. When a new pgwire connection starts, the requested user is
//! authenticated with frontegg from which a tenant id is returned. From that a target internal
//! hostname is resolved to an IP address, and the connection is proxied to that address which has a
//! running environmentd's pgwire port. When a new HTTPS connection starts, its SNI hostname is used
//! to generate an internal hostname that is resolved to an IP address, which is similarly proxied.

mod codec;

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use bytes::BytesMut;
use mz_build_info::{build_info, BuildInfo};
use mz_frontegg_auth::Authentication as FronteggAuthentication;
use mz_ore::metric;
use mz_ore::metrics::{ComputedGauge, MetricsRegistry};
use mz_ore::netio::AsyncReady;
use mz_ore::server::{listen, TlsCertConfig, TlsConfig, TlsMode};
use mz_ore::task::JoinSetExt;
use mz_pgwire_common::{
    decode_startup, Conn, ErrorResponse, FrontendMessage, FrontendStartupMessage,
    ACCEPT_SSL_ENCRYPTION, REJECT_ENCRYPTION, VERSION_3,
};
use openssl::ssl::Ssl;
use semver::Version;
use tokio::io::{self, AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::task::JoinSet;
use tokio_openssl::SslStream;
use tokio_postgres::error::SqlState;
use tracing::{error, warn};

use crate::codec::{BackendMessage, FramedConn};

/// Balancer build information.
pub const BUILD_INFO: BuildInfo = build_info!();

pub struct BalancerConfig {
    /// Info about which version of the code is running.
    build_version: Version,
    /// Listen address for pgwire connections.
    pgwire_listen_addr: SocketAddr,
    /// Listen address for HTTP connections.
    http_listen_addr: SocketAddr,
    /// DNS resolver.
    resolver: Resolver,
    tls: Option<TlsCertConfig>,
}

impl BalancerConfig {
    pub fn new(
        build_info: &BuildInfo,
        pgwire_listen_addr: SocketAddr,
        http_listen_addr: SocketAddr,
        resolver: Resolver,
        tls: Option<TlsCertConfig>,
    ) -> Self {
        Self {
            build_version: build_info.semver_version(),
            pgwire_listen_addr,
            http_listen_addr,
            resolver,
            tls,
        }
    }
}

/// Prometheus monitoring metrics.
#[derive(Debug)]
pub struct Metrics {
    _uptime: ComputedGauge,
}

impl Metrics {
    /// Returns a new [Metrics] instance connected to the given registry.
    pub fn new(cfg: &BalancerConfig, registry: &MetricsRegistry) -> Self {
        let start = Instant::now();
        let uptime = registry.register_computed_gauge(
            metric!(
                name: "mz_balancer_metadata_seconds",
                help: "server uptime, labels are build metadata",
                const_labels: {
                    "version" => cfg.build_version,
                    "build_type" => if cfg!(release) { "release" } else { "debug" }
                },
            ),
            move || start.elapsed().as_secs_f64(),
        );
        Metrics { _uptime: uptime }
    }
}

pub struct BalancerService {
    cfg: BalancerConfig,
    _metrics: Metrics,
}

impl BalancerService {
    pub fn new(cfg: BalancerConfig, metrics: Metrics) -> Self {
        Self {
            cfg,
            _metrics: metrics,
        }
    }

    pub async fn serve(self) -> Result<(), anyhow::Error> {
        let (_pgwire_listen_handle, pgwire_stream) = listen(self.cfg.pgwire_listen_addr).await?;
        let (_http_listen_handle, http_stream) = listen(self.cfg.http_listen_addr).await?;

        let pgwire_tls = match self.cfg.tls {
            Some(tls) => Some(TlsConfig {
                context: tls.context()?,
                mode: TlsMode::Require,
            }),
            None => None,
        };

        let resolver = Arc::new(self.cfg.resolver);

        let pgwire = PgwireBalancer {
            resolver: Arc::clone(&resolver),
            tls: pgwire_tls,
        };
        let http = HttpBalancer {
            _resolver: Arc::clone(&resolver),
        };

        let mut set = JoinSet::new();
        set.spawn_named(|| "pgwire_stream", async move {
            mz_ore::server::serve(pgwire_stream, pgwire).await;
        });
        set.spawn_named(|| "http_stream", async move {
            mz_ore::server::serve(http_stream, http).await;
        });
        // The tasks should never exit, so complain if they do.
        while let Some(res) = set.join_next().await {
            let _ = res?;
            error!("serving task unexpectedly exited");
        }
        anyhow::bail!("serving tasks unexpectedly exited");
    }
}

struct PgwireBalancer {
    tls: Option<TlsConfig>,
    resolver: Arc<Resolver>,
    // todo: metrics
}

impl PgwireBalancer {
    #[tracing::instrument(level = "debug", skip_all)]
    async fn run<'a, A>(
        conn: &'a mut FramedConn<A>,
        version: i32,
        params: BTreeMap<String, String>,
        resolver: &Resolver,
        tls_mode: Option<TlsMode>,
    ) -> Result<(), io::Error>
    where
        A: AsyncRead + AsyncWrite + AsyncReady + Send + Sync + Unpin,
    {
        if version != VERSION_3 {
            return conn
                .send(ErrorResponse::fatal(
                    SqlState::SQLSERVER_REJECTED_ESTABLISHMENT_OF_SQLCONNECTION,
                    "server does not support the client's requested protocol version",
                ))
                .await;
        }

        let Some(user) = params.get("user") else {
            return conn
                .send(ErrorResponse::fatal(
                    SqlState::SQLSERVER_REJECTED_ESTABLISHMENT_OF_SQLCONNECTION,
                    "user parameter required",
                ))
                .await;
        };

        if let Err(err) = conn.inner().ensure_tls_compatibility(&tls_mode) {
            return conn.send(err).await;
        }

        let resolved = match resolver.resolve(conn, user).await {
            Ok(v) => v,
            Err(err) => {
                return conn
                    .send(ErrorResponse::fatal(
                        SqlState::INVALID_PASSWORD,
                        err.to_string(),
                    ))
                    .await;
            }
        };

        if let Err(err) = Self::stream(conn, resolved.addr, resolved.password, params).await {
            return conn
                .send(ErrorResponse::fatal(
                    SqlState::INVALID_PASSWORD,
                    err.to_string(),
                ))
                .await;
        }

        Ok(())
    }

    #[tracing::instrument(level = "debug", skip_all)]
    async fn stream<'a, A>(
        conn: &'a mut FramedConn<A>,
        envd_addr: SocketAddr,
        password: Option<String>,
        params: BTreeMap<String, String>,
    ) -> Result<(), anyhow::Error>
    where
        A: AsyncRead + AsyncWrite + AsyncReady + Send + Sync + Unpin,
    {
        let client_stream = conn.inner_mut();
        let mut mz_stream = TcpStream::connect(envd_addr).await?;
        let mut buf = BytesMut::new();

        // Send initial startup and password messages.
        let startup = FrontendStartupMessage::Startup {
            version: VERSION_3,
            params,
        };
        startup.encode(&mut buf)?;
        if let Some(password) = password {
            let password = FrontendMessage::Password { password };
            password.encode(&mut buf)?;
        }
        mz_stream.write_all(&buf).await?;

        // Now blindly shuffle bytes back and forth until closed.
        // TODO: Limit total memory use.
        tokio::io::copy_bidirectional(client_stream, &mut mz_stream).await?;

        Ok(())
    }
}

impl mz_ore::server::Server for PgwireBalancer {
    const NAME: &'static str = "pgwire_balancer";

    fn handle_connection(&self, conn: TcpStream) -> mz_ore::server::ConnectionHandler {
        let tls = self.tls.clone();
        let resolver = Arc::clone(&self.resolver);
        Box::pin(async move {
            // TODO: Try to merge this with pgwire/server.rs to avoid the duplication. May not be
            // worth it.
            let _result: Result<(), anyhow::Error> = async move {
                let mut conn = Conn::Unencrypted(conn);
                loop {
                    let message = decode_startup(&mut conn).await?;

                    conn = match message {
                        // Clients sometimes hang up during the startup sequence, e.g.
                        // because they receive an unacceptable response to an
                        // `SslRequest`. This is considered a graceful termination.
                        None => return Ok(()),

                        Some(FrontendStartupMessage::Startup { version, params }) => {
                            let mut conn = FramedConn::new(conn);
                            Self::run(
                                &mut conn,
                                version,
                                params,
                                &resolver,
                                tls.map(|tls| tls.mode),
                            )
                            .await?;
                            // TODO: Resolver lookup then begin relaying bytes.
                            conn.flush().await?;
                            return Ok(());
                        }

                        Some(FrontendStartupMessage::CancelRequest { .. }) => {
                            // Balancer ignores cancel requests.
                            //
                            // TODO: Can/should we return some error here so users are informed
                            // this won't ever work?
                            return Ok(());
                        }

                        Some(FrontendStartupMessage::SslRequest) => match (conn, &tls) {
                            (Conn::Unencrypted(mut conn), Some(tls)) => {
                                conn.write_all(&[ACCEPT_SSL_ENCRYPTION]).await?;
                                let mut ssl_stream = SslStream::new(Ssl::new(&tls.context)?, conn)?;
                                if let Err(e) = Pin::new(&mut ssl_stream).accept().await {
                                    let _ = ssl_stream.get_mut().shutdown().await;
                                    return Err(e.into());
                                }
                                Conn::Ssl(ssl_stream)
                            }
                            (mut conn, _) => {
                                conn.write_all(&[REJECT_ENCRYPTION]).await?;
                                conn
                            }
                        },

                        Some(FrontendStartupMessage::GssEncRequest) => {
                            conn.write_all(&[REJECT_ENCRYPTION]).await?;
                            conn
                        }
                    }
                }
            }
            .await;
            // metrics.connection_status(result.is_ok()).inc();
            Ok(())
        })
    }
}

struct HttpBalancer {
    _resolver: Arc<Resolver>,
    // todo: metrics
}

impl mz_ore::server::Server for HttpBalancer {
    const NAME: &'static str = "http_balancer";

    fn handle_connection(&self, _conn: TcpStream) -> mz_ore::server::ConnectionHandler {
        Box::pin(async { Ok(()) })
    }
}

pub enum Resolver {
    Static(SocketAddr),
    Frontegg(FronteggResolver),
}

impl Resolver {
    async fn resolve<A>(
        &self,
        conn: &mut FramedConn<A>,
        user: &str,
    ) -> Result<ResolvedAddr, anyhow::Error>
    where
        A: AsyncRead + AsyncWrite + Unpin,
    {
        match self {
            Resolver::Frontegg(FronteggResolver {
                auth,
                addr_template,
            }) => {
                conn.send(BackendMessage::AuthenticationCleartextPassword)
                    .await?;
                conn.flush().await?;
                let password = match conn.recv().await? {
                    Some(FrontendMessage::Password { password }) => password,
                    _ => anyhow::bail!("expected Password message"),
                };
                match auth
                    .exchange_password_for_token(&password)
                    .await
                    .and_then(|response| {
                        let response = auth.validate_api_token_response(response, Some(user))?;
                        Ok(response.claims.tenant_id)
                    }) {
                    Ok(tenant_id) => {
                        let addr = addr_template.replace("{}", &tenant_id.to_string());
                        let mut addrs = tokio::net::lookup_host(&addr).await?;
                        let Some(addr) = addrs.next() else {
                            error!("{addr} did not resolve to any addresses");
                            anyhow::bail!("internal error");
                        };
                        Ok(ResolvedAddr {
                            addr,
                            password: Some(password),
                        })
                    }
                    Err(e) => {
                        warn!("pgwire connection failed authentication: {}", e);
                        // TODO: fix error codes.
                        anyhow::bail!("invalid password");
                    }
                }
            }
            Resolver::Static(addr) => Ok(ResolvedAddr {
                addr: addr.clone(),
                password: None,
            }),
        }
    }
}

pub struct FronteggResolver {
    pub auth: FronteggAuthentication,
    pub addr_template: String,
}

struct ResolvedAddr {
    addr: SocketAddr,
    password: Option<String>,
}