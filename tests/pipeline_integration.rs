mod support;

use futures::future::BoxFuture;
use rtsp::{
    Codec, Error, LocalPublisher, MountConfig, RtspServer, ServerConfig,
    client::{ClientOptions, RtspClient},
    pipeline::{
        Element, ElementOutput, IdentityElement, PipelineBuilder, PipelineState, QueueConfig,
    },
};
use std::{future::ready, sync::Arc, time::Duration};
use support::{description, unit, wait_until, within};

struct PublisherSink {
    publisher: LocalPublisher,
}

impl Element for PublisherSink {
    fn name(&self) -> &str {
        "publisher"
    }

    fn process<'a>(
        &'a self,
        input: rtsp::AccessUnit,
    ) -> BoxFuture<'a, rtsp::Result<ElementOutput>> {
        Box::pin(ready(
            self.publisher.try_push(input).map(|_| ElementOutput::Drop),
        ))
    }
}

struct FailingElement;

impl Element for FailingElement {
    fn name(&self) -> &str {
        "failing"
    }

    fn process<'a>(
        &'a self,
        _input: rtsp::AccessUnit,
    ) -> BoxFuture<'a, rtsp::Result<ElementOutput>> {
        Box::pin(ready(Err(Error::Media("intentional failure".into()))))
    }
}

#[tokio::test]
async fn pipeline_feeds_rtsp_and_accounts_for_element_errors() {
    let server = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    let mount = server
        .register_mount(
            Arc::from("/pipeline"),
            MountConfig::Local {
                description: description("pipeline", &[Codec::H264]),
            },
        )
        .await
        .unwrap();
    let mut client = RtspClient::connect(
        url::Url::parse(&format!("rtsp://{}/pipeline", server.local_addr())).unwrap(),
        ClientOptions::default(),
    )
    .await
    .unwrap();

    let mut builder = PipelineBuilder::new();
    let source = builder
        .add_element(
            Arc::new(IdentityElement::new("source")),
            QueueConfig::default(),
        )
        .unwrap();
    let sink = builder
        .add_element(
            Arc::new(PublisherSink {
                publisher: mount.publisher().unwrap(),
            }),
            QueueConfig::default(),
        )
        .unwrap();
    builder.link(source, sink, QueueConfig::default()).unwrap();
    let pipeline = builder.build().await.unwrap();
    pipeline.set_state(PipelineState::Playing).await.unwrap();
    wait_until(Duration::from_secs(1), || {
        mount.snapshot().subscriber_count > 0
    })
    .await;
    let expected = unit(0, Codec::H264, 1);
    pipeline
        .ingress()
        .push(source, expected.clone())
        .await
        .unwrap();
    let received = within(Duration::from_secs(1), client.next())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(received.data, expected.data);
    pipeline.shutdown().await.unwrap();

    let mut builder = PipelineBuilder::new();
    let failing = builder
        .add_element(Arc::new(FailingElement), QueueConfig::default())
        .unwrap();
    let pipeline = builder.build().await.unwrap();
    pipeline.set_state(PipelineState::Playing).await.unwrap();
    pipeline
        .ingress()
        .push(failing, unit(0, Codec::H264, 2))
        .await
        .unwrap();
    wait_until(Duration::from_secs(1), || {
        pipeline.snapshot().elements[0].2.errors == 1
    })
    .await;
    assert_eq!(pipeline.snapshot().elements[0].2.processed, 0);
    pipeline.shutdown().await.unwrap();
    server.shutdown().await.unwrap();
}
