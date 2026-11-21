//! Routes of the configuration editor. Reading: the JSON schema of the
//! configuration, and the files on disk with their tables as entities,
//! secrets masked. Writing: a whole file or one entity, saved to the file,
//! validated and reloaded; a change that is not valid or does not reload
//! leaves every file as it was.
//!
//! No answer holds a secret. Errors about a file on disk give the place and
//! not the text, and errors of the validation and of the reload lose the
//! lines of the file a TOML error quotes.
//!
//! A caller without the `admin` role reads and writes hosts only, and only
//! as far as [`Scope::allowed`] says for the `name` and the `groups` of each:
//! see [`Listed::of`] for what it reads, and [`viewed_host`],
//! [`check_changed`], [`check_created`] and [`check_table`] for what it
//! writes. Those checks
//! run on the file that is being written, while it is locked.

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError};

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

use crate::access::{Host, Permission, Scope};
use crate::admin::{self, AdminState, Caller, Control};
use crate::edit::{self, ConfigFile, SaveError};
use crate::entity::{self, Body, Entity, Kind};
use crate::host_keys;
use crate::secret;

/// Status and message of a request that failed.
type Failure = (StatusCode, String);

/// One file of the configuration, masked. Only an administrator gets its
/// path and its text.
#[derive(Serialize)]
struct File {
    index: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<PathBuf>,
    /// SHA-256 of the text on disk, before masking.
    revision: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
}

#[derive(Serialize)]
struct Files {
    editable: bool,
    files: Vec<File>,
    entities: Vec<Listed>,
}

/// An entity as a caller gets it.
#[derive(Serialize)]
#[serde(untagged)]
enum Listed {
    /// A server, with what the caller may do with it.
    Server {
        #[serde(flatten)]
        entity: Entity,
        permissions: Vec<Permission>,
    },
    /// A pool or a section, for an administrator.
    Whole(Entity),
    /// A pool for anybody else: its name, to choose it in the form of a host.
    Name { kind: Kind, key: String },
}

impl Listed {
    /// What a caller with `scope` gets of `entity`: the servers it views,
    /// the names of the pools, and no section. An administrator gets all.
    fn of(entity: Entity, scope: &Scope) -> Option<Self> {
        match entity.kind {
            Kind::Server => {
                // A table that is not a host yet is for administrators.
                let host: Host = host_of(&entity.value).unwrap_or_default();
                let permissions: Vec<Permission> = scope.allowed(&host);
                permissions
                    .contains(&Permission::View)
                    .then_some(Self::Server {
                        entity,
                        permissions,
                    })
            }
            Kind::Upstream | Kind::Section if scope.is_admin() => Some(Self::Whole(entity)),
            Kind::Upstream => Some(Self::Name {
                kind: entity.kind,
                key: entity.key,
            }),
            Kind::Section => None,
        }
    }
}

/// The `name` and the `groups` of the table of a server, `None` when they
/// are not a string and a list of strings.
fn host_of(value: &Json) -> Option<Host> {
    Host::deserialize(value).ok()
}

/// The table of the server `key` (`FILE:POSITION`) of file `index`, and
/// its host: `None` when there is no such server or it is not a host.
fn host_at(document: &DocumentMut, index: usize, key: &str) -> Option<(Host, Json)> {
    let server: Entity = entity::list(index, document)
        .into_iter()
        .find(|entity| entity.kind == Kind::Server && entity.key == key)?;
    Some((host_of(&server.value)?, server.value))
}

/// The addresses `table` listens on, `None` when one is not an address.
fn listens(table: &Json) -> Option<Vec<SocketAddr>> {
    let texts: Vec<&Json> = match table.get("listen") {
        Some(Json::Array(texts)) => texts.iter().collect(),
        Some(text) => vec![text],
        None => Vec::new(),
    };
    texts
        .into_iter()
        .map(|text| text.as_str()?.parse().ok())
        .collect()
}

