// The shared folders' file system (drives.rs): what IronRDP's drive backend asks with the server's paths. Ported from
// tabby-rdp's test/unit/drives.ts.
use super::*;

struct Sandbox {
    _root: tempfile::TempDir,
    dir: PathBuf,
    other: PathBuf,
}

fn sandbox() -> Sandbox {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("share");
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    std::fs::write(dir.join("hello.txt"), "hello").unwrap();
    std::fs::write(dir.join("sub/deep.txt"), "deep").unwrap();
    std::fs::write(root.path().join("secret.txt"), "outside").unwrap();
    Sandbox { other: root.path().to_owned(), dir, _root: root }
}

fn folder(dir: &Path, read_only: bool) -> FolderSpec {
    FolderSpec { id: 1, path: dir.to_string_lossy().into_owned(), name: "Share".into(), read_only }
}

fn mode(write: bool, create: bool, exclusive: bool, truncate: bool) -> OpenMode { OpenMode { write, create, exclusive, truncate } }
const READ: OpenMode = OpenMode { write: false, create: false, exclusive: false, truncate: false };

fn code<T: std::fmt::Debug>(result: Result<T, DriveError>) -> &'static str {
    match result {
        Ok(_) => "ok",
        Err(e) => e.code,
    }
}

#[test]
fn paths_are_the_drives_and_nothing_outside_the_folder() {
    let s = sandbox();
    let f = folder(&s.dir, false);
    assert!(stat(&f, "\\").unwrap().unwrap().dir);
    assert_eq!(stat(&f, "\\hello.txt").unwrap().unwrap().size, 5);
    assert_eq!(stat(&f, "sub/deep.txt").unwrap().unwrap().size, 4);
    assert!(stat(&f, "\\missing.txt").unwrap().is_none());
    let mut names = list(&f, "\\").unwrap();
    names.sort();
    assert_eq!(names, ["hello.txt", "sub"]);
    // The folder above holds secret.txt: not reachable, however the path is spelled.
    for escape in ["\\..\\secret.txt", "..\\secret.txt", "\\sub\\..\\..\\secret.txt", "/../secret.txt", "\\sub\\..\\hello.txt"] {
        assert!(stat(&f, escape).unwrap().is_none(), "{escape}");
        assert_eq!(code(open(&f, escape, READ)), "ENOENT", "{escape}");
    }
}

#[test]
fn files_read_and_write_at_offsets() {
    let s = sandbox();
    let f = folder(&s.dir, false);
    let file = open(&f, "\\hello.txt", READ).unwrap();
    assert_eq!(read(&file, 0, 100).unwrap(), b"hello");
    assert_eq!(read(&file, 1, 2).unwrap(), b"el");
    assert!(read(&file, 5, 10).unwrap().is_empty());
    assert_eq!(code(write(&file, 0, b"x")), "EBADF"); // opened for reading
    let file = open(&f, "\\new.txt", mode(true, true, true, false)).unwrap();
    assert_eq!(write(&file, 0, b"abcdef").unwrap(), 6);
    assert_eq!(write(&file, 2, b"XY").unwrap(), 2);
    file.set_len(5).unwrap();
    drop(file);
    assert_eq!(std::fs::read_to_string(s.dir.join("new.txt")).unwrap(), "abXYe");
    assert_eq!(code(open(&f, "\\new.txt", mode(true, true, true, false))), "EEXIST");
    // Reads are capped at what one answer can carry.
    std::fs::write(s.dir.join("big.bin"), vec![7u8; READ_LIMIT + 10]).unwrap();
    let big = open(&f, "\\big.bin", READ).unwrap();
    assert_eq!(read(&big, 0, usize::MAX).unwrap().len(), READ_LIMIT);
}

