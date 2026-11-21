//! Routes of the configuration editor. Reading: the JSON schema of the
//! configuration, and the files on disk with their tables as entities,
//! secrets masked. Writing: a whole file or one entity, saved to the file,
//! validated and reloaded; a change that is not valid or does not reload
//! leaves every file as it was.
//!
//! No answer holds a secret. Errors about a file on disk give the place and
//! not the text, and errors of the validation and of the reload lose the
//! lines of the file a TOML error quotes.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use rxh_core::config::Config;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value as Json, json};
use sha2::{Digest, Sha256};
use tokio::sync::oneshot;
use toml_edit::DocumentMut;

use crate::admin::{self, AdminState, Control};
use crate::edit::{self, ConfigFile, SaveError};
use crate::entity::{self, Body, Entity, Kind};
use crate::secret;

/// Status and message of a request that failed.
type Failure = (StatusCode, String);

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

/// The main file and the files it includes.
fn paths(main: &Path) -> Result<Vec<PathBuf>, String> {
    crate::config_files(main)
        .map_err(|err| format!("{}: {}", main.display(), place(&err.to_string())))
}

/// What is told about a file on disk that cannot be read: the first line of
/// the error, which names the file, the line and the column. The lines after
/// it quote the file, and the broken line could hold a secret.
fn place(message: &str) -> &str {
    message.lines().next().unwrap_or_default()
}

/// `message` without the lines of the file a TOML error quotes. After a save
/// they are lines of the file with its secrets put back and its placeholders
/// replaced.
fn without_source(message: &str) -> String {
    let lines: Vec<&str> = message
        .lines()
        .filter(|line| {
            let gutter = line
                .trim_start()
                .trim_start_matches(|c: char| c.is_ascii_digit());
            !gutter.trim_start().starts_with('|')
        })
        .collect();
    lines.join("\n")
}

