use super::*;

use bytes::Bytes;
use rtflow_media::Codec;
use std::time::Duration;

struct Sink {
    frames: Arc<Mutex<Vec<AccessUnit>>>,
}
impl Element for Sink {
    fn name(&self) -> &str {
        "sink"
    }

    fn process<'a>(&'a self, input: AccessUnit) -> BoxFuture<'a, Result<ElementOutput>> {
        self.frames.lock().unwrap().push(input);
        Box::pin(ready(Ok(ElementOutput::Drop)))
    }
}

fn frame() -> AccessUnit {
    AccessUnit {
        track_id: 0,
        codec: Codec::H264,
        pts: Duration::ZERO,
        dts: None,
        rtp_timestamp: 1,
        is_keyframe: true,
        data: Bytes::from_static(&[0x65, 1]),
    }
}

#[tokio::test]
async fn graph_forwards_and_preserves_bytes_storage() {
    let frames = Arc::new(Mutex::new(Vec::new()));
    let mut builder = PipelineBuilder::new();
    let source = builder
        .add_element(
            Arc::new(IdentityElement::new("source")),
            QueueConfig::default(),
        )
        .unwrap();
    let sink = builder
        .add_element(
            Arc::new(Sink {
                frames: frames.clone(),
            }),
            QueueConfig::default(),
        )
        .unwrap();
    builder.link(source, sink, QueueConfig::default()).unwrap();
    let pipeline = builder.build().await.unwrap();
    pipeline.set_state(PipelineState::Playing).await.unwrap();
    let input = frame();
    let pointer = input.data.as_ptr();
    assert_eq!(
        pipeline.ingress().push(source, input).await.unwrap(),
        IngressOutcome::Accepted
    );
    tokio::time::timeout(Duration::from_secs(1), async {
        while frames.lock().unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(frames.lock().unwrap()[0].data.as_ptr(), pointer);
    assert_eq!(pipeline.snapshot().elements[0].2.processed, 1);
    pipeline.shutdown().await.unwrap();
}

#[tokio::test]
async fn blocking_queue_has_a_deadline() {
    let (sender, _receiver) = mpsc::channel(1);
    sender.send(frame()).await.unwrap();
    let result = deliver(
        &sender,
        QueueConfig {
            capacity: 1,
            overflow: OverflowPolicy::Block,
            block_timeout: Duration::from_millis(1),
        },
        frame(),
    )
    .await;
    assert!(matches!(
        result,
        Err(Error::Limit("pipeline blocking queue"))
    ));
}

#[tokio::test]
async fn rejects_cycles_and_bounded_ingress_drops() {
    let mut builder = PipelineBuilder::new();
    let queue = QueueConfig {
        capacity: 1,
        overflow: OverflowPolicy::DropNewest,
        block_timeout: Duration::from_millis(100),
    };
    let first = builder
        .add_element(Arc::new(IdentityElement::new("a")), queue)
        .unwrap();
    let second = builder
        .add_element(Arc::new(IdentityElement::new("b")), queue)
        .unwrap();
    builder.link(first, second, queue).unwrap();
    assert!(builder.link(second, first, queue).is_ok());
    assert!(builder.build().await.is_err());

    let mut builder = PipelineBuilder::new();
    let node = builder
        .add_element(Arc::new(IdentityElement::new("paused")), queue)
        .unwrap();
    let pipeline = builder.build().await.unwrap();
    assert_eq!(
        pipeline.ingress().try_push(node, frame()).unwrap(),
        IngressOutcome::Accepted
    );
    assert_eq!(
        pipeline.ingress().try_push(node, frame()).unwrap(),
        IngressOutcome::DroppedQueueFull
    );
}
