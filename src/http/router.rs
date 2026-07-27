use super::request::{HttpRequest, Method};
use super::response::ResponseWriter;
use crate::Result;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tokio::net::tcp::OwnedWriteHalf;

type ConnWriter = OwnedWriteHalf;

pub type Handler = Arc<
    dyn Fn(
            HttpRequest,
            ResponseWriter<ConnWriter>,
        ) -> Pin<Box<dyn Future<Output = Result<ResponseWriter<ConnWriter>>> + Send>>
        + Send
        + Sync,
>;

pub struct Route {
    pub method: Method,
    pub pattern: PathPattern,
    pub handler: Handler,
}

pub struct Router {
    routes: Vec<Route>,
    not_found: Handler,
    method_not_allowed: Handler,
}

impl Default for Router {
    fn default() -> Self {
        Self::new()
    }
}

impl Router {
    pub fn new() -> Self {
        Self {
            routes: Vec::new(),
            not_found: Arc::new(|_req, mut writer| {
                Box::pin(async move {
                    writer.write_body(404, "text/plain", b"Not Found").await?;
                    Ok(writer)
                })
            }),
            method_not_allowed: Arc::new(|_req, mut writer| {
                Box::pin(async move {
                    writer
                        .write_headers(405, &[("Allow", "GET, HEAD, OPTIONS")])
                        .await?;
                    writer.write_raw(b"Method Not Allowed").await?;
                    Ok(writer)
                })
            }),
        }
    }

    pub fn add(&mut self, method: Method, pattern: &str, handler: Handler) {
        self.routes.push(Route {
            method,
            pattern: PathPattern::new(pattern),
            handler,
        });
    }

    pub(crate) fn route(&self, method: &Method, path: &str) -> (&Handler, Vec<String>) {
        let mut path_match = false;
        for route in &self.routes {
            if route.pattern.matches(path).is_some() {
                path_match = true;
            }
            if &route.method != method {
                continue;
            }
            if let Some(params) = route.pattern.matches(path) {
                return (&route.handler, params);
            }
        }
        if path_match {
            (&self.method_not_allowed, Vec::new())
        } else {
            (&self.not_found, Vec::new())
        }
    }
}

pub struct PathPattern {
    segments: Vec<PatternSegment>,
}

enum PatternSegment {
    Literal(String),
    #[allow(dead_code)]
    Param(String),
    Wildcard,
}

impl PathPattern {
    pub fn new(pattern: &str) -> Self {
        let segments = pattern
            .split('/')
            .filter(|s| !s.is_empty())
            .map(|s| {
                if s == "*" {
                    PatternSegment::Wildcard
                } else if s.starts_with('{') && s.ends_with('}') {
                    PatternSegment::Param(s[1..s.len() - 1].to_string())
                } else {
                    PatternSegment::Literal(s.to_string())
                }
            })
            .collect();
        Self { segments }
    }

    fn matches(&self, path: &str) -> Option<Vec<String>> {
        let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        if parts.len() != self.segments.len() {
            if let Some(PatternSegment::Wildcard) = self.segments.last() {
                if parts.len() < self.segments.len() - 1 {
                    return None;
                }
            } else {
                return None;
            }
        }

        let mut params = Vec::new();
        for (i, seg) in self.segments.iter().enumerate() {
            match seg {
                PatternSegment::Literal(lit) => {
                    if i >= parts.len() || parts[i] != lit {
                        return None;
                    }
                }
                PatternSegment::Param(_) => {
                    if i >= parts.len() {
                        return None;
                    }
                    params.push(parts[i].to_string());
                }
                PatternSegment::Wildcard => {
                    break;
                }
            }
        }
        Some(params)
    }
}
