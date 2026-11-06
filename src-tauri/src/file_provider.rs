//! The app's own Finder locations, one per canvas shown in Finder.
//!
//! Only the app that contains a File Provider extension may add or remove its locations, and it
//! must be signed into the app group the extension reads. So this runs in the app itself, which
//! a signed build is: macOS will not run a separate program inside the app with those
//! entitlements. It never reads the disk: the extension gets everything through the app's media
//! port, whose address and token are left here, in the app group's folder, for it to read.
//!
//! The commands take a JSON request and give a JSON reply:
//!
//!   show     {base, token, serial, canvas, name, revision}  adds a location, gives its folder
//!   hide     {serial, canvas}                                removes it
//!   sync     {base, token, serial, canvases: [{id, name, revision}]}
//!            refreshes this disk's locations, removes those whose canvas has gone, and marks
//!            other disks' locations as unreachable
//!   offline  {}                                              marks every location unreachable
//!   list     {}                                              the locations there are

use anyhow::{bail, Context, Result};
use block2::RcBlock;
use objc2::rc::Retained;
use objc2::AnyThread;
use objc2_file_provider::{
    NSFileProviderDomain, NSFileProviderManager, NSFileProviderRootContainerItemIdentifier,
    NSFileProviderWorkingSetContainerItemIdentifier,
};
use objc2_foundation::{NSArray, NSError, NSFileManager, NSString, NSURL};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::ptr::NonNull;
use std::sync::mpsc;
use std::time::Duration;

const APP_GROUP: &str = "group.com.antimatterstudios.captainsalvage";
const EXTENSION: &str = "Captain Salvage.appex";
/// How long macOS gets to answer a request, and to show a new location's folder.
const ANSWER_SECONDS: u64 = 30;
const SIGNAL_SECONDS: u64 = 10;
const FOLDER_TRIES: usize = 20;

#[derive(Deserialize, Default)]
struct Canvas {
    id: i64,
    revision: String,
}

#[derive(Deserialize, Default)]
struct Request {
    base: Option<String>,
    token: Option<String>,
    serial: Option<String>,
    canvas: Option<i64>,
    name: Option<String>,
    revision: Option<String>,
    canvases: Option<Vec<Canvas>>,
}

/// What the extension reads to reach the open disk: the media port, and which canvas.
#[derive(Serialize)]
struct Config<'a> {
    base: &'a str,
    token: &'a str,
    serial: &'a str,
    canvas: i64,
    revision: &'a str,
}

/// Whether this copy of the app holds the extension. A build without signing material does
/// not, and Finder is then not offered.
pub fn available() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|program| Some(program.parent()?.parent()?.join("PlugIns").join(EXTENSION)))
        .is_some_and(|extension| extension.is_dir())
}

/// Starts a call that answers through a completion handler, and waits for its answer.
fn wait<T: Send + 'static>(seconds: u64, start: impl FnOnce(mpsc::Sender<T>)) -> Option<T> {
    let (sender, receiver) = mpsc::channel();
    start(sender);
    receiver.recv_timeout(Duration::from_secs(seconds)).ok()
}

fn error_text(error: *mut NSError) -> Option<String> {
    // SAFETY: a non-null error from a completion handler is a valid NSError for the call.
    unsafe { error.as_ref() }.map(|error| error.localizedDescription().to_string())
}

fn domains_folder() -> Result<PathBuf> {
    let manager = NSFileManager::defaultManager();
    let url = manager
        .containerURLForSecurityApplicationGroupIdentifier(&NSString::from_str(APP_GROUP))
        .context("This copy of the app is not signed into its app group.")?;
    let folder = PathBuf::from(
        url.path()
            .context("the app group has no folder")?
            .to_string(),
    )
    .join("domains");
    std::fs::create_dir_all(&folder)?;
    Ok(folder)
}

fn identifier(serial: &str, canvas: i64) -> String {
    format!("{serial}-{canvas}")
}

