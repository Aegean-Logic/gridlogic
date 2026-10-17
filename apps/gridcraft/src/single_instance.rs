//! One GridLogic per user on Windows and Linux: a file double-clicked, or a `gridlogic://` link
//! followed (Open in Desktop App), while GridLogic is running opens in that window, as Excel's
//! do. (macOS does this itself: Finder sends the running app an Apple event.)
//!
//! The first GridLogic listens on a random localhost port and writes the port and a random token
//! to `instance.json` in the user's settings folder (readable by that user only). A later one
//! reads it, sends its arguments with the token, and exits once the first says `ok`. A stale file
//! (GridLogic crashed, or the port went to another program) gets no `ok`, so the new GridLogic
//! starts as usual and takes over the file.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// The most a message from another GridLogic may carry.
const MAX_MESSAGE: u64 = 1 << 20;
const MAX_ARGS: usize = 64;
const MAX_ARG: usize = 32 * 1024;

#[derive(Serialize, Deserialize)]
struct Record {
    port: u16,
    token: String,
    pid: u32,
}

#[derive(Serialize, Deserialize)]
struct Message {
    token: String,
    /// Absolute paths and `gridlogic://` links; none: a new workbook.
    args: Vec<String>,
}

/// What was asked of the running GridLogic, for its next frame.
#[derive(Default)]
struct Shared {
    asked: Vec<Vec<String>>,
    ctx: Option<egui::Context>,
}

/// This GridLogic is the one running: it hears the others.
pub struct Primary {
    shared: Arc<Mutex<Shared>>,
    file: PathBuf,
    token: String,
}

pub enum Claim {
    /// This is the GridLogic that runs (and hears later ones, when it could start listening).
    Primary(Option<Primary>),
    /// The running GridLogic took the arguments: this one can exit.
    Forwarded,
}

/// Hands `args` to a running GridLogic, or becomes the one that runs.
pub fn claim(dir: &Path, args: &[String]) -> Claim {
    let file = dir.join("instance.json");
    if let Some(record) = std::fs::read(&file).ok().and_then(|b| serde_json::from_slice::<Record>(&b).ok())
        && forward(&record, args).is_ok()
    {
        return Claim::Forwarded;
    }
    match listen(dir, &file) {
        Ok(p) => Claim::Primary(Some(p)),
        Err(e) => {
            log::warn!("single instance: {e}");
            Claim::Primary(None)
        }
    }
}

/// Sends the arguments to the running GridLogic; `Ok` once it answers `ok`.
fn forward(record: &Record, args: &[String]) -> std::io::Result<()> {
    let addr = SocketAddr::from(([127, 0, 0, 1], record.port));
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(1))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let message = serde_json::to_string(&Message { token: record.token.clone(), args: args.to_vec() })?;
    writeln!(stream, "{message}")?;
    let mut answer = String::new();
    BufReader::new(stream.take(16)).read_line(&mut answer)?;
    if answer.trim() == "ok" { Ok(()) } else { Err(std::io::Error::other("not a GridLogic that answered")) }
}

/// A token no other program can guess: two keys from the system's random source (each
/// `RandomState` is seeded from it).
fn token() -> String {
    let mut out = String::new();
    for i in 0..2u32 {
        let mut h = RandomState::new().build_hasher();
        h.write_u32(std::process::id());
        h.write_u32(i);
        out.push_str(&format!("{:016x}", h.finish()));
    }
    out
}

fn listen(dir: &Path, file: &Path) -> std::io::Result<Primary> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    let token = token();
    std::fs::create_dir_all(dir)?;
    let record = serde_json::to_vec(&Record { port, token: token.clone(), pid: std::process::id() })?;
    write_private(file, &record)?;
    let shared: Arc<Mutex<Shared>> = Arc::default();
    let (hear, expected) = (shared.clone(), token.clone());
    std::thread::Builder::new().name("single instance".into()).spawn(move || {
        for stream in listener.incoming().flatten() {
            if let Some(args) = hear_one(stream, &expected) {
                let mut s = hear.lock().unwrap_or_else(|e| e.into_inner());
                s.asked.push(args);
                if let Some(ctx) = &s.ctx {
                    ctx.request_repaint();
                }
            }
        }
    })?;
    Ok(Primary { shared, file: file.to_path_buf(), token })
}