/// Whether `table` has no `host` matcher of its own.
fn has_no_host(table: &Json) -> bool {
    match table.get("host") {
        Some(Json::Array(names)) => names.is_empty(),
        Some(Json::String(name)) => name.is_empty(),
        _ => true,
    }
}

/// What a caller without the `admin` role may put in the table of a host,
/// besides what its grants say: the keys of [`host_keys::ALLOWED`], on the
/// addresses in `running` (the ones the servers that run listen on), under
/// a `host` of its own. `before` is the stored table, `None` for a new one.
fn check_table(before: Option<&Json>, after: &Json, running: &[SocketAddr]) -> Result<(), Failure> {
    host_keys::check(before, after).map_err(|message| forbidden(&message))?;
    let kept: Vec<SocketAddr> = before.and_then(listens).unwrap_or_default();
    let shared = |address: &SocketAddr| {
        kept.contains(address) || (address.port() != 0 && running.contains(address))
    };
    let is_kept = before.is_some_and(|before| before.get("listen") == after.get("listen"));
    if !is_kept && !listens(after).is_some_and(|addresses| addresses.iter().all(shared)) {
        return Err(forbidden(
            "this needs the admin role: listen takes only addresses another server listens on",
        ));
    }
    if has_no_host(after) && !before.is_some_and(has_no_host) {
        return Err(forbidden(
            "this needs the admin role: host must name what the host answers for",
        ));
    }
    Ok(())
}

/// The addresses the running servers listen on.
fn running_listens(state: &AdminState) -> Vec<SocketAddr> {
    let current = state.current.read().unwrap_or_else(PoisonError::into_inner);
    let servers = current.config.servers.iter();
    servers.flat_map(|server| server.listen.clone()).collect()
}

/// Answer to a caller without the `admin` role about a host it does not
/// view, which is the answer about a host that does not exist.
const NO_HOST: &str = "no such host";

/// Answer to a save that names another revision than the one of the file.
const FILE_CHANGED: &str = "the file changed, read it again";

/// What a caller without the `admin` role is told instead of
/// [`FILE_CHANGED`].
const CONFIGURATION_CHANGED: &str = "the configuration changed, read it again";

fn forbidden(message: &str) -> Failure {
    (StatusCode::FORBIDDEN, message.to_owned())
}

fn bad_request(message: String) -> Failure {
    (StatusCode::BAD_REQUEST, message)
}

fn not_a_host() -> Failure {
    bad_request(String::from(
        "`name` must be a string and `groups` a list of strings",
    ))
}

/// Pools, sections and whole files are for administrators.
fn check_kind(scope: &Scope, kind: Kind) -> Result<(), Failure> {
    if scope.is_admin() || kind == Kind::Server {
        Ok(())
    } else {
        Err(forbidden(admin::NEEDS_ADMIN))
    }
}

/// The stored host at `key`, for a caller without the `admin` role that
/// wants to do `needed` with it: `404` unless the caller views it, `403`
/// with `refusal` unless it may do that.
fn viewed_host(
    scope: &Scope,
    stored: &DocumentMut,
    index: usize,
    key: &str,
    needed: Permission,
    refusal: &str,
) -> Result<(Host, Json), Failure> {
    let (host, table) = host_at(stored, index, key)
        .filter(|(host, _)| scope.views(host))
        .ok_or((StatusCode::NOT_FOUND, String::from(NO_HOST)))?;
    if scope.allowed(&host).contains(&needed) {
        Ok((host, table))
    } else {
        Err(forbidden(refusal))
    }
}

/// Whether a caller without the `admin` role may turn the host `before`
/// into `after`: the name stays, each added group is one it may create in,
/// and the result is still a host it may change.
fn check_changed(scope: &Scope, before: &Host, after: &Host) -> Result<(), Failure> {
    if after.name != before.name {
        return Err(forbidden(
            "changing the name of a host needs the admin role",
        ));
    }
    let added: Vec<String> = after
        .groups
        .iter()
        .filter(|group| !before.groups.contains(group))
        .cloned()
        .collect();
    if !added.is_empty() && !scope.may_create(&added) {
        return Err(forbidden(
            "adding a group to a host needs create on that group",
        ));
    }
    if !scope.allowed(after).contains(&Permission::Modify) {
        return Err(forbidden(
            "the groups left must still let you change the host",
        ));
    }
    Ok(())
}

