use rtsp::{FileSource, FileSourceOptions, LoopMode, MountConfig, RtspServer, ServerConfig};
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .ok_or("usage: file_server <media-file>")?;
    let server = RtspServer::bind(ServerConfig::new("0.0.0.0:8554".parse()?)).await?;
    let source = FileSource::open_async(
        path,
        FileSourceOptions {
            loop_mode: LoopMode::Forever,
            ..Default::default()
        },
    )
    .await?;
    let mount = server
        .register_mount(
            Arc::from("/movie"),
            MountConfig::Local {
                description: source.description().clone(),
            },
        )
        .await?;
    println!("serving rtsp://127.0.0.1:8554/movie");
    source.play(mount.publisher()?).wait().await?;
    server.shutdown().await?;
    Ok(())
}