#[test]
fn folders_are_made_renamed_and_removed() {
    let s = sandbox();
    let f = folder(&s.dir, false);
    mkdir(&f, "\\made").unwrap();
    assert!(stat(&f, "\\made").unwrap().unwrap().dir);
    assert_eq!(code(mkdir(&f, "\\made")), "EEXIST");
    assert_eq!(code(remove(&f, "\\sub")), "ENOTEMPTY");
    rename(&f, "\\hello.txt", "\\made\\moved.txt", false).unwrap();
    assert_eq!(std::fs::read_to_string(s.dir.join("made/moved.txt")).unwrap(), "hello");
    assert_eq!(code(rename(&f, "\\sub\\deep.txt", "\\made\\moved.txt", false)), "EEXIST");
    rename(&f, "\\sub\\deep.txt", "\\made\\moved.txt", true).unwrap();
    assert_eq!(std::fs::read_to_string(s.dir.join("made/moved.txt")).unwrap(), "deep");
    // The same file under another spelling (case-insensitive file systems see it as existing).
    rename(&f, "\\made\\moved.txt", "\\made\\MOVED.txt", false).unwrap();
    assert!(std::fs::read_dir(s.dir.join("made")).unwrap().any(|e| e.unwrap().file_name() == "MOVED.txt"));
    remove(&f, "\\made\\MOVED.txt").unwrap();
    remove(&f, "\\made").unwrap();
    remove(&f, "\\sub").unwrap();
    assert!(list(&f, "\\").unwrap().is_empty());
    assert_eq!(code(remove(&f, "\\")), "EACCES"); // the shared folder itself
    assert!(s.dir.exists());
}

#[test]
fn a_read_only_folder_refuses_every_change() {
    let s = sandbox();
    let f = folder(&s.dir, true);
    assert!(stat(&f, "\\hello.txt").unwrap().unwrap().readonly);
    let file = open(&f, "\\hello.txt", READ).unwrap();
    assert_eq!(read(&file, 0, 5).unwrap(), b"hello");
    assert_eq!(code(open(&f, "\\hello.txt", mode(true, true, false, false))), "EACCES");
    assert_eq!(code(open(&f, "\\new.txt", mode(false, true, false, false))), "EACCES");
    assert_eq!(code(open(&f, "\\hello.txt", mode(false, false, false, true))), "EACCES");
    assert_eq!(code(mkdir(&f, "\\made")), "EACCES");
    assert_eq!(code(remove(&f, "\\hello.txt")), "EACCES");
    assert_eq!(code(rename(&f, "\\hello.txt", "\\bye.txt", false)), "EACCES");
    assert_eq!(code(writable(&f, "can't write")), "EACCES");
    assert_eq!(std::fs::read_to_string(s.dir.join("hello.txt")).unwrap(), "hello");
}

#[test]
fn the_volume_is_the_folders_labelled_with_the_drives_name() {
    let s = sandbox();
    let v = volume(&folder(&s.dir, false)).unwrap();
    assert_eq!(v.label, "Share");
    #[cfg(unix)]
    assert!(v.total > 0.0 && v.free > 0.0 && v.free <= v.total);
}

#[cfg(unix)]
#[test]
fn links_inside_work_and_out_of_it_dont_exist() {
    use std::os::unix::fs::symlink;
    let s = sandbox();
    std::fs::create_dir(s.other.join("elsewhere")).unwrap();
    symlink(s.other.join("secret.txt"), s.dir.join("link-out.txt")).unwrap();
    symlink(s.other.join("elsewhere"), s.dir.join("dir-out")).unwrap();
    symlink(s.other.join("nothing-here"), s.dir.join("dangling")).unwrap();
    symlink(s.dir.join("hello.txt"), s.dir.join("link-in.txt")).unwrap();
    symlink(s.dir.join("sub"), s.dir.join("dir-in")).unwrap();
    let f = folder(&s.dir, false);
    // A link that stays inside is the file it points to.
    assert_eq!(stat(&f, "\\link-in.txt").unwrap().unwrap().size, 5);
    assert_eq!(stat(&f, "\\dir-in\\deep.txt").unwrap().unwrap().size, 4);
    assert_eq!(read(&open(&f, "\\link-in.txt", READ).unwrap(), 0, 10).unwrap(), b"hello");
    // One that leads out isn't there: not to look at, read, overwrite, or make things through.
    assert!(stat(&f, "\\link-out.txt").unwrap().is_none());
    assert!(stat(&f, "\\dir-out").unwrap().is_none());
    assert!(stat(&f, "\\dir-out\\anything").unwrap().is_none());
    assert_eq!(code(open(&f, "\\link-out.txt", READ)), "ENOENT");
    assert_eq!(code(open(&f, "\\link-out.txt", mode(true, true, false, true))), "ENOENT");
    assert_eq!(code(open(&f, "\\dir-out\\new.txt", mode(true, true, true, false))), "ENOENT");
    assert_eq!(code(open(&f, "\\dangling", mode(true, true, false, false))), "ENOENT");
    assert_eq!(code(mkdir(&f, "\\dir-out\\made")), "ENOENT");
    assert_eq!(code(list(&f, "\\dir-out")), "ENOENT");
    assert_eq!(code(rename(&f, "\\hello.txt", "\\dir-out\\moved.txt", true)), "ENOENT");
    assert_eq!(std::fs::read_to_string(s.other.join("secret.txt")).unwrap(), "outside");
    assert!(std::fs::read_dir(s.other.join("elsewhere")).unwrap().next().is_none());
    assert!(!s.other.join("nothing-here").exists());
    // Removing a link removes the link, whatever it points to.
    remove(&f, "\\link-out.txt").unwrap();
    remove(&f, "\\link-in.txt").unwrap();
    assert!(!s.dir.join("link-in.txt").exists());
    assert_eq!(std::fs::read_to_string(s.dir.join("hello.txt")).unwrap(), "hello");
    assert_eq!(std::fs::read_to_string(s.other.join("secret.txt")).unwrap(), "outside");
}

