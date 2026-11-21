//! Local users, their roles and the grants of each role, kept in a TOML file
//! (the users file), and what a set of roles may do with a host.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::SystemTime;

use rxh_core::config;
use serde::{Deserialize, Serialize};

use crate::edit;

/// The built-in role that may do everything. It cannot be defined as a
/// `[[role]]`.
pub const ADMIN_ROLE: &str = "admin";

/// bcrypt cost of the stored hashes.
#[cfg(not(test))]
const COST: u32 = bcrypt::DEFAULT_COST;
/// The lowest cost bcrypt accepts, so that tests stay fast.
#[cfg(test)]
const COST: u32 = 4;

/// Shortest accepted password, in characters.
const MIN_PASSWORD_CHARS: usize = 8;
/// Longest accepted password, in bytes: bcrypt reads no further.
const MAX_PASSWORD_BYTES: usize = 72;

/// What a grant allows on the hosts it reaches. None implies another.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Permission {
    /// See the host in lists, its statistics and its form.
    View,
    /// Change the host.
    Modify,
    /// Create a host in a group.
    Create,
    /// Delete the host.
    Delete,
}

/// The hosts a grant reaches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// Every host whose `groups` has this group.
    Group(String),
    /// The host with this `name`.
    Host(String),
}

/// Permissions on the hosts of a target. In the file:
/// `{ group = "web", allow = ["view"] }` or `{ host = "shop", allow = [...] }`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "GrantFields", into = "GrantFields")]
pub struct Grant {
    /// The hosts reached.
    pub target: Target,
    /// What is allowed on them.
    pub allow: Vec<Permission>,
}

/// A grant as written in the file, where the target is one of two keys.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GrantFields {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    group: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    host: Option<String>,
    allow: Vec<Permission>,
}

impl TryFrom<GrantFields> for Grant {
    type Error = String;

    fn try_from(fields: GrantFields) -> Result<Self, String> {
        let target = match (fields.group, fields.host) {
            (Some(group), None) => Target::Group(group),
            (None, Some(host)) => Target::Host(host),
            _ => return Err("a grant names exactly one of `group` and `host`".to_owned()),
        };
        Ok(Self {
            target,
            allow: fields.allow,
        })
    }
}

impl From<Grant> for GrantFields {
    fn from(grant: Grant) -> Self {
        let (group, host) = match grant.target {
            Target::Group(group) => (Some(group), None),
            Target::Host(host) => (None, Some(host)),
        };
        Self {
            group,
            host,
            allow: grant.allow,
        }
    }
}

/// A named set of grants.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Role {
    /// Name, as `[a-z0-9][a-z0-9_-]*`.
    pub name: String,
    /// The grants, whose permissions add up.
    #[serde(default)]
    pub grant: Vec<Grant>,
}

/// A local user.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct User {
    /// Name: a role name that may also have `.` and `@`.
    pub name: String,
    /// bcrypt hash of the password.
    pub password: String,
    /// Names of the roles: defined ones or [`ADMIN_ROLE`].
    #[serde(default)]
    pub roles: Vec<String>,
}

impl fmt::Debug for User {
    /// Leaves the hash out, so that it cannot reach a log line.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("User")
            .field("name", &self.name)
            .field("roles", &self.roles)
            .finish_non_exhaustive()
    }
}

/// The content of the users file: `[[user]]` and `[[role]]` tables.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Access {
    /// The local users.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub user: Vec<User>,
    /// The defined roles.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub role: Vec<Role>,
}