/// Whether a caller without the `admin` role may create `host`. `reserved`
/// says whether a grant of any role targets a name: it is asked only of who
/// may create, so the others do not learn which names are reserved.
fn check_created(
    scope: &Scope,
    host: &Host,
    reserved: impl Fn(&str) -> bool,
) -> Result<(), Failure> {
    if !scope.may_create(&host.groups) {
        return Err(forbidden(
            "creating a host needs create on each of its groups, and at least one group",
        ));
    }
    if host.name.as_deref().is_some_and(reserved) {
        return Err(forbidden("this name is reserved"));
    }
    Ok(())
}

/// What a caller with `scope` is told of a save that failed. Without the
/// `admin` role: nothing about what changed in a file, about a key or a
/// file that does not exist, or about the files on disk.
fn told(scope: &Scope, failure: Failure) -> Failure {
    if scope.is_admin() {
        return failure;
    }
    match failure {
        (StatusCode::NOT_FOUND, _) => (StatusCode::NOT_FOUND, String::from(NO_HOST)),
        (StatusCode::CONFLICT, message) if message == FILE_CHANGED => {
            (StatusCode::CONFLICT, String::from(CONFIGURATION_CHANGED))
        }
        (StatusCode::INTERNAL_SERVER_ERROR, _) => internal("the configuration could not be saved"),
        other => other,
    }
}

/// `GET /schema`: the JSON schema of the configuration, as `rxh schema`
/// prints it.
pub(crate) fn schema() -> Response<Full<Bytes>> {
    admin::json_response(StatusCode::OK, &Config::json_schema())
}