fn read(path: &Path) -> Result<Files, String> {
    let paths: Vec<PathBuf> = paths(path)?;
    let mut files: Vec<File> = Vec::new();
    let mut entities: Vec<Entity> = Vec::new();
    for (index, path) in paths.into_iter().enumerate() {
        let text = fs::read_to_string(&path).map_err(|err| format!("{}: {err}", path.display()))?;
        let revision = revision(&text);
        let mut document: DocumentMut = text.parse().map_err(|err: toml_edit::TomlError| {
            format!("{}: {}", path.display(), place(&err.to_string()))
        })?;
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

#[derive(Deserialize)]
struct FileBody {
    text: String,
    revision: String,
}

#[derive(Deserialize)]
struct EntityBody {
    value: Option<Map<String, Json>>,
    toml: Option<String>,
    /// Name of a new pool or section.
    key: Option<String>,
    /// File a new entity goes to, the main one by default.
    #[serde(default)]
    file: usize,
    revision: Option<String>,
}

/// What a client sends as an entity.
enum Content {
    Value(Map<String, Json>),
    Toml(String),
}

impl Content {
    fn of(value: Option<Map<String, Json>>, toml: Option<String>) -> Result<Self, Failure> {
        match (value, toml) {
            (Some(value), None) => Ok(Self::Value(value)),
            (None, Some(toml)) => Ok(Self::Toml(toml)),
            _ => Err((
                StatusCode::BAD_REQUEST,
                String::from("the body takes either `value` or `toml`"),
            )),
        }
    }

    fn body(&self) -> Body<'_> {
        match self {
            Self::Value(value) => Body::Value(value),
            Self::Toml(toml) => Body::Toml(toml),
        }
    }
}

/// `PUT /config/files/{index}`: replaces the text of one file.
pub(crate) async fn put_file(
    state: &Arc<AdminState>,
    index: &str,
    request: Request<Incoming>,
) -> Response<Full<Bytes>> {
    let saved = async {
        let index: usize = file_index(index)?;
        let body: FileBody = json_body(request).await?;
        save(state, index, Some(&body.revision), move |file| {
            let stored: DocumentMut = file.document().clone();
            file.set_text(&body.text)?;
            secret::restore_file(file.document_mut(), &stored)
        })
        .await
    };
    answer(saved.await, StatusCode::OK, &json!({ "saved": true }))
}

/// `POST /config/entities/{kind}`: adds a pool, a server or a section.
pub(crate) async fn create(
    state: &Arc<AdminState>,
    kind: &str,
    request: Request<Incoming>,
) -> Response<Full<Bytes>> {
    let created = async {
        let kind: Kind = kind_of(kind)?;
        let body: EntityBody = json_body(request).await?;
        let content = Content::of(body.value, body.toml)?;
        let index: usize = body.file;
        let (key_found, key) = oneshot::channel();
        save(state, index, None, move |file| {
            let stored: DocumentMut = file.document().clone();
            let key: String = entity::create(
                file.document_mut(),
                kind,
                body.key.as_deref(),
                content.body(),
            )?;
            secret::restore(file.document_mut(), &stored)?;
            let _ = key_found.send(match kind {
                Kind::Server => format!("{index}:{key}"),
                Kind::Upstream | Kind::Section => key,
            });
            Ok(())
        })
        .await?;
        key.await
            .map_err(|_| internal("the saved entity has no key"))
    };
    match created.await {
        Ok(key) => admin::json_response(StatusCode::CREATED, &json!({ "key": key })),
        Err((status, message)) => admin::error(status, &message),
    }
}

/// `PUT /config/entities/{kind}/{key}`: replaces one entity.
pub(crate) async fn replace(
    state: &Arc<AdminState>,
    kind: &str,
    key: &str,
    request: Request<Incoming>,
) -> Response<Full<Bytes>> {
    let saved = async {
        let kind: Kind = kind_of(kind)?;
        let body: EntityBody = json_body(request).await?;
        let content = Content::of(body.value, body.toml)?;
        let revision: String = body.revision.ok_or_else(revision_required)?;
        let (index, key) = locate(state, kind, key).await?;
        save(state, index, Some(&revision), move |file| {
            let stored: DocumentMut = file.document().clone();
            let document: &mut DocumentMut = file.document_mut();
            match &content {
                Content::Value(value) => entity::replace_value(document, kind, &key, value)?,
                Content::Toml(toml) => entity::replace_toml(document, kind, &key, toml)?,
            }
            // The other tables are the stored ones, so this only reaches the
            // entity that was written.
            secret::restore(document, &stored)
        })
        .await
    };
    answer(saved.await, StatusCode::OK, &json!({ "saved": true }))
}

/// `DELETE /config/entities/{kind}/{key}?revision=...`: deletes one entity.
pub(crate) async fn remove(
    state: &Arc<AdminState>,
    kind: &str,
    key: &str,
    query: Option<&str>,
) -> Response<Full<Bytes>> {
    let saved = async {
        let kind: Kind = kind_of(kind)?;
        let revision: &str = query
            .and_then(|query| {
                query
                    .split('&')
                    .find_map(|pair| pair.strip_prefix("revision="))
            })
            .ok_or_else(revision_required)?;
        let (index, key) = locate(state, kind, key).await?;
        save(state, index, Some(revision), move |file| {
            entity::remove(file.document_mut(), kind, &key)
        })
        .await
    };
    answer(saved.await, StatusCode::OK, &json!({ "saved": true }))
}

fn answer(outcome: Result<(), Failure>, status: StatusCode, body: &Json) -> Response<Full<Bytes>> {
    match outcome {
        Ok(()) => admin::json_response(status, body),
        Err((status, message)) => admin::error(status, &message),
    }
}

fn internal(message: impl Into<String>) -> Failure {
    (StatusCode::INTERNAL_SERVER_ERROR, message.into())
}

fn revision_required() -> Failure {
    (
        StatusCode::BAD_REQUEST,
        String::from("`revision` of the file is required"),
    )
}

fn kind_of(segment: &str) -> Result<Kind, Failure> {
    Kind::from_segment(segment).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            format!("no kind of entity named {segment}"),
        )
    })
}

fn file_index(segment: &str) -> Result<usize, Failure> {
    segment.parse().map_err(|_| no_file(segment))
}

fn no_file(index: impl std::fmt::Display) -> Failure {
    (
        StatusCode::NOT_FOUND,
        format!("no configuration file {index}"),
    )
}

fn main_path(state: &AdminState) -> Result<PathBuf, Failure> {
    state.config_path.clone().ok_or_else(|| {
        (
            StatusCode::CONFLICT,
            String::from("the daemon runs without a configuration file, nothing can be saved"),
        )
    })
}

async fn json_body<T: DeserializeOwned>(request: Request<Incoming>) -> Result<T, Failure> {
    let body: Bytes = admin::read_body(request, admin::MAX_CONFIG_BODY)
        .await
        .map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                String::from("could not read the body"),
            )
        })?;
    serde_json::from_slice(&body)
        .map_err(|err| (StatusCode::BAD_REQUEST, format!("invalid JSON body: {err}")))
}

/// Runs file system work off the runtime threads.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, Failure> + Send + 'static,
) -> Result<T, Failure> {
    tokio::task::spawn_blocking(work)
        .await
        .unwrap_or_else(|err| Err(internal(err.to_string())))
}