impl Access {
    /// Reads and validates the users file. A missing file is an empty one.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(err) => return Err(format!("{}: {err}", path.display())),
        };
        let access: Self = toml::from_str(&text).map_err(|err| parse_error(path, &text, &err))?;
        access
            .validate()
            .map_err(|err| format!("{}: {err}", path.display()))?;
        Ok(access)
    }

    /// Replaces the users file atomically, creating its directories. Only the
    /// owner can read the result, whatever the mode of the file it replaces.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let text = toml::to_string(self).map_err(|err| err.to_string())?;
        // The atomic write keeps the mode of the file it replaces.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            match fs::set_permissions(path, fs::Permissions::from_mode(0o600)) {
                Err(err) if err.kind() != io::ErrorKind::NotFound => {
                    return Err(format!("{}: {err}", path.display()));
                }
                _ => {}
            }
        }
        edit::write_atomically(path, &text)
    }

    /// Checks names, references between users and roles, grants and hashes.
    pub fn validate(&self) -> Result<(), String> {
        for (index, user) in self.user.iter().enumerate() {
            if !is_user_name(&user.name) {
                return Err(format!(
                    "user {:?}: a name is 1 to 64 of a-z, 0-9, `_`, `-`, `.` and `@`, starting \
                     with a letter or a digit",
                    user.name
                ));
            }
            if self.user[..index]
                .iter()
                .any(|other| other.name == user.name)
            {
                return Err(format!("user {:?} is defined twice", user.name));
            }
            if user.password.parse::<bcrypt::HashParts>().is_err() {
                return Err(format!(
                    "user {:?}: the password is not a bcrypt hash",
                    user.name
                ));
            }
            for role in &user.roles {
                if role != ADMIN_ROLE && self.find_role(role).is_none() {
                    return Err(format!("user {:?}: no role {role:?}", user.name));
                }
            }
        }
        for (index, role) in self.role.iter().enumerate() {
            if role.name == ADMIN_ROLE {
                return Err(format!(
                    "role {ADMIN_ROLE:?} is built in and cannot be defined"
                ));
            }
            if !config::is_group_name(&role.name) {
                return Err(format!(
                    "role {:?}: a name is 1 to 64 of a-z, 0-9, `_` and `-`, starting with a \
                     letter or a digit",
                    role.name
                ));
            }
            if self.role[..index]
                .iter()
                .any(|other| other.name == role.name)
            {
                return Err(format!("role {:?} is defined twice", role.name));
            }
            for grant in &role.grant {
                match &grant.target {
                    Target::Group(group) if !config::is_group_name(group) => {
                        return Err(format!("role {:?}: invalid group {group:?}", role.name));
                    }
                    Target::Host(host) if host.is_empty() => {
                        return Err(format!("role {:?}: a host grant needs a name", role.name));
                    }
                    Target::Host(host) if grant.allow.contains(&Permission::Create) => {
                        return Err(format!(
                            "role {:?}: `create` is for groups, not for host {host:?}",
                            role.name
                        ));
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }

    /// What a user with `roles` may do. Roles that are not defined add
    /// nothing.
    #[must_use]
    pub fn scope(&self, roles: &[String]) -> Scope {
        if roles.iter().any(|role| role == ADMIN_ROLE) {
            return Scope::Admin;
        }
        let grants: Vec<Grant> = roles
            .iter()
            .filter_map(|name| self.find_role(name))
            .flat_map(|role| role.grant.iter().cloned())
            .collect();
        Scope::Grants(grants)
    }

    /// Adds a user with a new password.
    pub fn add_user(&mut self, name: &str, password: &str, roles: &[String]) -> Result<(), String> {
        if self.find_user(name).is_some() {
            return Err(format!("user {name:?} exists"));
        }
        let user = User {
            name: name.to_owned(),
            password: hash(password)?,
            roles: roles.to_vec(),
        };
        self.apply(|access| {
            access.user.push(user);
            Ok(())
        })
    }

    /// Replaces the password of a user.
    pub fn set_password(&mut self, name: &str, password: &str) -> Result<(), String> {
        let hashed = hash(password)?;
        self.user_mut(name)?.password = hashed;
        Ok(())
    }

    /// Replaces the roles of a user. Refuses to leave no administrator.
    pub fn set_roles(&mut self, name: &str, roles: &[String]) -> Result<(), String> {
        if !roles.iter().any(|role| role == ADMIN_ROLE) {
            self.refuse_last_administrator(name)?;
        }
        self.apply(|access| {
            access.user_mut(name)?.roles = roles.to_vec();
            Ok(())
        })
    }

    /// Removes a user. Refuses to remove the last administrator.
    pub fn remove_user(&mut self, name: &str) -> Result<(), String> {
        self.user_mut(name)?;
        self.refuse_last_administrator(name)?;
        self.user.retain(|user| user.name != name);
        Ok(())
    }

    /// Creates a role, or replaces the one with the same name.
    pub fn put_role(&mut self, role: Role) -> Result<(), String> {
        self.apply(|access| {
            match access
                .role
                .iter_mut()
                .find(|existing| existing.name == role.name)
            {
                Some(existing) => *existing = role,
                None => access.role.push(role),
            }
            Ok(())
        })
    }

    /// Removes a role. Refuses a role some user has, naming the users.
    pub fn remove_role(&mut self, name: &str) -> Result<(), String> {
        if name == ADMIN_ROLE {
            return Err(format!(
                "role {ADMIN_ROLE:?} is built in and cannot be removed"
            ));
        }
        if self.find_role(name).is_none() {
            return Err(format!("no role {name:?}"));
        }
        let holders: Vec<&str> = self
            .user
            .iter()
            .filter(|user| user.roles.iter().any(|role| role == name))
            .map(|user| user.name.as_str())
            .collect();
        if !holders.is_empty() {
            return Err(format!("role {name:?} is in use by {}", holders.join(", ")));
        }
        self.role.retain(|role| role.name != name);
        Ok(())
    }

    /// Whether `password` is the password of user `name`. An unknown user
    /// costs one bcrypt comparison as well, so that timing does not tell
    /// which names exist.
    #[must_use]
    pub fn verify(&self, name: &str, password: &str) -> bool {
        static UNKNOWN: OnceLock<String> = OnceLock::new();
        match self.find_user(name) {
            Some(user) => bcrypt::verify(password, &user.password).unwrap_or(false),
            None => {
                let hash = UNKNOWN.get_or_init(|| {
                    bcrypt::hash("not a password", COST).expect("bcrypt hashes a short password")
                });
                let _ = bcrypt::verify(password, hash);
                false
            }
        }
    }

    fn find_user(&self, name: &str) -> Option<&User> {
        self.user.iter().find(|user| user.name == name)
    }

    fn find_role(&self, name: &str) -> Option<&Role> {
        self.role.iter().find(|role| role.name == name)
    }

    fn user_mut(&mut self, name: &str) -> Result<&mut User, String> {
        self.user
            .iter_mut()
            .find(|user| user.name == name)
            .ok_or_else(|| format!("no user {name:?}"))
    }

    /// Refuses when `name` is the only user with the `admin` role.
    fn refuse_last_administrator(&self, name: &str) -> Result<(), String> {
        let is_admin = |user: &User| user.roles.iter().any(|role| role == ADMIN_ROLE);
        let others = self
            .user
            .iter()
            .any(|user| user.name != name && is_admin(user));
        if !others && self.find_user(name).is_some_and(is_admin) {
            return Err(format!("{name:?} is the last administrator"));
        }
        Ok(())
    }

    /// Applies `edit` to a copy and keeps it only when the result validates.
    fn apply(&mut self, edit: impl FnOnce(&mut Self) -> Result<(), String>) -> Result<(), String> {
        let mut next = self.clone();
        edit(&mut next)?;
        next.validate()?;
        *self = next;
        Ok(())
    }
}

/// Where and why the users file does not parse. Never the source line, which
/// can hold a password hash.
fn parse_error(path: &Path, text: &str, err: &toml::de::Error) -> String {
    let Some(span) = err.span() else {
        return format!("{}: {}", path.display(), err.message());
    };
    let before = &text[..span.start];
    let line = before.matches('\n').count() + 1;
    let column = before
        .rsplit('\n')
        .next()
        .map_or(0, |last| last.chars().count())
        + 1;
    format!("{}:{line}:{column}: {}", path.display(), err.message())
}

/// Whether `name` is a valid user name: a role name that may also have `.`
/// and `@` after its first byte.
fn is_user_name(name: &str) -> bool {
    config::is_group_name(&name.replace(['.', '@'], "-"))
}

fn hash(password: &str) -> Result<String, String> {
    if password.chars().count() < MIN_PASSWORD_CHARS || password.len() > MAX_PASSWORD_BYTES {
        return Err(format!(
            "a password has at least {MIN_PASSWORD_CHARS} characters and at most \
             {MAX_PASSWORD_BYTES} bytes"
        ));
    }
    bcrypt::hash(password, COST).map_err(|err| err.to_string())
}

/// What grants look at in a `[[server]]`: its `name` and its `groups`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct Host {
    /// The `name`, which `host` grants target.
    pub name: Option<String>,
    /// The `groups`, which `group` grants target.
    #[serde(default)]
    pub groups: Vec<String>,
}

impl From<&config::Server> for Host {
    fn from(server: &config::Server) -> Self {
        Self {
            name: server.name.clone(),
            groups: server.groups.clone(),
        }
    }
}

/// What a set of roles may do with hosts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Scope {
    /// Everything: the `admin` role.
    Admin,
    /// What the grants of the roles allow, added up.
    Grants(Vec<Grant>),
}

impl Scope {
    /// Whether this is the `admin` role.
    #[must_use]
    pub fn is_admin(&self) -> bool {
        matches!(self, Self::Admin)
    }

    /// Whether `permission` is allowed on the host with `name` (when it has
    /// one) and `groups`: through a grant on the name or on any one group.
    #[must_use]
    pub fn permits(&self, permission: Permission, name: Option<&str>, groups: &[String]) -> bool {
        match self {
            Self::Admin => true,
            Self::Grants(grants) => grants.iter().any(|grant| {
                grant.allow.contains(&permission)
                    && match &grant.target {
                        Target::Group(group) => groups.contains(group),
                        Target::Host(host) => name == Some(host.as_str()),
                    }
            }),
        }
    }

    /// Everything this allows on `host`, in the order of [`Permission`]. The
    /// one answer to "what may this caller do with this server": the routes
    /// of the admin API that list, change or count a host all ask here.
    #[must_use]
    pub fn allowed(&self, host: &Host) -> Vec<Permission> {
        use Permission::{Create, Delete, Modify, View};

        let mut allowed = vec![View, Modify, Create, Delete];
        allowed.retain(|permission| self.permits(*permission, host.name.as_deref(), &host.groups));
        allowed
    }

    /// Whether `host` is seen at all.
    #[must_use]
    pub fn views(&self, host: &Host) -> bool {
        self.allowed(host).contains(&Permission::View)
    }

    /// Whether a host with `groups` may be created: `groups` is not empty and
    /// each has Create. The administrator may create any host.
    #[must_use]
    pub fn may_create(&self, groups: &[String]) -> bool {
        match self {
            Self::Admin => true,
            Self::Grants(_) => {
                let creatable = self.creatable_groups();
                !groups.is_empty() && groups.iter().all(|group| creatable.contains(group))
            }
        }
    }

    /// The groups with Create, sorted. Empty for the administrator, who may
    /// create in every group.
    #[must_use]
    pub fn creatable_groups(&self) -> Vec<String> {
        let Self::Grants(grants) = self else {
            return Vec::new();
        };
        let mut groups: Vec<String> = grants
            .iter()
            .filter(|grant| grant.allow.contains(&Permission::Create))
            .filter_map(|grant| match &grant.target {
                Target::Group(group) => Some(group.clone()),
                Target::Host(_) => None,
            })
            .collect();
        groups.sort();
        groups.dedup();
        groups
    }

    /// Whether a grant targets the host `name`.
    #[must_use]
    pub fn names_host(&self, name: &str) -> bool {
        match self {
            Self::Admin => false,
            Self::Grants(grants) => grants
                .iter()
                .any(|grant| matches!(&grant.target, Target::Host(host) if host == name)),
        }
    }
}

/// The users file, reread when its modification time changes, so that edits
/// made by another process (the CLI) apply.
pub struct Store {
    path: PathBuf,
    cache: Mutex<Cache>,
    /// Held through a whole change, so that changes do not interleave while
    /// `read` never waits on a hash.
    changes: Mutex<()>,
}

/// The last good content and the modification time last read.
struct Cache {
    access: Access,
    /// `None` when the file did not exist.
    modified: Option<SystemTime>,
}

impl Store {
    /// Opens the users file at `path`, which need not exist.
    #[must_use]
    pub fn open(path: PathBuf) -> Self {
        let cache = Cache {
            access: Access::default(),
            modified: None,
        };
        let store = Self {
            path,
            cache: Mutex::new(cache),
            changes: Mutex::new(()),
        };
        store.refresh(&mut store.lock(), /* force= */ true);
        store
    }

    /// The current content. When the file no longer parses or validates, the
    /// last good content.
    #[must_use]
    pub fn read(&self) -> Access {
        let mut cache = self.lock();
        self.refresh(&mut cache, /* force= */ false);
        cache.access.clone()
    }

    /// Reads the file, applies `edit`, validates and saves the result. Refuses
    /// when the file does not parse, rather than overwrite it with older
    /// content.
    pub fn change<T>(
        &self,
        edit: impl FnOnce(&mut Access) -> Result<T, String>,
    ) -> Result<T, String> {
        let _change = self
            .changes
            .lock()
            .expect("no code panics while holding the users change lock");
        let mut access = Access::load(&self.path)?;
        let result = edit(&mut access)?;
        access.validate()?;
        access.save(&self.path)?;
        let mut cache = self.lock();
        cache.modified = modified(&self.path);
        cache.access = access;
        Ok(result)
    }

    /// Whether a `host` grant of any role names `name`: creating a host with
    /// that name needs the `admin` role.
    #[must_use]
    pub fn reserved_host(&self, name: &str) -> bool {
        self.read()
            .role
            .iter()
            .flat_map(|role| &role.grant)
            .any(|grant| matches!(&grant.target, Target::Host(host) if host == name))
    }

    fn lock(&self) -> MutexGuard<'_, Cache> {
        self.cache
            .lock()
            .expect("no code panics while holding the users store lock")
    }

    /// Rereads the file when its modification time changed. A failure is
    /// logged once per modification time and keeps the last good content.
    fn refresh(&self, cache: &mut Cache, force: bool) {
        let modified = modified(&self.path);
        if !force && modified == cache.modified {
            return;
        }
        cache.modified = modified;
        match Access::load(&self.path) {
            Ok(access) => cache.access = access,
            Err(err) => {
                eprintln!("Main => Users file not read, keeping the last good content: {err}")
            }
        }
    }
}

