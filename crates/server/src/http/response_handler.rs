//! `handle_response` entries of a proxy route: what to do with an upstream
//! response instead of streaming it to the client unchanged.

use hyper::{Method, Request, Response, StatusCode};
use rxh_core::config::HandleResponse;
use rxh_core::template::Template;

use crate::files::Files;
use crate::http::response::{self, BoxResponse};

/// One compiled `handle_response` entry.
#[derive(Debug)]
pub(crate) struct ResponseHandler {
    matcher: HandleResponse,
    action: Action,
}

#[derive(Debug)]
enum Action {
    /// Send the upstream response, with this status if any.
    Copy(Option<StatusCode>),

    /// Answer with a file, found at the rendered `rewrite` or else at the
    /// request path.
    Files {
        files: Files,
        rewrite: Option<Template>,
    },

    /// Answer with a redirect to the rendered `to`.
    Redirect { to: Template, status: StatusCode },
}

impl ResponseHandler {
    /// Fails with a message on what [`HandleResponse`] validation rejects.
    pub(crate) fn compile(config: &HandleResponse) -> Result<Self, String> {
        let status = |code: u16| StatusCode::from_u16(code).map_err(|err| err.to_string());
        let action = if let Some(files) = &config.serve {
            Action::Files {
                files: Files::new(files),
                rewrite: config
                    .rewrite
                    .as_deref()
                    .map(Template::for_response)
                    .transpose()?,
            }
        } else if let Some(redirect) = &config.redirect {
            Action::Redirect {
                to: Template::for_response(&redirect.to)?,
                status: status(redirect.status)?,
            }
        } else {
            Action::Copy(config.new_status().map(status).transpose()?)
        };
        Ok(Self {
            matcher: config.clone(),
            action,
        })
    }

    /// Returns true if the entry handles `upstream`.
    pub(crate) fn matches<B>(&self, upstream: &Response<B>) -> bool {
        self.matcher
            .matches(upstream.status().as_u16(), upstream.headers())
    }

    /// Runs the entry for the `upstream` response to `request` (to `host`).
    /// `None` means `upstream` itself is sent, its status replaced if the
    /// entry says so; otherwise the returned response replaces it.
    pub(crate) async fn apply<B>(
        &self,
        request: &Request<()>,
        host: &str,
        upstream: &mut Response<B>,
    ) -> Option<BoxResponse> {
        let render = |template: &Template| {
            template.render_response(
                host,
                request.uri(),
                upstream.status().as_u16(),
                upstream.headers(),
            )
        };
        match &self.action {
            Action::Copy(status) => {
                if let Some(status) = status {
                    *upstream.status_mut() = *status;
                }
                None
            }
            Action::Files { files, rewrite } => {
                let target = match rewrite {
                    Some(rewrite) => render(rewrite),
                    None => request.uri().path().to_owned(),
                };
                let path = target.split_once('?').map_or(&*target, |(path, _)| path);
                // The file answers the request whatever its method was, like
                // Caddy's `method * GET` in the `X-Accel-Redirect` recipe.
                let method = match *request.method() {
                    Method::HEAD => Method::HEAD,
                    _ => Method::GET,
                };
                let mut served = files.transfer(&method, request.headers(), path, "/").await;
                // An error page keeps the status of the error.
                if served.status() == StatusCode::OK {
                    *served.status_mut() = upstream.status();
                }
                Some(served)
            }
            Action::Redirect { to, status } => Some(response::redirect(*status, &render(to))),
        }
    }
}
