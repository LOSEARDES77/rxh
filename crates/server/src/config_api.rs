//! Read routes of the configuration editor: the JSON schema of the
//! configuration, and the files on disk with their tables as entities,
//! secrets masked.

use std::fs;
use std::path::{Path, PathBuf};

use bytes::Bytes;
use http_body_util::Full;
use hyper::{Response, StatusCode};
use rxh_core::config::Config;
use serde::Serialize;
use sha2::{Digest, Sha256};
use toml_edit::DocumentMut;

use crate::admin::{self, AdminState};
use crate::entity::{self, Entity};
use crate::secret;

/// One file of the configuration, masked.
#[derive(Serialize)]
struct File {
    index: usize,
    path: PathBuf,
    /// SHA-256 of the text on disk, before masking.
    revision: String,
    text: String,
}

#[derive(Serialize)]
struct Files {
    editable: bool,
    files: Vec<File>,
    entities: Vec<Entity>,
}

/// `GET /schema`: the JSON schema of the configuration, as `rxh schema`
/// prints it.
pub(crate) fn schema() -> Response<Full<Bytes>> {
    admin::json_response(StatusCode::OK, &Config::json_schema())
}

/// `GET /config/files`: the main file then the included ones, and their
/// entities. Without a configuration file nothing is editable.
pub(crate) async fn files(state: &AdminState) -> Response<Full<Bytes>> {
    let Some(path) = state.config_path.clone() else {
        let none = Files {
            editable: false,
            files: Vec::new(),
            entities: Vec::new(),
        };
        return admin::json_response(StatusCode::OK, &none);
    };
    match tokio::task::spawn_blocking(move || read(&path)).await {
        Ok(Ok(files)) => admin::json_response(StatusCode::OK, &files),
        Ok(Err(message)) => admin::error(StatusCode::INTERNAL_SERVER_ERROR, &message),
        Err(err) => admin::error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    }
}

/// SHA-256 of `text` in lowercase hex.
pub(crate) fn revision(text: &str) -> String {
    crate::dns01::hex(&Sha256::digest(text.as_bytes()))
}

fn read(path: &Path) -> Result<Files, String> {
    let paths: Vec<PathBuf> = crate::config_files(path).map_err(|err| err.to_string())?;
    let mut files: Vec<File> = Vec::new();
    let mut entities: Vec<Entity> = Vec::new();
    for (index, path) in paths.into_iter().enumerate() {
        let text = fs::read_to_string(&path).map_err(|err| format!("{}: {err}", path.display()))?;
        let revision = revision(&text);
        let mut document: DocumentMut = text
            .parse()
            .map_err(|err| format!("{}: {err}", path.display()))?;
        secret::mask(&mut document);
        entities.extend(entity::list(index, &document));
        files.push(File {
            index,
            path,
            revision,
            text: document.to_string(),
        });
    }
    Ok(Files {
        editable: true,
        files,
        entities,
    })
}
