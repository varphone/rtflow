use clap::Parser;
use futures::future::BoxFuture;
use rtflow::{
    LocalPublisher, MountConfig, RtspServer, ServerConfig,
    pipeline::{Element, ElementOutput, PipelineBuilder, PipelineState, QueueConfig},
};
use rtflow_testkit::example::{
    CodecArg, advertised_url, description, init_tracing, mount_path, sample_unit,
};
use std::{future::ready, net::SocketAddr, sync::Arc, time::Duration};

struct PublisherSink {
    publisher: LocalPublisher,
}

impl Element for PublisherSink {
    fn name(&self) -> &str {
        "rtsp-publisher"
    }

    fn process<'a>(
        &'a self,
        input: rtflow::AccessUnit,
    ) -> BoxFuture<'a, rtflow::Result<ElementOutput>> {
        Box::pin(ready(
            self.publisher.try_push(input).map(|_| ElementOutput::Drop),
        ))
    }
}

/// Route generated access units through an Element DAG into an RTSP mount.
#[derive(Debug, Parser)]
struct Args {
    #[arg(long, default_value = "0.0.0.0:8554")]
    listen: SocketAddr,
    #[arg(long, default_value = "/pipeline")]
    mount: String,
    #[arg(long, default_value_t = 25)]
    fps: u32,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_tracing();
    let args = Args::parse();
    if args.fps == 0 || args.fps > 1_000 {
        return Err("--fps must be in 1..=1000".into());
    }
    let media = description("pipeline", &[CodecArg::H264]);
    let server = RtspServer::bind(ServerConfig::new(args.listen)).await?;
    let mount = server
        .register_mount(
            mount_path(&args.mount)?,
            MountConfig::Local { description: media },
        )
        .await?;

    let mut builder = PipelineBuilder::new();
    let source = builder.add_element(
        Arc::new(rtflow::pipeline::IdentityElement::new("device-source")),
        QueueConfig::default(),
    )?;
    let sink = builder.add_element(
        Arc::new(PublisherSink {
            publisher: mount.publisher()?,
        }),
        QueueConfig::default(),
    )?;
    builder.link(source, sink, QueueConfig::default())?;
    let pipeline = builder.build().await?;
    pipeline.set_state(PipelineState::Playing).await?;
    let ingress = pipeline.ingress();
    tracing::info!(
        url = %advertised_url(server.local_addr(), &args.mount),
        "pipeline source ready"
    );

    let mut ticker = tokio::time::interval(Duration::from_secs_f64(1.0 / f64::from(args.fps)));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut frame = 0u64;
    loop {
        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                result?;
                break;
            }
            _ = ticker.tick() => {
                ingress
                    .push(
                        source,
                        sample_unit(&mount.description.tracks[0], frame, args.fps),
                    )
                    .await?;
                frame = frame.wrapping_add(1);
            }
        }
    }
    tracing::info!(snapshot = ?pipeline.snapshot(), "stopping pipeline");
    pipeline.shutdown().await?;
    server.shutdown().await?;
    Ok(())
}
