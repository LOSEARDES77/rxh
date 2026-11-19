//! `error_pages` of a server: files replacing the body of the error
//! responses RXH generates itself.

use std::path::Path;

use bytes::Bytes;
use hyper::header::{self, HeaderValue};
use rxh_core::config::Server;
use rxh_core::status::StatusMatcher;

use crate::files;
use crate::http::body;
use crate::http::response::{BoxResponse, Generated};

/// Pages of one server, read when the listener is built (start and reload).
#[derive(Debug, Default)]
pub(crate) struct ErrorPages(Vec<Page>);

#[derive(Debug)]
struct Page {
    status: StatusMatcher,
    content_type: &'static str,
    contents: Bytes,
}

impl ErrorPages {
    /// Reads the files of `server.error_pages`. Fails with a message naming
    /// the file that cannot be read.
    pub(crate) fn load(server: &Server) -> Result<Self, String> {
        let mut pages = Vec::new();
        for (status, file) in server.error_page_matchers()? {
            let contents = std::fs::read(file).map_err(|err| format!("{file}: {err}"))?;
            pages.push(Page {
                status,
                content_type: files::content_type(Path::new(file)),
                contents: contents.into(),
            });
        }
        Ok(Self(pages))
    }

    /// Replaces the body of `response` with its page if RXH generated it and
    /// a page matches its status. Status and other headers are kept, so a
    /// `429` keeps its `Retry-After` and a `401` its `WWW-Authenticate`.
    pub(crate) fn apply(&self, response: &mut BoxResponse) {
        if response.extensions().get::<Generated>().is_none() {
            return;
        }
        let status = response.status().as_u16();
        let Some(page) = self.0.iter().find(|page| page.status.matches(status)) else {
            return;
        };
        *response.body_mut() = body::full(page.contents.clone());
        let headers = response.headers_mut();
        headers.remove(header::CONTENT_LENGTH);
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static(page.content_type),
        );
    }
}
