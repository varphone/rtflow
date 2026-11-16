use super::*;

pub(super) fn control_proxies_handler(state: Arc<AdminState>) -> Handler {
    Arc::new(move |req, writer: ResponseWriter<_>| {
        let state = state.clone();
        Box::pin(async move {
            if auth(&state, &req).is_none() {
                return unauthorized(writer).await;
            }
            let Some(control) = control(&state) else {
                return respond_json(writer, 404, &json!({"error":"control_disabled"}), None).await;
            };
            respond_json(writer, 200, &control.proxies(), None).await
        })
    })
}
