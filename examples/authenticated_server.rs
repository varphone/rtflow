mod common;

use clap::{Parser, ValueEnum};
use common::{CodecArg, advertised_url, description, init_tracing, mount_path, sample_unit};
use futures::future::BoxFuture;
use rtflow::{
    AccessRole, AuthDecision, Authenticator, Credentials, MountConfig, RtspServer, ServerConfig,
    auth::{AuthRequest, verify_basic, verify_digest},
};
use std::{net::SocketAddr, sync::Arc, time::Duration};

#[derive(Clone, Copy, Debug, ValueEnum)]
enum AuthMode {
    Basic,
    Digest,
}

struct RoleAuthenticator {
    read: Credentials,
    publish: Credentials,
    mode: AuthMode,
    realm: String,
}

impl Authenticator for RoleAuthenticator {
    fn authorize<'a>(&'a self, request: AuthRequest<'a>) -> BoxFuture<'a, AuthDecision> {
        Box::pin(async move {
            let credentials = match request.role {
                AccessRole::Read => &self.read,
                AccessRole::Publish => &self.publish,
            };
            let allowed = request.authorization.is_some_and(|header| match self.mode {
                AuthMode::Basic => verify_basic(header, credentials),
                AuthMode::Digest => {
                    verify_digest(header, request.method, credentials, &self.realm).unwrap_or(false)
                }
            });
            if allowed {
                AuthDecision::Allow
            } else {
                match self.mode {
                    AuthMode::Basic => AuthDecision::Challenge {
                        realm: self.realm.clone(),
                    },
                    AuthMode::Digest => AuthDecision::DigestChallenge {
                        realm: self.realm.clone(),
                        nonce: "example-static-nonce".into(),
                    },
                }
            }
        })
    }
}

/// Run read and publish mounts protected by role-specific credentials.
#[derive(Debug, Parser)]
struct Args {
    #[arg(long, default_value = "0.0.0.0:8554")]
    listen: SocketAddr,
    #[arg(long, value_enum, default_value_t = AuthMode::Digest)]
    mode: AuthMode,
    #[arg(long, default_value = "/secure")]
    live_mount: String,
    #[arg(long, default_value = "/secure-ingest")]
    ingest_mount: String,
    #[arg(long, default_value = "viewer")]
    read_user: String,
    #[arg(long, default_value = "viewer-secret")]
    read_password: String,
    #[arg(long, default_value = "publisher")]
    publish_user: String,
    #[arg(long, default_value = "publish-secret")]
    publish_password: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_tracing();
    let args = Args::parse();
    let media = description("authenticated", &[CodecArg::H264]);
    let mut config = ServerConfig::new(args.listen);
    config.authenticator = Arc::new(RoleAuthenticator {
        read: Credentials {
            username: args.read_user.clone(),
            password: args.read_password,
        },
        publish: Credentials {
            username: args.publish_user.clone(),
            password: args.publish_password,
        },
        mode: args.mode,
        realm: "rtsp-example".into(),
    });
    let server = RtspServer::bind(config).await?;
    let live = server
        .register_mount(
            mount_path(&args.live_mount)?,
            MountConfig::Local {
                description: media.clone(),
            },
        )
        .await?;
    server
        .register_mount(
            mount_path(&args.ingest_mount)?,
            MountConfig::Publisher {
                description: media,
                allow_replace_after_disconnect: true,
            },
        )
        .await?;
    let publisher = live.publisher()?;
    tracing::info!(
        url = %advertised_url(server.local_addr(), &args.live_mount),
        user = %args.read_user,
        "authenticated playback ready"
    );
    tracing::info!(
        url = %advertised_url(server.local_addr(), &args.ingest_mount),
        user = %args.publish_user,
        "authenticated publishing ready"
    );

    let mut ticker = tokio::time::interval(Duration::from_millis(40));
    let mut frame = 0u64;
    loop {
        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                result?;
                break;
            }
            _ = ticker.tick() => {
                publisher.try_push(sample_unit(&live.description.tracks[0], frame, 25))?;
                frame = frame.wrapping_add(1);
            }
        }
    }
    publisher.close().await?;
    server.shutdown().await?;
    Ok(())
}