fn modified(path: &Path) -> Option<SystemTime> {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    fn names(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    fn grant(target: Target, allow: &[Permission]) -> Grant {
        Grant {
            target,
            allow: allow.to_vec(),
        }
    }

    fn group(name: &str) -> Target {
        Target::Group(name.to_owned())
    }

    fn host(name: &str) -> Target {
        Target::Host(name.to_owned())
    }

    /// One administrator, `ana`, and one editor, `luis`, with the role of the
    /// design document.
    fn sample() -> Access {
        let mut access = Access::default();
        access
            .put_role(Role {
                name: "web-editors".to_owned(),
                grant: vec![
                    grant(
                        group("web"),
                        &[Permission::View, Permission::Modify, Permission::Create],
                    ),
                    grant(host("shop"), &[Permission::View]),
                ],
            })
            .unwrap();
        access
            .add_user("ana", "password-ana", &names(&[ADMIN_ROLE]))
            .unwrap();
        access
            .add_user("luis", "password-luis", &names(&["web-editors"]))
            .unwrap();
        access
    }

    #[test]
    fn the_file_round_trips() {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("nested").join("users.toml");
        let access = sample();
        access.save(&path).unwrap();
        assert_eq!(Access::load(&path).unwrap(), access);
    }

    #[test]
    fn the_documented_format_parses() {
        let hash = bcrypt::hash("password-ana", COST).unwrap();
        let text = format!(
            r#"
            [[user]]
            name = "ana"
            password = "{hash}"
            roles = ["admin"]

            [[role]]
            name = "web-editors"
            grant = [
              {{ group = "web", allow = ["view", "modify", "create", "delete"] }},
              {{ host = "shop", allow = ["view"] }},
            ]
        "#
        );
        let access: Access = toml::from_str(&text).unwrap();
        access.validate().unwrap();
        assert_eq!(
            access.role[0].grant[1],
            grant(host("shop"), &[Permission::View])
        );
        let both = "[[role]]\nname = \"r\"\ngrant = [{ group = \"g\", host = \"h\", allow = [] }]";
        assert!(toml::from_str::<Access>(both).is_err());
        let neither = "[[role]]\nname = \"r\"\ngrant = [{ allow = [] }]";
        assert!(toml::from_str::<Access>(neither).is_err());
    }

    #[test]
    fn a_missing_file_is_empty() {
        let directory = TempDir::new().unwrap();
        let access = Access::load(&directory.path().join("users.toml")).unwrap();
        assert_eq!(access, Access::default());
    }

    #[cfg(unix)]
    #[test]
    fn the_saved_file_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let directory = TempDir::new().unwrap();
        let created = directory.path().join("created.toml");
        sample().save(&created).unwrap();
        assert_eq!(
            fs::metadata(&created).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let path = directory.path().join("users.toml");
        fs::write(&path, "").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        sample().save(&path).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn a_parse_error_does_not_show_the_hash() {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("users.toml");
        let hash = "$2b$04$abcdefghijklmnopqrstuuWv0tTFLmu4BUQ4ZxWXdhwPQx6a4ZJ.";
        let broken = [
            (
                3,
                format!("[[user]]\nname = \"ana\"\npassword = \"{hash}\" stray\n"),
            ),
            (
                4,
                format!("[[user]]\nname = \"ana\"\npassword = \"{hash}\"\npassword = \"{hash}\"\n"),
            ),
        ];
        for (line, text) in broken {
            fs::write(&path, text).unwrap();
            let error = Access::load(&path).unwrap_err();
            assert!(!error.contains("$2b$"), "{error}");
            assert!(error.contains(&format!("users.toml:{line}:")), "{error}");
        }
    }

    #[test]
    fn a_read_does_not_wait_for_a_change() {
        use std::sync::mpsc;
        use std::thread;
        use std::time::Duration;

        let directory = TempDir::new().unwrap();
        let path = directory.path().join("users.toml");
        sample().save(&path).unwrap();
        let store = &Store::open(path);
        let (started, has_started) = mpsc::channel();
        let (release, released) = mpsc::channel::<()>();
        thread::scope(|scope| {
            scope.spawn(move || {
                store
                    .change(|_| {
                        started.send(()).unwrap();
                        released.recv().unwrap();
                        Ok(())
                    })
                    .unwrap();
            });
            has_started.recv().unwrap();
            let (done, read) = mpsc::channel();
            scope.spawn(move || done.send(store.read().user.len()).unwrap());
            let users = read.recv_timeout(Duration::from_secs(5));
            release.send(()).unwrap();
            assert_eq!(users, Ok(2), "the read waited for the change");
        });
    }

    #[test]
    fn admin_permits_everything() {
        let scope = sample().scope(&names(&[ADMIN_ROLE]));
        assert!(scope.is_admin());
        assert!(scope.permits(Permission::Delete, None, &[]));
        assert!(scope.may_create(&names(&["anything"])));
    }

    #[test]
    fn a_group_grant_reaches_hosts_of_the_group() {
        let scope = sample().scope(&names(&["web-editors"]));
        assert!(!scope.is_admin());
        assert!(scope.permits(Permission::Modify, Some("blog"), &names(&["other", "web"])));
        assert!(scope.permits(Permission::Modify, None, &names(&["web"])));
        assert!(!scope.permits(Permission::Modify, Some("blog"), &names(&["other"])));
        assert!(!scope.permits(Permission::Delete, Some("blog"), &names(&["web"])));
    }

    #[test]
    fn a_host_grant_reaches_that_name_only() {
        let scope = sample().scope(&names(&["web-editors"]));
        assert!(scope.permits(Permission::View, Some("shop"), &[]));
        assert!(!scope.permits(Permission::Modify, Some("shop"), &[]));
        assert!(!scope.permits(Permission::View, Some("shop2"), &[]));
        assert!(scope.names_host("shop"));
        assert!(!scope.names_host("web"));
    }

    #[test]
    fn a_host_without_name_is_reached_only_by_group() {
        let scope = sample().scope(&names(&["web-editors"]));
        assert!(!scope.permits(Permission::View, None, &[]));
        assert!(scope.permits(Permission::View, None, &names(&["web"])));
    }

    #[test]
    fn permissions_are_the_union_over_roles() {
        let mut access = sample();
        access
            .put_role(Role {
                name: "deleters".to_owned(),
                grant: vec![grant(group("web"), &[Permission::Delete])],
            })
            .unwrap();
        let web = names(&["web"]);
        assert!(
            !access
                .scope(&names(&["web-editors"]))
                .permits(Permission::Delete, None, &web)
        );
        assert!(
            !access
                .scope(&names(&["deleters"]))
                .permits(Permission::View, None, &web)
        );
        let both = access.scope(&names(&["web-editors", "deleters"]));
        assert!(both.permits(Permission::Delete, None, &web));
        assert!(both.permits(Permission::View, None, &web));
        // A role that is not defined (an OIDC mapping to nothing) adds nothing.
        assert!(
            !access
                .scope(&names(&["missing"]))
                .permits(Permission::View, None, &web)
        );
    }

    #[test]
    fn modify_does_not_imply_view() {
        let mut access = Access::default();
        access
            .put_role(Role {
                name: "writers".to_owned(),
                grant: vec![grant(group("web"), &[Permission::Modify])],
            })
            .unwrap();
        let scope = access.scope(&names(&["writers"]));
        assert!(scope.permits(Permission::Modify, None, &names(&["web"])));
        assert!(!scope.permits(Permission::View, None, &names(&["web"])));
    }

    #[test]
    fn create_needs_every_group_and_at_least_one() {
        let mut access = sample();
        access
            .put_role(Role {
                name: "api".to_owned(),
                grant: vec![grant(group("api"), &[Permission::Create, Permission::View])],
            })
            .unwrap();
        let scope = access.scope(&names(&["web-editors", "api"]));
        assert!(scope.may_create(&names(&["web"])));
        assert!(scope.may_create(&names(&["web", "api"])));
        assert!(!scope.may_create(&names(&["web", "other"])));
        assert!(!scope.may_create(&[]));
        assert_eq!(scope.creatable_groups(), names(&["api", "web"]));
        // View through one group is enough to see the host.
        assert!(scope.permits(Permission::View, None, &names(&["api", "other"])));
    }

    #[test]
    fn create_on_a_host_grant_is_rejected() {
        let mut access = Access::default();
        let result = access.put_role(Role {
            name: "creators".to_owned(),
            grant: vec![grant(host("shop"), &[Permission::Create])],
        });
        assert!(result.unwrap_err().contains("create"));
        assert!(access.role.is_empty());
        let mut defined = sample();
        defined.role[0]
            .grant
            .push(grant(host("shop"), &[Permission::Create]));
        assert!(defined.validate().is_err());
    }

    #[test]
    fn the_last_administrator_cannot_be_removed_or_demoted() {
        let mut access = sample();
        let error = access.remove_user("ana").unwrap_err();
        assert!(error.contains("last administrator"), "{error}");
        let error = access
            .set_roles("ana", &names(&["web-editors"]))
            .unwrap_err();
        assert!(error.contains("last administrator"), "{error}");
        access.set_roles("luis", &names(&[ADMIN_ROLE])).unwrap();
        access.set_roles("ana", &names(&["web-editors"])).unwrap();
        let error = access.remove_user("luis").unwrap_err();
        assert!(error.contains("last administrator"), "{error}");
        access.remove_user("ana").unwrap();
        assert_eq!(access.user.len(), 1);
    }

    #[test]
    fn a_role_in_use_cannot_be_removed() {
        let mut access = sample();
        let error = access.remove_role("web-editors").unwrap_err();
        assert!(error.contains("luis"), "{error}");
        assert!(access.remove_role(ADMIN_ROLE).is_err());
        assert!(access.remove_role("missing").is_err());
        access.remove_user("luis").unwrap();
        access.remove_role("web-editors").unwrap();
        assert!(access.role.is_empty());
    }

    #[test]
    fn an_unknown_role_is_rejected() {
        let mut access = sample();
        assert!(
            access
                .add_user("eva", "password-eva", &names(&["missing"]))
                .is_err()
        );
        assert!(access.set_roles("luis", &names(&["missing"])).is_err());
        let defined = Role {
            name: ADMIN_ROLE.to_owned(),
            grant: Vec::new(),
        };
        assert!(access.put_role(defined).is_err());
        assert!(access.add_user("Eva", "password-eva", &[]).is_err());
        assert!(access.add_user("ana", "password-eva", &[]).is_err());
        access
            .add_user("eva.l@example", "password-eva", &[])
            .unwrap();
        let mut edited = sample();
        edited.user[1].roles.push("missing".to_owned());
        assert!(edited.validate().is_err());
    }

    #[test]
    fn passwords_outside_8_chars_to_72_bytes_are_rejected() {
        let mut access = sample();
        assert!(access.add_user("eva", "seven-c", &[]).is_err());
        assert!(access.add_user("eva", &"x".repeat(73), &[]).is_err());
        // Eight characters, sixteen bytes.
        access.add_user("eva", "ññññññññ", &[]).unwrap();
        assert!(access.set_password("eva", &"ñ".repeat(37)).is_err());
        access.set_password("eva", &"x".repeat(72)).unwrap();
        assert!(access.verify("eva", &"x".repeat(72)));
    }

    #[test]
    fn verify_accepts_the_password_and_rejects_others_and_unknown_users() {
        let mut access = sample();
        assert!(access.verify("ana", "password-ana"));
        assert!(!access.verify("ana", "password-luis"));
        assert!(!access.verify("nobody", "password-ana"));
        access.set_password("ana", "another-one").unwrap();
        assert!(!access.verify("ana", "password-ana"));
        assert!(access.verify("ana", "another-one"));
    }

    #[test]
    fn the_store_sees_an_edit_made_by_another_process() {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("users.toml");
        let store = Store::open(path.clone());
        assert_eq!(store.read(), Access::default());
        sample().save(&path).unwrap();
        assert_eq!(store.read(), sample_without_hashes(&store.read()));
        assert!(store.read().verify("luis", "password-luis"));
        assert!(store.reserved_host("shop"));
        assert!(!store.reserved_host("web"));
        store.change(|access| access.remove_user("luis")).unwrap();
        assert_eq!(Access::load(&path).unwrap().user.len(), 1);
        let error = store
            .change(|access| access.remove_user("ana"))
            .unwrap_err();
        assert!(error.contains("last administrator"));
        assert_eq!(store.read().user.len(), 1);
    }

    /// The sample with the hashes of `stored`, which differ at every hashing.
    fn sample_without_hashes(stored: &Access) -> Access {
        let mut expected = sample();
        for (user, stored) in expected.user.iter_mut().zip(&stored.user) {
            user.password.clone_from(&stored.password);
        }
        expected
    }

    #[test]
    fn a_broken_file_keeps_the_last_good_content() {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("users.toml");
        sample().save(&path).unwrap();
        let store = Store::open(path.clone());
        assert_eq!(store.read().user.len(), 2);
        fs::write(&path, "[[user]\nname = ").unwrap();
        assert_eq!(store.read().user.len(), 2);
        fs::write(&path, "[[user]]\nname = \"Bad Name\"\npassword = \"x\"\n").unwrap();
        assert_eq!(store.read().user.len(), 2);
        assert!(store.change(|access| access.remove_user("luis")).is_err());
    }
}