fn write(config: &Config) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let path = domains_folder()?.join(format!("{}.json", identifier(config.serial, config.canvas)));
    // Only this user may read the port's token, from the moment the file exists.
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)?;
    file.write_all(&serde_json::to_vec(config)?)?;
    Ok(())
}

fn forget(id: &str) {
    if let Ok(folder) = domains_folder() {
        let _ = std::fs::remove_file(folder.join(format!("{id}.json")));
    }
}

fn domain(id: &str, name: &str) -> Retained<NSFileProviderDomain> {
    // SAFETY: a plain initialiser on a fresh allocation, with valid strings.
    unsafe {
        NSFileProviderDomain::initWithIdentifier_displayName(
            NSFileProviderDomain::alloc(),
            &NSString::from_str(id),
            &NSString::from_str(name),
        )
    }
}

/// The locations there are, as (identifier, name).
fn domains() -> Vec<(String, String)> {
    wait(ANSWER_SECONDS, |done| {
        let block = RcBlock::new(
            move |found: NonNull<NSArray<NSFileProviderDomain>>, _error: *mut NSError| {
                // SAFETY: macOS hands a valid array for the length of the call.
                let found = unsafe { found.as_ref() };
                let list = found
                    .iter()
                    .map(|item| {
                        // SAFETY: plain getters on a valid domain.
                        unsafe {
                            (
                                item.identifier().to_string(),
                                item.displayName().to_string(),
                            )
                        }
                    })
                    .collect::<Vec<_>>();
                let _ = done.send(list);
            },
        );
        // SAFETY: the block lives as long as macOS needs it, which RcBlock arranges.
        unsafe { NSFileProviderManager::getDomainsWithCompletionHandler(&block) };
    })
    .unwrap_or_default()
}

/// Tells Finder to read a location again.
fn signal(id: &str, name: &str) {
    let found = domain(id, name);
    // SAFETY: a valid domain; the manager may be missing for one macOS does not know.
    let Some(manager) = (unsafe { NSFileProviderManager::managerForDomain(&found) }) else {
        return;
    };
    let _ = wait(SIGNAL_SECONDS, |done| {
        let block = RcBlock::new(move |_error: *mut NSError| {
            let _ = done.send(());
        });
        // SAFETY: a valid manager and identifier, and a block RcBlock keeps alive.
        unsafe {
            manager.signalEnumeratorForContainerItemIdentifier_completionHandler(
                NSFileProviderWorkingSetContainerItemIdentifier,
                &block,
            )
        };
    });
}

fn add(id: &str, name: &str) -> Result<()> {
    let found = domain(id, name);
    let answer = wait(ANSWER_SECONDS, |done| {
        let block = RcBlock::new(move |error: *mut NSError| {
            let _ = done.send(error_text(error));
        });
        // SAFETY: a valid domain, and a block RcBlock keeps alive.
        unsafe { NSFileProviderManager::addDomain_completionHandler(&found, &block) };
    });
    match answer {
        None => bail!("macOS did not answer when adding the Finder location."),
        Some(Some(error)) => bail!("macOS would not add the Finder location: {error}"),
        Some(None) => Ok(()),
    }
}

fn remove(id: &str, name: &str) -> Result<()> {
    let found = domain(id, name);
    let answer = wait(ANSWER_SECONDS, |done| {
        let block = RcBlock::new(move |error: *mut NSError| {
            let _ = done.send(error_text(error));
        });
        // SAFETY: a valid domain, and a block RcBlock keeps alive.
        unsafe { NSFileProviderManager::removeDomain_completionHandler(&found, &block) };
    });
    match answer {
        Some(Some(error)) => bail!("macOS would not remove the Finder location: {error}"),
        _ => Ok(()),
    }
}