/// Writes the record readable by this user only.
fn write_private(file: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(file)?.write_all(bytes)
}

/// One message from another GridLogic: its arguments, if it knew the token.
fn hear_one(stream: TcpStream, token: &str) -> Option<Vec<String>> {
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let mut reply = stream.try_clone().ok()?;
    let mut line = String::new();
    BufReader::new(stream.take(MAX_MESSAGE)).read_line(&mut line).ok()?;
    let message: Message = serde_json::from_str(&line).ok()?;
    if message.token != token || message.args.len() > MAX_ARGS || message.args.iter().any(|a| a.len() > MAX_ARG) {
        return None;
    }
    writeln!(reply, "ok").ok()?;
    Some(message.args)
}

impl Primary {
    /// Lets messages wake the window (once it exists).
    pub fn attach(&self, ctx: &egui::Context) {
        self.shared.lock().unwrap_or_else(|e| e.into_inner()).ctx = Some(ctx.clone());
    }

    /// What later GridLogics asked for since the last frame, one list of arguments each.
    pub fn take(&self) -> Vec<Vec<String>> {
        std::mem::take(&mut self.shared.lock().unwrap_or_else(|e| e.into_inner()).asked)
    }
}

impl Drop for Primary {
    /// Removes the record, unless a newer GridLogic has written its own since.
    fn drop(&mut self) {
        let ours = std::fs::read(&self.file).ok().and_then(|b| serde_json::from_slice::<Record>(&b).ok()).is_some_and(|r| r.token == self.token);
        if ours {
            let _ = std::fs::remove_file(&self.file);
        }
    }
}

/// A file named on the command line, as the running GridLogic (with another working folder)
/// can open it.
pub fn absolute(arg: &str) -> String {
    std::path::absolute(arg).map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| arg.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("gridlogic-instance-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn a_second_launch_hands_its_files_to_the_first() {
        let d = dir("handover");
        let Claim::Primary(Some(first)) = claim(&d, &[]) else { panic!("the first launch runs") };
        let args = vec![absolute("Budget.xlsx"), "gridlogic://open?server=x&path=y".to_string()];
        assert!(matches!(claim(&d, &args), Claim::Forwarded), "the second hands over");
        assert_eq!(first.take(), vec![args]);
        assert!(matches!(claim(&d, &[]), Claim::Forwarded), "a plain launch hands over too (a new workbook)");
        assert_eq!(first.take(), vec![Vec::<String>::new()]);
        drop(first);
        assert!(!d.join("instance.json").exists(), "the record goes with the GridLogic that wrote it");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_stale_record_or_wrong_token_doesnt_hand_over() {
        let d = dir("stale");
        std::fs::create_dir_all(&d).unwrap();
        // A port nothing listens on any more.
        let port = TcpListener::bind(("127.0.0.1", 0)).unwrap().local_addr().unwrap().port();
        std::fs::write(d.join("instance.json"), serde_json::to_vec(&Record { port, token: "old".into(), pid: 1 }).unwrap()).unwrap();
        let Claim::Primary(Some(first)) = claim(&d, &[]) else { panic!("starts as usual") };
        // Someone who doesn't know the token gets nothing in.
        let record: Record = serde_json::from_slice(&std::fs::read(d.join("instance.json")).unwrap()).unwrap();
        let wrong = Record { port: record.port, token: "guess".into(), pid: 1 };
        assert!(forward(&wrong, &["C:/evil.xlsx".to_string()]).is_err());
        assert!(first.take().is_empty());
        drop(first);
        let _ = std::fs::remove_dir_all(&d);
    }
}
