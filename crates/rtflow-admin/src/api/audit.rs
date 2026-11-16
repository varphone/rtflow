use super::*;

pub(super) fn audit_handler(state: Arc<AdminState>) -> Handler {
    Arc::new(move |req, writer: ResponseWriter<_>| {
        let state = state.clone();
        Box::pin(async move {
            if auth(&state, &req).is_none() {
                return unauthorized(writer).await;
            }
            let Some(control) = state.config.control.as_ref() else {
                return respond_json(writer, 404, &json!({"error":"control_disabled"}), None).await;
            };
            let limit = req
                .query
                .get("limit")
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(200)
                .clamp(1, 1000);
            respond_json(writer, 200, &control.audit_events(limit), None).await
        })
    })
}