/// The folder Finder shows for a location. It can take a moment to appear after adding.
fn visible_folder(id: &str, name: &str) -> Option<String> {
    let found = domain(id, name);
    // SAFETY: a valid domain.
    let manager = unsafe { NSFileProviderManager::managerForDomain(&found) }?;
    for _ in 0..FOLDER_TRIES {
        let path = wait(SIGNAL_SECONDS, |done| {
            let block = RcBlock::new(move |url: *mut NSURL, _error: *mut NSError| {
                // SAFETY: a non-null URL from the handler is valid for the call.
                let path = unsafe { url.as_ref() }
                    .and_then(|url| url.path())
                    .map(|path| path.to_string());
                let _ = done.send(path);
            });
            // SAFETY: a valid manager and identifier, and a block RcBlock keeps alive.
            unsafe {
                manager.getUserVisibleURLForItemIdentifier_completionHandler(
                    NSFileProviderRootContainerItemIdentifier,
                    &block,
                )
            };
        })
        .flatten();
        if path.is_some() {
            return path;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    None
}

/// Runs one command, as the separate helper app used to.
pub fn run(command: &str, request: &Value) -> Result<Value> {
    let request: Request = serde_json::from_value(request.clone()).unwrap_or_default();
    match command {
        "show" => {
            let (Some(base), Some(token), Some(serial), Some(canvas), Some(name)) = (
                request.base.as_deref(),
                request.token.as_deref(),
                request.serial.as_deref(),
                request.canvas,
                request.name.as_deref(),
            ) else {
                bail!("show needs base, token, serial, canvas and name.");
            };
            let revision = request.revision.as_deref().unwrap_or("0");
            write(&Config {
                base,
                token,
                serial,
                canvas,
                revision,
            })?;
            let id = identifier(serial, canvas);
            add(&id, name)?;
            signal(&id, name);
            Ok(match visible_folder(&id, name) {
                Some(folder) => json!({"shown": true, "folder": folder}),
                None => {
                    json!({"shown": true, "folder": null, "note": "Turn on Captain Salvage in System Settings, General, Login Items & Extensions, File Providers."})
                }
            })
        }
        "hide" => {
            let (Some(serial), Some(canvas)) = (request.serial.as_deref(), request.canvas) else {
                bail!("hide needs serial and canvas.");
            };
            let id = identifier(serial, canvas);
            if let Some((_, name)) = domains().into_iter().find(|(found, _)| *found == id) {
                remove(&id, &name)?;
            }
            forget(&id);
            Ok(json!({"hidden": true}))
        }
        "sync" => {
            let (Some(base), Some(token), Some(serial)) = (
                request.base.as_deref(),
                request.token.as_deref(),
                request.serial.as_deref(),
            ) else {
                bail!("sync needs base, token and serial.");
            };
            let canvases = request.canvases.unwrap_or_default();
            let mut shown = Vec::new();
            for (id, name) in domains() {
                let Some((owner, canvas)) = id.split_once('-') else {
                    continue;
                };
                let Ok(canvas) = canvas.parse::<i64>() else {
                    continue;
                };
                if owner != serial {
                    // Another disk's canvas: unreachable until that disk is open again.
                    forget(&id);
                    signal(&id, &name);
                } else if let Some(found) = canvases.iter().find(|item| item.id == canvas) {
                    write(&Config {
                        base,
                        token,
                        serial,
                        canvas,
                        revision: &found.revision,
                    })?;
                    signal(&id, &name);
                    shown.push(canvas);
                } else {
                    let _ = remove(&id, &name);
                    forget(&id);
                }
            }
            Ok(json!({"shown": shown}))
        }
        "offline" => {
            for (id, name) in domains() {
                forget(&id);
                signal(&id, &name);
            }
            Ok(json!({"offline": true}))
        }
        _ => Ok(
            json!({"domains": domains().into_iter().map(|(id, name)| json!({"id": id, "name": name})).collect::<Vec<_>>()}),
        ),
    }
}