#[cfg(unix)]
#[test]
fn a_shared_folder_reached_through_a_link_works() {
    let s = sandbox();
    std::os::unix::fs::symlink(&s.dir, s.other.join("alias")).unwrap();
    let f = folder(&s.other.join("alias"), false);
    assert_eq!(stat(&f, "\\hello.txt").unwrap().unwrap().size, 5);
    assert_eq!(list(&f, "\\sub").unwrap(), ["deep.txt"]);
}

#[cfg(unix)]
#[test]
fn pipes_and_other_special_files_are_not_served() {
    let s = sandbox();
    let pipe = std::ffi::CString::new(s.dir.join("pipe").to_string_lossy().as_bytes()).unwrap();
    // SAFETY: a zero-terminated path.
    assert_eq!(unsafe { libc::mkfifo(pipe.as_ptr(), 0o600) }, 0);
    let f = folder(&s.dir, false);
    assert!(stat(&f, "\\pipe").unwrap().is_none());
    // Opening one would wait forever for a writer: refused before, and O_NONBLOCK if it is swapped in meanwhile.
    assert_eq!(code(open(&f, "\\pipe", READ)), "EACCES");
    assert_eq!(code(open(&f, "\\pipe", mode(true, false, false, false))), "EACCES");
}

#[test]
fn handles_belong_to_their_session_and_drive() {
    let s = sandbox();
    let spec = folder(&s.dir, false);
    let session = DriveSession {
        owner: "main".into(),
        drives: HashMap::from([(1, Arc::new(Drive { spec: spec.clone(), lock: tokio::sync::Mutex::new(()) }))]),
        files: Mutex::default(),
        next_handle: AtomicU32::new(0),
    };
    session.files.lock().unwrap().insert(7, OpenFile { file: Arc::new(open(&spec, "\\hello.txt", READ).unwrap()), device: 1 });
    assert!(session.file(7).is_ok());
    assert_eq!(session.file(8).err().unwrap().code, "EBADF");
    assert_eq!(session.drive(2).err().unwrap().code, "ENOENT");
    let table = Drives::default();
    table.sessions.lock().unwrap().insert(1, Arc::new(session));
    table.reset("other");
    assert!(table.session(1).is_ok());
    table.reset("main");
    assert_eq!(table.session(1).err().unwrap().code, "EBADF");
}

#[tokio::test]
async fn one_request_at_a_time_per_drive() {
    let s = sandbox();
    let drive = Arc::new(Drive { spec: folder(&s.dir, false), lock: tokio::sync::Mutex::new(()) });
    let running = Arc::new(AtomicU32::new(0));
    let most = Arc::new(AtomicU32::new(0));
    let tasks: Vec<_> = (0..8).map(|_| {
        let (drive, running, most) = (drive.clone(), running.clone(), most.clone());
        tokio::spawn(async move {
            on_drive(&drive, move || {
                let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                most.fetch_max(now, Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(5));
                running.fetch_sub(1, Ordering::SeqCst);
                Ok(())
            }).await
        })
    }).collect();
    for task in tasks {
        task.await.unwrap().unwrap();
    }
    assert_eq!(most.load(Ordering::SeqCst), 1);
}