/// Opens a file for a change. An error says where the file is broken.
fn open(path: &Path) -> Result<ConfigFile, Failure> {
    ConfigFile::open(path).map_err(|err| internal(place(&err)))
}

/// File that defines the entity, the first one for a pool or a section, and
/// the key of the entity in that file: a server loses its file index.
async fn locate(state: &AdminState, kind: Kind, key: &str) -> Result<(usize, String), Failure> {
    let main: PathBuf = main_path(state)?;
    let key: String = key.to_owned();
    blocking(move || {
        for (index, path) in paths(&main).map_err(internal)?.iter().enumerate() {
            let file: ConfigFile = open(path)?;
            let defined = entity::list(index, file.document())
                .iter()
                .any(|entity| entity.kind == kind && entity.key == key);
            if defined {
                let local: &str = match kind {
                    Kind::Server => key.split_once(':').map_or(key.as_str(), |(_, at)| at),
                    Kind::Upstream | Kind::Section => &key,
                };
                return Ok((index, local.to_owned()));
            }
        }
        Err((
            StatusCode::NOT_FOUND,
            format!("{key} is not defined in the configuration files"),
        ))
    })
    .await
}

/// Opens the files, checks `revision` of file `index`, applies `change`
/// (which restores the masked secrets of what it writes), saves, reloads, and
/// puts the file back if the reload fails.
async fn save(
    state: &Arc<AdminState>,
    index: usize,
    revision: Option<&str>,
    change: impl FnOnce(&mut ConfigFile) -> Result<(), String> + Send + 'static,
) -> Result<(), Failure> {
    let state: Arc<AdminState> = Arc::clone(state);
    let revision: Option<String> = revision.map(str::to_owned);
    // A task of its own: a client that hangs up drops its request, which must
    // not happen between writing the file and reloading it.
    let saved = tokio::task::spawn(async move {
        let main: PathBuf = main_path(&state)?;
        let _saving = state.saving.lock().await;
        let file: ConfigFile =
            blocking(move || write(&main, index, revision.as_deref(), change)).await?;
        let Err((status, message)) = admin::request(&state, Control::Reload).await else {
            println!("Admin => Saved {}", file.path().display());
            return Ok(());
        };
        blocking(move || {
            edit::restore(&[&file])
                .map_err(|err| internal(format!("the reload failed and {err} was not put back")))
        })
        .await?;
        Err((status, without_source(&message)))
    });
    saved
        .await
        .unwrap_or_else(|err| Err(internal(err.to_string())))
}

/// Writes file `index` with `change` applied. An error leaves it as it was.
fn write(
    main: &Path,
    index: usize,
    revision: Option<&str>,
    change: impl FnOnce(&mut ConfigFile) -> Result<(), String>,
) -> Result<ConfigFile, Failure> {
    let paths: Vec<PathBuf> = paths(main).map_err(internal)?;
    let path: &Path = paths.get(index).ok_or_else(|| no_file(index))?;
    let mut file: ConfigFile = open(path)?;
    let stored: String = self::revision(file.original().unwrap_or_default());
    if revision.is_some_and(|revision| revision != stored) {
        return Err((
            StatusCode::CONFLICT,
            String::from("the file changed, read it again"),
        ));
    }
    change(&mut file).map_err(|message| (StatusCode::BAD_REQUEST, message))?;
    match edit::try_save_all(&[&file], main) {
        Ok(_) => Ok(file),
        Err(SaveError::Invalid(message)) => {
            Err((StatusCode::BAD_REQUEST, without_source(&message)))
        }
        Err(SaveError::Io(message)) => Err(internal(without_source(&message))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "s3cret-of-the-pool";

    #[test]
    fn errors_lose_the_lines_of_the_file() {
        let text: String = format!("[upstream.api]\nsecret = \"{SECRET}\", polcy = 1\n");
        let parse: String = text.parse::<DocumentMut>().unwrap_err().to_string();
        assert!(
            parse.contains(SECRET),
            "the test needs a quoted line: {parse}"
        );
        assert!(!without_source(&parse).contains(SECRET));
        assert_eq!(place(&parse), "TOML parse error at line 2, column 30");

        let text: String = format!(
            "upstream.api = {{ backends = [\"127.0.0.1:1\"], secret = \"{SECRET}\", polcy = 1 }}\n"
        );
        let schema: String = Config::from_toml(&text).unwrap_err().to_string();
        assert!(
            schema.contains(SECRET),
            "the test needs a quoted line: {schema}"
        );
        let told: String = without_source(&format!("the change would break it: {schema}"));
        assert!(!told.contains(SECRET), "{told}");
        assert!(told.contains("line 1"), "{told}");
        assert!(told.contains("polcy"), "the reason is kept: {told}");
    }
}