/// `GET /config/files`: the main file then the included ones, and their
/// entities. Without a configuration file nothing is editable. A caller
/// without the `admin` role gets the revision of each file, the servers it
/// views and the names of the pools.
pub(crate) async fn files(state: &AdminState, caller: &Caller) -> Response<Full<Bytes>> {
    let scope: Scope = caller.scope();
    let is_admin: bool = scope.is_admin();
    let Some(path) = state.config_path.clone() else {
        let none = Files {
            editable: false,
            files: Vec::new(),
            entities: Vec::new(),
        };
        return admin::json_response(StatusCode::OK, &none);
    };
    match tokio::task::spawn_blocking(move || read(&path, &scope)).await {
        Ok(Ok(files)) => admin::json_response(StatusCode::OK, &files),
        Ok(Err(message)) if is_admin => admin::error(StatusCode::INTERNAL_SERVER_ERROR, &message),
        // The message names a file and a place in it.
        Ok(Err(_)) => admin::error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "the configuration files could not be read",
        ),
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

fn read(path: &Path, scope: &Scope) -> Result<Files, String> {
    let paths: Vec<PathBuf> = paths(path)?;
    let is_admin: bool = scope.is_admin();
    let mut files: Vec<File> = Vec::new();
    let mut entities: Vec<Listed> = Vec::new();
    for (index, path) in paths.into_iter().enumerate() {
        let text = fs::read_to_string(&path).map_err(|err| format!("{}: {err}", path.display()))?;
        let revision = revision(&text);
        let mut document: DocumentMut = text.parse().map_err(|err: toml_edit::TomlError| {
            format!("{}: {}", path.display(), place(&err.to_string()))
        })?;
        secret::mask(&mut document);
        for entity in entity::list(index, &document) {
            entities.extend(Listed::of(entity, scope));
        }
        files.push(File {
            index,
            path: is_admin.then_some(path),
            revision,
            text: is_admin.then(|| document.to_string()),
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

/// `PUT /config/files/{index}`: replaces the text of one file. For
/// administrators.
pub(crate) async fn put_file(
    state: &Arc<AdminState>,
    caller: &Caller,
    index: &str,
    request: Request<Incoming>,
) -> Response<Full<Bytes>> {
    let saved = async {
        if !caller.is_admin() {
            return Err(forbidden(admin::NEEDS_ADMIN));
        }
        let index: usize = file_index(index)?;
        let body: FileBody = json_body(request).await?;
        save(state, index, Some(&body.revision), move |file| {
            let stored: DocumentMut = file.document().clone();
            file.set_text(&body.text).map_err(bad_request)?;
            secret::restore_file(file.document_mut(), &stored).map_err(bad_request)
        })
        .await
    };
    answer(saved.await, StatusCode::OK, &json!({ "saved": true }))
}

/// `POST /config/entities/{kind}`: adds a pool, a server or a section. A
/// caller without the `admin` role adds a server, see [`check_created`].
pub(crate) async fn create(
    state: &Arc<AdminState>,
    caller: &Caller,
    kind: &str,
    request: Request<Incoming>,
) -> Response<Full<Bytes>> {
    let scope: Scope = caller.scope();
    let created = async {
        let kind: Kind = kind_of(kind)?;
        check_kind(&scope, kind)?;
        let body: EntityBody = json_body(request).await?;
        let content = Content::of(body.value, body.toml)?;
        let index: usize = body.file;
        let (key_found, key) = oneshot::channel();
        let users: Arc<AdminState> = Arc::clone(state);
        let scope: Scope = scope.clone();
        save(state, index, None, move |file| {
            let stored: DocumentMut = file.document().clone();
            let key: String = entity::create(
                file.document_mut(),
                kind,
                body.key.as_deref(),
                content.body(),
            )
            .map_err(bad_request)?;
            secret::restore(file.document_mut(), &stored).map_err(bad_request)?;
            let key: String = match kind {
                Kind::Server => format!("{index}:{key}"),
                Kind::Upstream | Kind::Section => key,
            };
            if !scope.is_admin() {
                let (host, table) = host_at(file.document(), index, &key).ok_or_else(not_a_host)?;
                check_created(&scope, &host, |name| users.access.reserved_host(name))?;
                check_table(None, &table, &running_listens(&users))?;
            }
            let _ = key_found.send(key);
            Ok(())
        })
        .await?;
        key.await
            .map_err(|_| internal("the saved entity has no key"))
    };
    match created.await.map_err(|failure| told(&scope, failure)) {
        Ok(key) => admin::json_response(StatusCode::CREATED, &json!({ "key": key })),
        Err((status, message)) => admin::error(status, &message),
    }
}

/// `PUT /config/entities/{kind}/{key}`: replaces one entity. A caller
/// without the `admin` role replaces a server it may change, see
/// [`check_changed`].
pub(crate) async fn replace(
    state: &Arc<AdminState>,
    caller: &Caller,
    kind: &str,
    key: &str,
    request: Request<Incoming>,
) -> Response<Full<Bytes>> {
    let scope: Scope = caller.scope();
    let saved = async {
        let kind: Kind = kind_of(kind)?;
        check_kind(&scope, kind)?;
        let body: EntityBody = json_body(request).await?;
        let content = Content::of(body.value, body.toml)?;
        let revision: String = body.revision.ok_or_else(revision_required)?;
        let listed: String = key.to_owned();
        let (index, key) = position_of(state, &scope, kind, key).await?;
        let scope: Scope = scope.clone();
        let users: Arc<AdminState> = Arc::clone(state);
        save(state, index, Some(&revision), move |file| {
            let stored: DocumentMut = file.document().clone();
            let before: Option<(Host, Json)> = if scope.is_admin() {
                None
            } else {
                let refusal = "changing this host needs modify on it";
                let needed = Permission::Modify;
                Some(viewed_host(
                    &scope, &stored, index, &listed, needed, refusal,
                )?)
            };
            let document: &mut DocumentMut = file.document_mut();
            match &content {
                Content::Value(value) => entity::replace_value(document, kind, &key, value),
                Content::Toml(toml) => entity::replace_toml(document, kind, &key, toml),
            }
            .map_err(bad_request)?;
            // The other tables are the stored ones, so this only reaches the
            // entity that was written.
            secret::restore(document, &stored).map_err(bad_request)?;
            if let Some((before, stored)) = before {
                // The table as it will be written, secrets put back.
                let (after, table) = host_at(document, index, &listed).ok_or_else(not_a_host)?;
                check_changed(&scope, &before, &after)?;
                check_table(Some(&stored), &table, &running_listens(&users))?;
            }
            Ok(())
        })
        .await
    };
    let saved = saved.await.map_err(|failure| told(&scope, failure));
    answer(saved, StatusCode::OK, &json!({ "saved": true }))
}

/// `DELETE /config/entities/{kind}/{key}?revision=...`: deletes one entity.
/// A caller without the `admin` role deletes a server it may delete.
pub(crate) async fn remove(
    state: &Arc<AdminState>,
    caller: &Caller,
    kind: &str,
    key: &str,
    query: Option<&str>,
) -> Response<Full<Bytes>> {
    let scope: Scope = caller.scope();
    let saved = async {
        let kind: Kind = kind_of(kind)?;
        check_kind(&scope, kind)?;
        let revision: &str = query
            .and_then(|query| {
                query
                    .split('&')
                    .find_map(|pair| pair.strip_prefix("revision="))
            })
            .ok_or_else(revision_required)?;
        let listed: String = key.to_owned();
        let (index, key) = position_of(state, &scope, kind, key).await?;
        let scope: Scope = scope.clone();
        save(state, index, Some(revision), move |file| {
            if !scope.is_admin() {
                let refusal = "deleting this host needs delete on it";
                let needed = Permission::Delete;
                viewed_host(&scope, file.document(), index, &listed, needed, refusal)?;
            }
            entity::remove(file.document_mut(), kind, &key).map_err(bad_request)
        })
        .await
    };
    let saved = saved.await.map_err(|failure| told(&scope, failure));
    answer(saved, StatusCode::OK, &json!({ "saved": true }))
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

/// [`locate`] for an administrator. For anybody else, who writes servers
/// only, the file and the position the key names, without a look at the
/// files: whether the key exists is told by the checks of the save, which
/// answer the same for a host that is not viewed.
async fn position_of(
    state: &AdminState,
    scope: &Scope,
    kind: Kind,
    key: &str,
) -> Result<(usize, String), Failure> {
    if scope.is_admin() {
        return locate(state, kind, key).await;
    }
    key.split_once(':')
        .and_then(|(file, position)| Some((file.parse().ok()?, position.to_owned())))
        .ok_or((StatusCode::NOT_FOUND, String::from(NO_HOST)))
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
/// (which restores the masked secrets of what it writes and checks what its
/// caller may write), saves, reloads, and puts the file back if the reload
/// fails.
async fn save(
    state: &Arc<AdminState>,
    index: usize,
    revision: Option<&str>,
    change: impl FnOnce(&mut ConfigFile) -> Result<(), Failure> + Send + 'static,
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
    change: impl FnOnce(&mut ConfigFile) -> Result<(), Failure>,
) -> Result<ConfigFile, Failure> {
    let paths: Vec<PathBuf> = paths(main).map_err(internal)?;
    let path: &Path = paths.get(index).ok_or_else(|| no_file(index))?;
    let mut file: ConfigFile = open(path)?;
    let stored: String = self::revision(file.original().unwrap_or_default());
    if revision.is_some_and(|revision| revision != stored) {
        return Err((StatusCode::CONFLICT, String::from(FILE_CHANGED)));
    }
    change(&mut file)?;
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
